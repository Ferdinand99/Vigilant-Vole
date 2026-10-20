use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage};
use protox::file::{File, FileResolver, GoogleFileResolver};
use tokio::io::{AsyncRead, AsyncWrite};

use super::reflection::{self, ListServiceResponse, ReflectionRequest, ReflectionResponse, Request, Response, ServiceResponse};
use super::*;
use crate::db::models::HeartbeatStatus;
use crate::monitor::checkers::grpc::frame;
use crate::monitor::checkers::test_support::{monitor, tls_acceptor_alpn};

/// What the monitor pastes: one file, importing a well-known type.
const PROTO: &str = r#"syntax = "proto3";
package demo.v1;
import "google/protobuf/timestamp.proto";

service Greeter {
  rpc SayHello (HelloRequest) returns (HelloReply);
  rpc Big (HelloRequest) returns (HelloReply);
  rpc Watch (HelloRequest) returns (stream HelloReply);
}
enum Mood { MOOD_UNSPECIFIED = 0; HAPPY = 1; }
message Stats { int64 big_number = 1; }
message HelloRequest { string name = 1; }
message HelloReply {
  string message = 1;
  Mood mood = 2;
  Stats stats = 3;
  google.protobuf.Timestamp at = 4;
  int32 zero = 5;
}
"#;

/// The server's own files: the same types split over two files, so a
/// reflection client has to fetch the import.
const COMMON: &str = r#"syntax = "proto3";
package demo.v1;
enum Mood { MOOD_UNSPECIFIED = 0; HAPPY = 1; }
message Stats { int64 big_number = 1; }
"#;
const GREETER: &str = r#"syntax = "proto3";
package demo.v1;
import "demo/common.proto";
import "google/protobuf/timestamp.proto";
service Greeter {
  rpc SayHello (HelloRequest) returns (HelloReply);
  rpc Big (HelloRequest) returns (HelloReply);
  rpc Watch (HelloRequest) returns (stream HelloReply);
}
message HelloRequest { string name = 1; }
message HelloReply {
  string message = 1;
  Mood mood = 2;
  Stats stats = 3;
  google.protobuf.Timestamp at = 4;
  int32 zero = 5;
}
"#;

struct Files;

impl FileResolver for Files {
    fn open_file(&self, name: &str) -> Result<File, protox::Error> {
        match name {
            "demo/common.proto" => File::from_source(name, COMMON),
            "demo/greeter.proto" => File::from_source(name, GREETER),
            _ => GoogleFileResolver::new().open_file(name),
        }
    }
}

fn server_pool() -> DescriptorPool {
    let mut compiler = protox::Compiler::with_file_resolver(Files);
    compiler.include_imports(true);
    compiler.open_file("demo/greeter.proto").unwrap();
    compiler.descriptor_pool()
}

/// Which reflection service the test server offers.
#[derive(Clone, Copy, PartialEq)]
enum Reflect {
    V1,
    V1Alpha,
    Off,
}

struct Server {
    pool: DescriptorPool,
    reflect: Reflect,
    reflection_calls: AtomicUsize,
}

fn trailers(code: u32, message: &str) -> http::HeaderMap {
    let mut t = http::HeaderMap::new();
    t.insert("grpc-status", code.to_string().parse().unwrap());
    if !message.is_empty() {
        t.insert("grpc-message", message.parse().unwrap());
    }
    t
}

fn reply(json: &str, pool: &DescriptorPool, name: &str) -> Vec<u8> {
    let desc = pool.get_message_by_name(name).unwrap();
    DynamicMessage::deserialize(desc, &mut serde_json::Deserializer::from_str(json)).unwrap().encode_to_vec()
}

