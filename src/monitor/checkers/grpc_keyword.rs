//! gRPC keyword checker: `target` = `grpc[s]://host[:port]/package.Service/Method`.
//!
//! Calls one unary method with a JSON request body (`http_body`) and
//! metadata (`http_headers`), renders the answer as JSON (camelCase keys,
//! enum names, 64-bit integers as strings, like Uptime Kuma) and looks for
//! `keyword` in it (`keyword_invert` flips that; no keyword = any OK answer
//! is up). A non-OK gRPC status is down.
//!
//! The message types come from the monitor's `.proto` text (`grpc_proto`)
//! or, when that is empty, from the server itself via server reflection.
//! Compiled descriptors are cached per monitor ([`schema::cache`]).
//! Response time = connect + call (reflection not included).

use std::time::{Duration, Instant};

use http::HeaderMap;
use prost_reflect::DescriptorPool;

use super::conn;
use super::endpoint::{self, Endpoint};
use super::grpc::Channel;
use super::util;
use crate::db::models::{CheckOutcome, Monitor, MonitorType};

pub mod reflection;
pub mod schema;

#[cfg(test)]
mod tests;

/// Characters of the answer shown in messages.
const SNIPPET_CHARS: usize = 120;

/// A parsed target: where to connect and which method to call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub endpoint: Endpoint,
    /// As written: full (`pkg.Service`) or short (`Service`).
    pub service: String,
    pub method: String,
}

/// `grpc[s]://host[:port]/package.Service/Method`.
pub fn parse_target(target: &str) -> Result<Target, String> {
    let endpoint = endpoint::parse(MonitorType::GrpcKeyword, target)?;
    let path = endpoint.path.trim_matches('/');
    let Some((service, method)) = path.rsplit_once('/') else {
        return Err("Add the service and method to the address: grpc://host:port/package.Service/Method".into());
    };
    let service = service.trim_matches('/');
    if service.is_empty() || method.is_empty() || service.contains('/') {
        return Err("The path must be /package.Service/Method".into());
    }
    let ident = |s: &str| s.split('.').all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    if !ident(service) || !ident(method) || method.contains('.') {
        return Err(format!("\"{}\" isn't a service and method name (package.Service/Method)", util::truncate(path, 80)));
    }
    Ok(Target { service: service.to_string(), method: method.to_string(), endpoint })
}

/// The monitor's metadata (`http_headers`, a JSON object).
pub fn metadata(monitor: &Monitor) -> Result<HeaderMap, String> {
    let map = super::parse_headers(monitor.http_headers.as_deref().unwrap_or_default()).map_err(|e| format!("Metadata: {e}"))?;
    if let Some(name) = map.keys().find(|n| super::grpc::is_reserved_metadata(n.as_str())) {
        return Err(format!("Metadata: \"{name}\" is set by gRPC itself and can't be overridden"));
    }
    Ok(map)
}

/// `Ok(up message)` / `Err(down message)` for the rendered answer.
pub fn evaluate(json: &str, keyword: Option<&str>, invert: bool) -> Result<String, String> {
    let shown_answer = || if json.is_empty() || json == "{}" { "empty answer ({})".to_string() } else { util::snippet(json, SNIPPET_CHARS) };
    let Some(keyword) = keyword.filter(|k| !k.is_empty()) else {
        return Ok(format!("OK — {}", shown_answer()));
    };
    let shown = util::truncate(keyword, 40);
    match (json.contains(keyword), invert) {
        (true, false) => Ok(format!("Keyword \"{shown}\" found — {}", shown_answer())),
        (false, true) => Ok(format!("Keyword \"{shown}\" absent, as expected — {}", shown_answer())),
        (false, false) => Err(format!("Keyword \"{shown}\" not found — answer: {}", shown_answer())),
        (true, true) => Err(format!("Keyword \"{shown}\" found but should be absent — answer: {}", shown_answer())),
    }
}

