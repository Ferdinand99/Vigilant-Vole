//! Uptime Kuma's `grpc-keyword` monitors → `grpc_keyword`.
//!
//! Kuma keeps the address (`grpc_url`, `host:port`) and TLS switch apart
//! from the service and method; here they make one target
//! (`grpcs://host:port/pkg.Service/Method`). The `.proto`, JSON body,
//! keyword and invert move over as they are. Kuma never sent its metadata;
//! here it is sent, so the plan says so.

use serde_json::{Map, Value, json};

use super::{Row, b, get, raw, s};

/// `(our type, target)`; the other settings go into `opts`.
pub(super) fn map(
    row: &Row,
    opts: &mut Map<String, Value>,
    warnings: &mut Vec<String>,
) -> Result<(&'static str, String), String> {
    let url = s(row, &["grpc_url", "grpcUrl"]).ok_or("It has no gRPC server address.")?;
    let tls = b(row, &["grpc_enable_tls", "grpcEnableTls"]).unwrap_or(false);
    // Kuma's address is `host:port`; a scheme in front decides nothing there.
    let address = url
        .split_once("://")
        .map_or(url.as_str(), |(_, rest)| rest)
        .trim_end_matches('/');
    let service =
        s(row, &["grpc_service_name", "grpcServiceName"]).ok_or("It names no gRPC service.")?;
    let method = s(row, &["grpc_method", "grpcMethod"]).ok_or("It names no gRPC method.")?;
    let scheme = if tls { "grpcs" } else { "grpc" };
    let target = format!(
        "{scheme}://{address}/{}/{}",
        service.trim().trim_matches('.'),
        method.trim()
    );

    if let Some(proto) =
        raw(row, &["grpc_protobuf", "grpcProtobuf"]).filter(|p| !p.trim().is_empty())
    {
        opts.insert("grpc_proto".into(), json!(proto));
    } else {
        warnings.push("it has no .proto, so the server is asked for its schema (server reflection must be on).".into());
    }
    if let Some(body) = get(row, &["grpc_body", "grpcBody"])
        .and_then(Value::as_str)
        .filter(|b| !b.trim().is_empty())
    {
        opts.insert("http_body".into(), json!(body));
        opts.insert("http_body_encoding".into(), json!("json"));
    }
    if let Some(meta) = s(row, &["grpc_metadata", "grpcMetadata"]) {
        match serde_json::from_str::<Value>(&meta) {
            Ok(Value::Object(map)) if !map.is_empty() => {
                opts.insert("http_headers".into(), Value::Object(map));
                warnings.push(
                    "its metadata is now sent with the call (Uptime Kuma never sent it).".into(),
                );
            }
            Ok(Value::Object(_)) => {}
            _ => warnings.push("its metadata isn’t a JSON object and was left out.".into()),
        }
    }
    if let Some(keyword) = get(row, &["keyword"])
        .and_then(Value::as_str)
        .filter(|k| !k.is_empty())
    {
        opts.insert("keyword".into(), json!(keyword));
    }
    if b(row, &["invert_keyword", "invertKeyword"]).unwrap_or(false) {
        opts.insert("keyword_invert".into(), json!(true));
    }
    warnings.push("a CANCELLED answer counts as down here (Uptime Kuma counted it as up).".into());
    Ok(("grpc_keyword", target))
}

#[cfg(test)]
mod tests {
    use crate::kuma_import::read_json;

    #[test]
    fn grpc_keyword_monitors_become_one_target() {
        let json = r#"{
            "version": "1.23.16",
            "monitorList": [
                {"id": 1, "name": "Greeter", "type": "grpc-keyword", "grpcUrl": "api.local:50051",
                 "grpcEnableTls": true, "grpcServiceName": "demo.v1.Greeter", "grpcMethod": "sayHello",
                 "grpcProtobuf": "syntax = \"proto3\";\npackage demo.v1;\nservice Greeter { rpc SayHello (Req) returns (Rep); }\nmessage Req { string name = 1; }\nmessage Rep { string message = 1; }\n",
                 "grpcBody": "{\"name\": \"vole\"}", "grpcMetadata": "{\"x-team\": \"ops\"}",
                 "keyword": "Hello", "invertKeyword": true, "interval": 60},
                {"id": 2, "name": "Plain", "type": "grpc-keyword", "grpcUrl": "10.0.0.5:9000",
                 "grpcServiceName": "Health", "grpcMethod": "check", "grpcMetadata": "not json", "keyword": "SERVING"},
                {"id": 3, "name": "Broken", "type": "grpc-keyword", "grpcUrl": "h:1"}
            ]
        }"#;
        let import = read_json(json).unwrap();
        let monitors = import.doc.monitors.as_ref().unwrap();
        let doc = |name: &str| {
            monitors
                .iter()
                .find(|m| m.name == name)
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        let greeter = doc("Greeter");
        assert_eq!(
            (greeter.monitor_type.as_str(), greeter.target.as_str()),
            (
                "grpc_keyword",
                "grpcs://api.local:50051/demo.v1.Greeter/sayHello"
            )
        );
        let o = &greeter.options;
        assert!(
            o["grpc_proto"]
                .as_str()
                .unwrap()
                .contains("service Greeter")
        );
        assert_eq!(
            (
                o["http_body"].as_str(),
                o["keyword"].as_str(),
                o["keyword_invert"].as_bool()
            ),
            (Some("{\"name\": \"vole\"}"), Some("Hello"), Some(true))
        );
        assert_eq!(o["http_headers"]["x-team"], "ops");
        let plain = doc("Plain");
        assert_eq!(plain.target, "grpc://10.0.0.5:9000/Health/check");
        assert!(
            !plain.options.contains_key("http_headers")
                && !plain.options.contains_key("grpc_proto")
        );
        assert!(!monitors.iter().any(|m| m.name == "Broken"));
        let notes = format!("{:?} {:?}", import.warnings, import.skipped);
        assert!(notes.contains("CANCELLED"), "{notes}");
        assert!(notes.contains("never sent"), "{notes}");
        assert!(notes.contains("server reflection"), "{notes}");
        assert!(notes.contains("It names no gRPC service."), "{notes}");
    }
}
