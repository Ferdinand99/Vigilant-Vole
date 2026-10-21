//! gRPC server reflection client: asks the server for the file that defines
//! a service (and the files that one imports), so a monitor works without
//! a pasted `.proto`. Speaks `grpc.reflection.v1` and falls back to
//! `v1alpha` (same messages) for servers that only have the older one.
//!
//! Each request is its own call on the monitor's connection: the stream is
//! bidirectional, but one request and one answer per stream is enough.

use std::collections::HashMap;

use http::HeaderMap;
use prost::Message;
use prost_reflect::DescriptorPool;
use prost_types::FileDescriptorProto;

use crate::monitor::checkers::grpc::{CallError, Channel};
use crate::monitor::checkers::util;

pub const V1: &str = "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo";
pub const V1ALPHA: &str = "/grpc.reflection.v1alpha.ServerReflection/ServerReflectionInfo";
/// Most files fetched for one service (its file plus everything it imports).
const MAX_FILES: usize = 64;

#[derive(Clone, PartialEq, Message)]
pub struct ReflectionRequest {
    #[prost(string, tag = "1")]
    pub host: String,
    #[prost(oneof = "Request", tags = "3, 4, 7")]
    pub request: Option<Request>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum Request {
    #[prost(string, tag = "3")]
    FileByFilename(String),
    #[prost(string, tag = "4")]
    FileContainingSymbol(String),
    #[prost(string, tag = "7")]
    ListServices(String),
}

#[derive(Clone, PartialEq, Message)]
pub struct ReflectionResponse {
    #[prost(oneof = "Response", tags = "4, 6, 7")]
    pub response: Option<Response>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub enum Response {
    #[prost(message, tag = "4")]
    Files(FileDescriptorResponse),
    #[prost(message, tag = "6")]
    Services(ListServiceResponse),
    #[prost(message, tag = "7")]
    Error(ErrorResponse),
}

#[derive(Clone, PartialEq, Message)]
pub struct FileDescriptorResponse {
    #[prost(bytes = "vec", repeated, tag = "1")]
    pub file_descriptor_proto: Vec<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ListServiceResponse {
    #[prost(message, repeated, tag = "1")]
    pub service: Vec<ServiceResponse>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServiceResponse {
    #[prost(string, tag = "1")]
    pub name: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ErrorResponse {
    #[prost(int32, tag = "1")]
    pub error_code: i32,
    #[prost(string, tag = "2")]
    pub error_message: String,
}

/// A reflection session on one connection; remembers which version works.
struct Session<'a> {
    channel: &'a mut Channel,
    metadata: &'a HeaderMap,
    path: &'static str,
}

impl Session<'_> {
    async fn ask(&mut self, request: Request) -> Result<Response, String> {
        let message = ReflectionRequest {
            host: String::new(),
            request: Some(request),
        }
        .encode_to_vec();
        let answer = match self.channel.unary(self.path, self.metadata, &message).await {
            // UNIMPLEMENTED on v1: try the older name once.
            Err(CallError::Status { code: 12, .. }) if self.path == V1 => {
                self.path = V1ALPHA;
                self.channel.unary(self.path, self.metadata, &message).await
            }
            other => other,
        };
        let answer = answer.map_err(|e| match e {
            CallError::Status { code: 12, .. } => {
                "The server doesn't offer server reflection; paste the service's .proto into the monitor".to_string()
            }
            CallError::Status { .. } => format!("Server reflection failed: {}", e.describe()),
            CallError::Failed(msg) => msg,
        })?;
        let response = ReflectionResponse::decode(answer.as_slice())
            .map_err(|_| "The server's reflection answer isn't readable".to_string())?;
        response
            .response
            .ok_or_else(|| "The server's reflection answer is empty".into())
    }

    /// The files of a file request; `what` names it in a NOT_FOUND message.
    async fn files(
        &mut self,
        request: Request,
        what: &str,
    ) -> Result<Vec<FileDescriptorProto>, String> {
        match self.ask(request).await? {
            Response::Error(err) if err.error_code == 5 => {
                Err(format!("The server doesn't know {what}"))
            }
            Response::Error(err) => Err(format!(
                "Server reflection failed: {}",
                util::truncate(&err.error_message, 120)
            )),
            Response::Files(files) => files
                .file_descriptor_proto
                .iter()
                .map(|bytes| {
                    FileDescriptorProto::decode(bytes.as_slice())
                        .map_err(|_| "The server sent an unreadable file descriptor".to_string())
                })
                .collect(),
            _ => Err("The server's reflection answer has no files".into()),
        }
    }
}

/// The full name of `service`: as given when it has a package, otherwise
/// looked up in the server's service list (Uptime Kuma accepts short names).
async fn resolve_service(session: &mut Session<'_>, service: &str) -> Result<String, String> {
    if service.contains('.') {
        return Ok(service.to_string());
    }
    let list = match session.ask(Request::ListServices(String::new())).await? {
        Response::Services(list) => list,
        Response::Error(err) => {
            return Err(format!(
                "Server reflection failed: {}",
                util::truncate(&err.error_message, 120)
            ));
        }
        Response::Files(_) => {
            return Err("The server's reflection answer has no service list".into());
        }
    };
    let suffix = format!(".{service}");
    let found: Vec<&str> = list
        .service
        .iter()
        .map(|s| s.name.as_str())
        .filter(|n| *n == service || n.ends_with(&suffix))
        .collect();
    match found.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => {
            let all: Vec<&str> = list
                .service
                .iter()
                .map(|s| s.name.as_str())
                .take(5)
                .collect();
            Err(format!(
                "The server has no service \"{}\" (it lists: {})",
                util::truncate(service, 80),
                if all.is_empty() {
                    "none".into()
                } else {
                    all.join(", ")
                }
            ))
        }
        many => Err(format!(
            "\"{service}\" is ambiguous on this server ({}): use the full service name",
            many.join(", ")
        )),
    }
}

fn add(list: Vec<FileDescriptorProto>, files: &mut HashMap<String, FileDescriptorProto>) {
    for f in list {
        files.entry(f.name().to_string()).or_insert(f);
    }
}

/// Fetches the descriptors for `service` from the server and builds a pool.
/// Imports the server leaves out are asked for by file name; the
/// well-known types (`google/protobuf/*.proto`) are built in.
pub(crate) async fn fetch(
    channel: &mut Channel,
    metadata: &HeaderMap,
    service: &str,
) -> Result<DescriptorPool, String> {
    let mut session = Session {
        channel,
        metadata,
        path: V1,
    };
    let full = resolve_service(&mut session, service).await?;
    let what = format!("the service \"{}\"", util::truncate(&full, 80));
    let first = session
        .files(Request::FileContainingSymbol(full.clone()), &what)
        .await?;
    let mut pool = DescriptorPool::global();
    let mut files: HashMap<String, FileDescriptorProto> = HashMap::new();
    add(first, &mut files);
    loop {
        let missing: Vec<(String, String)> = files
            .values()
            .flat_map(|f| {
                f.dependency
                    .iter()
                    .map(move |d| (d.clone(), f.name().to_string()))
            })
            .filter(|(d, _)| !files.contains_key(d) && pool.get_file_by_name(d).is_none())
            .collect();
        let Some((name, needed_by)) = missing.into_iter().next() else {
            break;
        };
        if files.len() >= MAX_FILES {
            return Err(format!(
                "The service needs more than {MAX_FILES} .proto files"
            ));
        }
        let got = session
            .files(Request::FileByFilename(name.clone()), &name)
            .await
            .map_err(|e| format!("The server didn't send {name} (imported by {needed_by}): {e}"))?;
        if !got.iter().any(|f| f.name() == name) {
            return Err(format!(
                "The server didn't send {name} (imported by {needed_by})"
            ));
        }
        add(got, &mut files);
    }
    pool.add_file_descriptor_protos(files.into_values())
        .map_err(|e| format!("The server's descriptors don't fit together: {e}"))?;
    Ok(pool)
}