/// Where the descriptors come from, for caching.
fn fingerprint(monitor: &Monitor, target: &Target) -> (u64, Option<Duration>) {
    match monitor.grpc_proto.as_deref().filter(|p| !p.trim().is_empty()) {
        Some(proto) => (schema::fingerprint(&["proto", proto]), None),
        None => {
            let ep = &target.endpoint;
            let port = ep.port.to_string();
            let tls = if ep.tls { "tls" } else { "plain" };
            (schema::fingerprint(&["reflect", tls, &ep.host.to_ascii_lowercase(), &port, &target.service]), Some(schema::REFLECTION_TTL))
        }
    }
}

pub async fn check(monitor: &Monitor) -> CheckOutcome {
    let target = match parse_target(&monitor.target) {
        Ok(t) => t,
        Err(e) => return CheckOutcome::down(e),
    };
    let metadata = match metadata(monitor) {
        Ok(m) => m,
        Err(e) => return CheckOutcome::down(e),
    };
    let timeout = super::timeout_of(monitor);
    let (fingerprint, ttl) = fingerprint(monitor, &target);
    let cached = schema::cache().get(monitor.id, fingerprint, Instant::now());
    // A pasted .proto compiles before any network traffic: a broken one is
    // the monitor's problem, not the server's.
    let compiled = match (&cached, monitor.grpc_proto.as_deref().filter(|p| !p.trim().is_empty())) {
        (None, Some(proto)) => match schema::compile_proto(proto) {
            Ok(pool) => Some(pool),
            Err(e) => return CheckOutcome::down(format!("The .proto doesn't compile: {e}")),
        },
        _ => None,
    };
    let from_reflection = ttl.is_some();
    let start = Instant::now();
    let probe = async {
        let mut channel = Channel::open(&target.endpoint, monitor.ignore_tls, timeout).await?;
        let connected = util::elapsed_ms(start);
        let pool: DescriptorPool = match cached.or(compiled) {
            Some(pool) => pool,
            None => reflection::fetch(&mut channel, &metadata, &target.service).await.map_err(CheckOutcome::down)?,
        };
        let method = schema::find_method(&pool, &target.service, &target.method).map_err(CheckOutcome::down)?;
        schema::cache().put(monitor.id, fingerprint, pool, ttl, Instant::now());
        let request = schema::encode_request(&method, monitor.http_body.as_deref().unwrap_or_default()).map_err(CheckOutcome::down)?;
        let path = format!("/{}/{}", method.parent_service().full_name(), method.name());
        let called = Instant::now();
        let answer = channel.unary(&path, &metadata, &request).await.map_err(|e| CheckOutcome::down(e.describe()))?;
        let ms = connected + util::elapsed_ms(called);
        let json = schema::decode_response(&method, &answer).map_err(CheckOutcome::down)?;
        match evaluate(&json, monitor.keyword.as_deref(), monitor.keyword_invert) {
            Ok(msg) => Ok(conn::up(ms, msg, channel.cert.as_ref(), channel.tls_version)),
            Err(msg) => Err(CheckOutcome { response_time_ms: Some(ms), ..CheckOutcome::down(msg) }),
        }
    };
    let outcome = match tokio::time::timeout(timeout, probe).await {
        Ok(Ok(outcome)) | Ok(Err(outcome)) => outcome,
        Err(_) => conn::timed_out(&format!("gRPC server {}:{}", target.endpoint.host, target.endpoint.port), timeout),
    };
    // Descriptors from reflection may be stale (a redeployed server): fetch
    // them again after a failure.
    if from_reflection && outcome.status != crate::db::models::HeartbeatStatus::Up {
        schema::cache().invalidate(monitor.id);
    }
    outcome
}

/// Drops the cached descriptors of a monitor (edited, paused or deleted).
pub fn forget(monitor_id: i64) {
    schema::cache().invalidate(monitor_id);
}
