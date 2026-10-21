//! Message types for the gRPC keyword checker: compiling the monitor's
//! `.proto` text, finding the method, JSON ⇄ protobuf, and a small cache of
//! compiled descriptors.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use miette::Diagnostic;
use prost::Message;
use prost_reflect::{DescriptorPool, DeserializeOptions, DynamicMessage, MethodDescriptor};
use protox::file::{ChainFileResolver, File, FileResolver, GoogleFileResolver};

use crate::monitor::checkers::util;

/// Name the monitor's `.proto` text compiles under (shows in messages).
pub const PROTO_FILE: &str = "monitor.proto";
/// Longest accepted `.proto` text.
pub const MAX_PROTO_LEN: usize = 256 * 1024;

/// The monitor's `.proto` as the only file; `google/protobuf/*.proto`
/// imports come from protox's built-in copies. Nothing is read from disk.
struct OneFile(String);

impl FileResolver for OneFile {
    fn open_file(&self, name: &str) -> Result<File, protox::Error> {
        if name == PROTO_FILE {
            File::from_source(name, &self.0)
        } else {
            Err(protox::Error::file_not_found(name))
        }
    }
}

/// Compiles `.proto` text (plus the well-known types it imports).
/// `Err` reads like "line 3: name 'strin' is not defined".
pub fn compile_proto(text: &str) -> Result<DescriptorPool, String> {
    if text.len() > MAX_PROTO_LEN {
        return Err(format!(
            "The .proto is larger than {} KiB",
            MAX_PROTO_LEN / 1024
        ));
    }
    let mut chain = ChainFileResolver::new();
    chain.add(OneFile(text.to_string()));
    chain.add(GoogleFileResolver::new());
    let mut compiler = protox::Compiler::with_file_resolver(chain);
    compiler.include_imports(true);
    match compiler.open_file(PROTO_FILE) {
        Ok(_) => Ok(compiler.descriptor_pool()),
        Err(err) => Err(describe_proto_error(&err, text)),
    }
}

/// The compiler's message, prefixed with the line it points at.
fn describe_proto_error(err: &protox::Error, text: &str) -> String {
    let message = err.to_string();
    let offset = err
        .labels()
        .and_then(|mut labels| labels.next())
        .map(|l| l.offset())
        .or_else(|| {
            err.related()
                .and_then(|mut rel| rel.next())
                .and_then(|d| d.labels())
                .and_then(|mut l| l.next())
                .map(|l| l.offset())
        });
    match offset.filter(|o| *o <= text.len() && err.file().is_none_or(|f| f == PROTO_FILE)) {
        Some(at) => {
            let line = text.as_bytes()[..at]
                .iter()
                .filter(|b| **b == b'\n')
                .count()
                + 1;
            format!("line {line}: {message}")
        }
        None => message,
    }
}

fn names<I: Iterator<Item = String>>(list: I) -> String {
    let all: Vec<String> = list.collect();
    let mut shown = all.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if all.len() > 5 {
        shown.push_str(", …");
    }
    if shown.is_empty() {
        "none".into()
    } else {
        shown
    }
}

/// The method `service`/`method` names. The service is its full name
/// (`pkg.Service`) or an unambiguous short one (`Service`); the method
/// matches exactly or ignoring case, so Uptime Kuma's lowerCamel names
/// (`sayHello`) work. Streaming methods are refused.
pub fn find_method(
    pool: &DescriptorPool,
    service: &str,
    method: &str,
) -> Result<MethodDescriptor, String> {
    let service = service.trim_start_matches('.');
    let svc = match pool.get_service_by_name(service) {
        Some(s) => s,
        None => {
            let suffix = format!(".{service}");
            let mut matches = pool
                .services()
                .filter(|s| s.name() == service || s.full_name().ends_with(&suffix));
            match (matches.next(), matches.next()) {
                (Some(s), None) => s,
                (Some(a), Some(b)) => {
                    return Err(format!(
                        "\"{service}\" is ambiguous ({}, {}, …): use the full service name",
                        a.full_name(),
                        b.full_name()
                    ));
                }
                _ => {
                    return Err(format!(
                        "No service \"{}\" (services: {})",
                        util::truncate(service, 80),
                        names(pool.services().map(|s| s.full_name().to_string()))
                    ));
                }
            }
        }
    };
    let found = svc.methods().find(|m| m.name() == method).or_else(|| {
        let mut loose = svc
            .methods()
            .filter(|m| m.name().eq_ignore_ascii_case(method));
        match (loose.next(), loose.next()) {
            (Some(m), None) => Some(m),
            _ => None,
        }
    });
    let Some(m) = found else {
        return Err(format!(
            "{} has no method \"{}\" (methods: {})",
            svc.full_name(),
            util::truncate(method, 80),
            names(svc.methods().map(|m| m.name().to_string()))
        ));
    };
    if m.is_client_streaming() || m.is_server_streaming() {
        return Err(format!(
            "{} is a streaming method; only unary methods can be checked",
            m.full_name()
        ));
    }
    Ok(m)
}