impl Server {
    /// The answer (message bytes) or an error status for one call.
    fn answer(&self, path: &str, headers: &http::HeaderMap, body: &[u8]) -> Result<Vec<u8>, (u32, String)> {
        let message = body.get(5..).unwrap_or_default();
        let header = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        match path {
            "/demo.v1.Greeter/SayHello" | "/demo.v1.Greeter/Big" => {
                match header("x-fail").as_str() {
                    "notfound" => return Err((5, "no such greeting".into())),
                    "cancelled" => return Err((1, String::new())),
                    _ => {}
                }
                let req = DynamicMessage::decode(self.pool.get_message_by_name("demo.v1.HelloRequest").unwrap(), message).unwrap();
                let name = req.get_field_by_name("name").unwrap().as_str().unwrap_or_default().to_string();
                let team = header("x-team");
                let greeting = if path.ends_with("Big") {
                    "x".repeat(70_000)
                } else if team.is_empty() {
                    format!("Hello {name}")
                } else {
                    format!("Hello {name} from {team}")
                };
                let json = format!(r#"{{"message":"{greeting}","mood":"HAPPY","stats":{{"bigNumber":"9007199254740993"}},"at":"2026-10-20T12:00:00Z"}}"#);
                Ok(reply(&json, &self.pool, "demo.v1.HelloReply"))
            }
            reflection::V1 | reflection::V1ALPHA => {
                let offered = match self.reflect {
                    Reflect::V1 => reflection::V1,
                    Reflect::V1Alpha => reflection::V1ALPHA,
                    Reflect::Off => "",
                };
                if path != offered {
                    return Err((12, String::new()));
                }
                self.reflection_calls.fetch_add(1, Ordering::SeqCst);
                let file = |name: &str| self.pool.get_file_by_name(name).unwrap().file_descriptor_proto().encode_to_vec();
                let not_found = || Response::Error(reflection::ErrorResponse { error_code: 5, error_message: "not found".into() });
                let response = match ReflectionRequest::decode(message).unwrap().request {
                    Some(Request::ListServices(_)) => Response::Services(ListServiceResponse {
                        service: ["demo.v1.Greeter", "grpc.reflection.v1.ServerReflection"].map(|n| ServiceResponse { name: n.into() }).to_vec(),
                    }),
                    // Only the file itself: the client must ask for the import.
                    Some(Request::FileContainingSymbol(s)) if s == "demo.v1.Greeter" => {
                        Response::Files(reflection::FileDescriptorResponse { file_descriptor_proto: vec![file("demo/greeter.proto")] })
                    }
                    // Well-known types aren't served: the client has them.
                    Some(Request::FileByFilename(n)) if n == "demo/common.proto" => {
                        Response::Files(reflection::FileDescriptorResponse { file_descriptor_proto: vec![file("demo/common.proto")] })
                    }
                    _ => not_found(),
                };
                Ok(ReflectionResponse { response: Some(response) }.encode_to_vec())
            }
            _ => Err((12, String::new())),
        }
    }
}

async fn serve_h2<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(io: S, server: Arc<Server>) {
    let Ok(mut conn) = h2::server::handshake(io).await else { return };
    while let Some(Ok((request, mut respond))) = conn.accept().await {
        let server = server.clone();
        tokio::spawn(async move {
            let (parts, mut body) = request.into_parts();
            assert_eq!(parts.headers["content-type"], "application/grpc");
            assert_eq!(parts.headers["te"], "trailers");
            assert!(parts.headers["grpc-timeout"].to_str().unwrap().ends_with('m'));
            let mut data = Vec::new();
            while let Some(Ok(chunk)) = body.data().await {
                let _ = body.flow_control().release_capacity(chunk.len());
                data.extend_from_slice(&chunk);
            }
            let head = http::Response::builder().status(200).header("content-type", "application/grpc").body(()).unwrap();
            match server.answer(parts.uri.path(), &parts.headers, &data) {
                Ok(message) => {
                    let mut send = respond.send_response(head, false).unwrap();
                    send.send_data(Bytes::from(frame(&message)), false).unwrap();
                    let _ = send.send_trailers(trailers(0, ""));
                }
                Err((code, message)) => {
                    let mut send = respond.send_response(head, false).unwrap();
                    let _ = send.send_trailers(trailers(code, &message.replace(' ', "%20")));
                }
            }
        });
    }
}

async fn serve(tls: bool, reflect: Reflect) -> (std::net::SocketAddr, Arc<Server>) {
    let server = Arc::new(Server { pool: server_pool(), reflect, reflection_calls: AtomicUsize::new(0) });
    let acceptor = tls_acceptor_alpn(&[b"h2"]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let shared = server.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, server) = (acceptor.clone(), shared.clone());
            tokio::spawn(async move {
                if tls {
                    if let Ok(stream) = acceptor.accept(tcp).await {
                        serve_h2(stream, server).await;
                    }
                } else {
                    serve_h2(tcp, server).await;
                }
            });
        }
    });
    (addr, server)
}

/// Each test its own monitor id: the descriptor cache is keyed by it.
fn grpc_kw(id: i64, target: &str, proto: Option<&str>) -> Monitor {
    let mut m = monitor(MonitorType::GrpcKeyword, target);
    m.id = id;
    m.timeout_seconds = 3;
    m.grpc_proto = proto.map(str::to_string);
    m.http_body = Some(r#"{"name": "vole", "ignored": true}"#.into());
    m
}

// -- schema ---------------------------------------------------------------

#[test]
fn a_pasted_proto_compiles_with_well_known_imports() {
    let pool = schema::compile_proto(PROTO).unwrap();
    assert!(pool.get_service_by_name("demo.v1.Greeter").is_some());
    assert!(pool.get_message_by_name("google.protobuf.Timestamp").is_some());
}

#[test]
fn compiler_errors_name_the_line() {
    let err = schema::compile_proto("syntax = \"proto3\";\nmessage X {\n  strin a = 1;\n}\n").unwrap_err();
    assert!(err.starts_with("line 3: ") && err.contains("name 'strin' is not defined"), "{err}");
    let err = schema::compile_proto("syntax = \"proto3\";\nimport \"other.proto\";\n").unwrap_err();
    assert!(err.contains("import 'other.proto' not found"), "{err}");
    let err = schema::compile_proto("syntax = \"proto3\";\nmessage {").unwrap_err();
    assert!(err.starts_with("line 2: "), "{err}");
    assert!(schema::compile_proto(&" ".repeat(schema::MAX_PROTO_LEN + 1)).unwrap_err().contains("larger than"));
}

#[test]
fn services_resolve_by_full_or_short_name_and_methods_ignore_case() {
    let pool = schema::compile_proto(PROTO).unwrap();
    for (service, method) in [("demo.v1.Greeter", "SayHello"), ("Greeter", "sayHello"), ("v1.Greeter", "sayhello"), (".demo.v1.Greeter", "SayHello")] {
        let m = schema::find_method(&pool, service, method).unwrap_or_else(|e| panic!("{service}/{method}: {e}"));
        assert_eq!(m.full_name(), "demo.v1.Greeter.SayHello");
    }
    let err = schema::find_method(&pool, "Greeter", "Watch").unwrap_err();
    assert_eq!(err, "demo.v1.Greeter.Watch is a streaming method; only unary methods can be checked");
    assert!(schema::find_method(&pool, "Greeter", "Nope").unwrap_err().contains("(methods: SayHello, Big, Watch)"));
    assert!(schema::find_method(&pool, "Nope", "x").unwrap_err().contains("services: demo.v1.Greeter"));
    let two = schema::compile_proto("syntax = \"proto3\";\npackage a;\nmessage M {}\nservice S { rpc Do (M) returns (M); }\n").unwrap();
    let mut both = two.clone();
    both.add_file_descriptor_set(prost_types::FileDescriptorSet {
        file: vec![{
            let mut f = two.get_file_by_name(schema::PROTO_FILE).unwrap().file_descriptor_proto().clone();
            f.name = Some("b.proto".into());
            f.dependency = vec![schema::PROTO_FILE.into()];
            f.package = Some("b".into());
            f
        }],
    })
    .unwrap();
    assert!(schema::find_method(&both, "S", "Do").unwrap_err().contains("ambiguous"));
    assert!(schema::find_method(&both, "a.S", "Do").is_ok());
}

#[test]
fn json_requests_and_answers_round_trip() {
    let pool = schema::compile_proto(PROTO).unwrap();
    let m = schema::find_method(&pool, "Greeter", "SayHello").unwrap();
    // Unknown fields are ignored (like Kuma); an empty body is all defaults.
    let bytes = schema::encode_request(&m, r#"{"name": "vole", "unknownField": [1, 2]}"#).unwrap();
    assert_eq!(bytes, [0x0A, 4, b'v', b'o', b'l', b'e']);
    assert_eq!(schema::encode_request(&m, "  ").unwrap(), Vec::<u8>::new());
    assert!(schema::encode_request(&m, r#"{"name": 5}"#).unwrap_err().contains("demo.v1.HelloRequest"));
    assert!(schema::encode_request(&m, r#"{"name": "a"} trailing"#).is_err());

    let answer = reply(
        r#"{"message":"hi","mood":"HAPPY","stats":{"bigNumber":"9007199254740993"},"at":"2026-10-20T12:00:00Z","zero":0}"#,
        &pool,
        "demo.v1.HelloReply",
    );
    let json = schema::decode_response(&m, &answer).unwrap();
    // camelCase keys, enum names, int64 as a string, defaults left out.
    assert_eq!(json, r#"{"message":"hi","mood":"HAPPY","stats":{"bigNumber":"9007199254740993"},"at":"2026-10-20T12:00:00Z"}"#);
    assert!(schema::decode_response(&m, &[0x0A, 50]).unwrap_err().contains("isn't a demo.v1.HelloReply"));
}

#[test]
fn targets_name_service_and_method() {
    let t = parse_target("grpcs://api.example.com/demo.v1.Greeter/SayHello").unwrap();
    assert_eq!((t.endpoint.tls, t.endpoint.port, t.service.as_str(), t.method.as_str()), (true, 443, "demo.v1.Greeter", "SayHello"));
    let t = parse_target("api.local:9000/Greeter/sayHello").unwrap();
    assert_eq!((t.endpoint.tls, t.endpoint.port, t.service.as_str()), (false, 9000, "Greeter"));
    assert!(parse_target("grpc://h").unwrap_err().contains("package.Service/Method"));
    assert!(parse_target("grpc://h/Greeter").unwrap_err().contains("package.Service/Method"));
    assert!(parse_target("grpc://h/a..b/M").is_err());
    assert!(parse_target("grpc://h/a/b/M").is_err());
    assert!(parse_target("http://h/S/M").unwrap_err().contains("Unsupported scheme"));
}

#[test]
fn keywords_are_matched_in_the_json() {
    let json = r#"{"status":"SERVING"}"#;
    assert_eq!(evaluate(json, Some("SERVING"), false).unwrap(), r#"Keyword "SERVING" found — {"status":"SERVING"}"#);
    assert_eq!(evaluate(json, None, false).unwrap(), r#"OK — {"status":"SERVING"}"#);
    assert_eq!(evaluate("{}", Some(""), true).unwrap(), "OK — empty answer ({})");
    assert!(evaluate(json, Some("serving"), false).unwrap_err().starts_with("Keyword \"serving\" not found — answer: "));
    assert!(evaluate(json, Some("SERVING"), true).unwrap_err().contains("found but should be absent"));
    assert!(evaluate(json, Some("DOWN"), true).unwrap().contains("absent, as expected"));
}

#[test]
fn reserved_metadata_is_refused() {
    let mut m = monitor(MonitorType::GrpcKeyword, "grpc://h/S/M");
    m.http_headers = Some(r#"{"te": "x"}"#.into());
    assert!(metadata(&m).unwrap_err().contains("\"te\""));
    m.http_headers = Some(r#"{"authorization": "Bearer t", "x-bin-bin": "AAEC"}"#.into());
    assert_eq!(metadata(&m).unwrap().len(), 2);
}

#[test]
fn the_cache_is_bounded_and_keyed_by_what_built_it() {
    let cache = schema::Cache::default();
    let pool = DescriptorPool::new();
    let now = std::time::Instant::now();
    cache.put(1, 7, pool.clone(), None, now);
    assert!(cache.get(1, 7, now).is_some());
    assert!(cache.get(1, 8, now).is_none(), "another .proto");
    cache.put(2, 7, pool.clone(), Some(Duration::from_secs(60)), now);
    assert!(cache.get(2, 7, now + Duration::from_secs(59)).is_some());
    assert!(cache.get(2, 7, now + Duration::from_secs(61)).is_none(), "reflection results expire");
    for id in 10..400 {
        cache.put(id, 1, pool.clone(), None, now);
    }
    assert!(cache.entries() <= 256);
    cache.invalidate(399);
    assert!(cache.get(399, 1, now).is_none());
}

#[test]
fn agents_need_the_batch_version() {
    use crate::agent::protocol::{BATCH_0_3_SINCE, agent_can_run, introduced_in};
    assert_eq!(introduced_in(MonitorType::GrpcKeyword), Some(BATCH_0_3_SINCE));
    let m = monitor(MonitorType::GrpcKeyword, "grpc://h/S/M");
    assert!(!agent_can_run(&m, Some("0.1.9")) && !agent_can_run(&m, None) && agent_can_run(&m, Some("0.3.0")));
    // A monitor from a server without the column still reads.
    let mut value = serde_json::to_value(&m).unwrap();
    value.as_object_mut().unwrap().remove("grpc_proto");
    assert_eq!(serde_json::from_value::<Monitor>(value).unwrap().grpc_proto, None);
}

// -- calls ----------------------------------------------------------------

#[tokio::test]
async fn keyword_found_missing_and_inverted() {
    let (addr, _) = serve(false, Reflect::Off).await;
    let mut m = grpc_kw(9_101, &format!("grpc://{addr}/demo.v1.Greeter/SayHello"), Some(PROTO));
    m.keyword = Some(r#""mood":"HAPPY""#.into());
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    let msg = out.message.unwrap();
    assert!(msg.starts_with(r#"Keyword ""mood":"HAPPY"" found — {"message":"Hello vole","mood":"HAPPY","stats":{"bigNumber":"9007199254740993"}"#), "{msg}");
    assert!(out.response_time_ms.is_some());

    m.keyword = Some("Goodbye".into());
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(out.message.unwrap().starts_with("Keyword \"Goodbye\" not found — answer: {\"message\":\"Hello vole\""));
    m.keyword_invert = true;
    assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    m.keyword = Some("Hello".into());
    assert_eq!(check(&m).await.status, HeartbeatStatus::Down);

    m.keyword = None;
    m.keyword_invert = false;
    m.target = format!("grpc://{addr}/Greeter/sayHello");
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert!(out.message.unwrap().starts_with("OK — {\"message\":\"Hello vole\""));
}

#[tokio::test]
async fn metadata_is_sent_and_error_statuses_are_down() {
    let (addr, _) = serve(false, Reflect::Off).await;
    let mut m = grpc_kw(9_102, &format!("grpc://{addr}/demo.v1.Greeter/SayHello"), Some(PROTO));
    m.http_headers = Some(r#"{"x-team": "ops"}"#.into());
    m.keyword = Some("Hello vole from ops".into());
    assert_eq!(check(&m).await.status, HeartbeatStatus::Up);

    m.http_headers = Some(r#"{"x-fail": "notfound"}"#.into());
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert_eq!(out.message.as_deref(), Some("gRPC status NOT_FOUND: no such greeting"));
    // Unlike Uptime Kuma, CANCELLED is not up.
    m.http_headers = Some(r#"{"x-fail": "cancelled"}"#.into());
    assert_eq!(check(&m).await.message.as_deref(), Some("gRPC status CANCELLED"));
}

#[tokio::test]
async fn a_broken_proto_or_method_fails_before_calling() {
    let mut m = grpc_kw(9_103, "grpc://127.0.0.1:1/demo.v1.Greeter/SayHello", Some("syntax = \"proto3\";\nmessage X { strin a = 1; }"));
    let out = check(&m).await;
    assert!(out.message.unwrap().starts_with("The .proto doesn't compile: line 2: name 'strin' is not defined"));
    let (addr, _) = serve(false, Reflect::Off).await;
    m.grpc_proto = Some(PROTO.into());
    m.target = format!("grpc://{addr}/demo.v1.Greeter/Watch");
    assert!(check(&m).await.message.unwrap().contains("is a streaming method"));
    m.target = format!("grpc://{addr}/demo.v1.Greeter/SayHello");
    m.http_body = Some(r#"{"name": 1}"#.into());
    assert!(check(&m).await.message.unwrap().starts_with("The request body doesn't fit demo.v1.HelloRequest"));
}

#[tokio::test]
async fn oversized_answers_fail() {
    let (addr, _) = serve(false, Reflect::Off).await;
    let m = grpc_kw(9_104, &format!("grpc://{addr}/demo.v1.Greeter/Big"), Some(PROTO));
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert_eq!(out.message.as_deref(), Some("The answer is larger than 64 KiB"));
}

#[tokio::test]
async fn reflection_v1_fetches_imports_and_is_cached() {
    let (addr, server) = serve(false, Reflect::V1).await;
    let mut m = grpc_kw(9_105, &format!("grpc://{addr}/Greeter/sayHello"), None);
    m.keyword = Some("2026-10-20T12:00:00Z".into());
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    // list services, the service's file, then its import (the well-known
    // Timestamp is built in, so not asked for).
    assert_eq!(server.reflection_calls.load(Ordering::SeqCst), 3);
    assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    assert_eq!(server.reflection_calls.load(Ordering::SeqCst), 3, "cached");
    forget(m.id);
    m.target = format!("grpc://{addr}/demo.v1.Greeter/SayHello");
    assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
    assert_eq!(server.reflection_calls.load(Ordering::SeqCst), 5, "no service list needed for a full name");
    // A failed check drops reflected descriptors (the server may have changed).
    m.keyword = Some("absent".into());
    assert_eq!(check(&m).await.status, HeartbeatStatus::Down);
    assert_eq!(check(&m).await.status, HeartbeatStatus::Down);
    assert_eq!(server.reflection_calls.load(Ordering::SeqCst), 7);
}

#[tokio::test]
async fn reflection_falls_back_to_v1alpha() {
    let (addr, server) = serve(false, Reflect::V1Alpha).await;
    let m = grpc_kw(9_106, &format!("grpc://{addr}/demo.v1.Greeter/SayHello"), None);
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert_eq!(server.reflection_calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn reflection_errors_are_explained() {
    let (addr, _) = serve(false, Reflect::Off).await;
    let m = grpc_kw(9_107, &format!("grpc://{addr}/demo.v1.Greeter/SayHello"), None);
    assert!(check(&m).await.message.unwrap().contains("doesn't offer server reflection"));
    let (addr, _) = serve(false, Reflect::V1).await;
    let m = grpc_kw(9_108, &format!("grpc://{addr}/Nope/Do"), None);
    assert_eq!(check(&m).await.message.as_deref(), Some("The server has no service \"Nope\" (it lists: demo.v1.Greeter, grpc.reflection.v1.ServerReflection)"));
    let m = grpc_kw(9_109, &format!("grpc://{addr}/other.Svc/Do"), None);
    assert_eq!(check(&m).await.message.as_deref(), Some("The server doesn't know the service \"other.Svc\""));
}

#[tokio::test]
async fn grpcs_checks_the_certificate_unless_told_not_to() {
    let (addr, _) = serve(true, Reflect::V1).await;
    let target = format!("grpcs://localhost:{}/demo.v1.Greeter/SayHello", addr.port());
    let mut m = grpc_kw(9_110, &target, Some(PROTO));
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Down);
    assert!(out.message.as_deref().unwrap().contains("not trusted"), "{:?}", out.message);
    m.ignore_tls = true;
    let out = check(&m).await;
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    assert!(out.message.as_deref().unwrap().ends_with(" · TLS 1.3"));
    assert!(out.expiry.is_some());
    // Reflection runs over TLS too.
    m.grpc_proto = None;
    assert_eq!(check(&m).await.status, HeartbeatStatus::Up);
}

/// Against the public test server: `cargo test --lib grpcbin -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs internet (grpcb.in:9000)"]
async fn live_grpcbin_reflection() {
    let mut m = grpc_kw(9_199, "grpc://grpcb.in:9000/hello.HelloService/SayHello", None);
    m.timeout_seconds = 15;
    m.http_body = Some(r#"{"greeting": "vole"}"#.into());
    m.keyword = Some("hello vole".into());
    let out = check(&m).await;
    println!("hello.HelloService/SayHello -> {:?} {:?} {:?}", out.status, out.response_time_ms, out.message);
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
    let mut m = grpc_kw(9_198, "grpc://grpcb.in:9000/GRPCBin/index", None);
    m.timeout_seconds = 15;
    m.http_body = None;
    let out = check(&m).await;
    println!("GRPCBin/index -> {:?} {:?} {:?}", out.status, out.response_time_ms, out.message);
    assert_eq!(out.status, HeartbeatStatus::Up, "{:?}", out.message);
}