/// The request message from JSON (empty = all defaults). Unknown fields
/// are ignored, like Uptime Kuma does.
pub fn encode_request(m: &MethodDescriptor, json: &str) -> Result<Vec<u8>, String> {
    let json = if json.trim().is_empty() { "{}" } else { json };
    let mut de = serde_json::Deserializer::from_str(json);
    let options = DeserializeOptions::new().deny_unknown_fields(false);
    let msg = DynamicMessage::deserialize_with_options(m.input(), &mut de, &options)
        .and_then(|msg| de.end().map(|()| msg))
        .map_err(|e| {
            format!(
                "The request body doesn't fit {}: {e}",
                m.input().full_name()
            )
        })?;
    Ok(msg.encode_to_vec())
}

/// The answer as JSON: camelCase keys, enum names, 64-bit integers as
/// strings and default values left out (what Uptime Kuma matches against).
pub fn decode_response(m: &MethodDescriptor, message: &[u8]) -> Result<String, String> {
    let msg = DynamicMessage::decode(m.output(), message)
        .map_err(|e| format!("The answer isn't a {}: {e}", m.output().full_name()))?;
    serde_json::to_string(&msg).map_err(|e| format!("The answer can't be shown as JSON: {e}"))
}

// ---------------------------------------------------------------------------
// Descriptor cache
// ---------------------------------------------------------------------------

/// Monitors whose descriptors are kept.
const MAX_ENTRIES: usize = 256;
/// Descriptors fetched by server reflection are fetched again after this
/// long (the server may have been redeployed with a new schema).
pub const REFLECTION_TTL: Duration = Duration::from_secs(10 * 60);

/// What a cached pool was built from: a hash of the `.proto` text, or of
/// the reflection target (scheme, host, port, service).
pub fn fingerprint(parts: &[&str]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}

struct Entry {
    fingerprint: u64,
    pool: DescriptorPool,
    /// `None` = valid until the monitor changes.
    expires: Option<Instant>,
}

/// Compiled descriptors by monitor id. Bounded, and a monitor's entry is
/// dropped when it is edited or deleted ([`forget`]).
#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<i64, Entry>>,
}

impl Cache {
    pub fn get(&self, monitor_id: i64, fingerprint: u64, now: Instant) -> Option<DescriptorPool> {
        let map = self.entries.lock().ok()?;
        let e = map.get(&monitor_id)?;
        (e.fingerprint == fingerprint && e.expires.is_none_or(|t| now < t)).then(|| e.pool.clone())
    }

    pub fn put(
        &self,
        monitor_id: i64,
        fingerprint: u64,
        pool: DescriptorPool,
        ttl: Option<Duration>,
        now: Instant,
    ) {
        let Ok(mut map) = self.entries.lock() else {
            return;
        };
        map.retain(|_, e| e.expires.is_none_or(|t| now < t));
        if map.len() >= MAX_ENTRIES
            && !map.contains_key(&monitor_id)
            && let Some(victim) = map.keys().next().copied()
        {
            map.remove(&victim);
        }
        map.insert(
            monitor_id,
            Entry {
                fingerprint,
                pool,
                expires: ttl.map(|t| now + t),
            },
        );
    }

    pub fn invalidate(&self, monitor_id: i64) {
        if let Ok(mut map) = self.entries.lock() {
            map.remove(&monitor_id);
        }
    }

    #[cfg(test)]
    pub fn entries(&self) -> usize {
        self.entries.lock().map(|m| m.len()).unwrap_or_default()
    }
}

pub(crate) fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Cache::default)
}
