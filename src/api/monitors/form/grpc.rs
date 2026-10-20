//! The gRPC keyword part of the monitor form: the `.proto` text (compiled
//! here, so mistakes show on the field), the request body and metadata,
//! and the keyword.

use super::{Errors, FormValues, MAX_BODY_LEN, first_line, int_in, non_empty};
use crate::db::models::NewMonitor;
use crate::monitor::checkers::{self, grpc_keyword};
use crate::monitor::checkers::grpc_keyword::schema;

pub(super) fn validate(v: &FormValues, m: &mut NewMonitor, e: &mut Errors) {
    // Metadata: a JSON object of header names and values.
    m.http_headers = match non_empty(&v.http_headers) {
        None => None,
        Some(h) => match checkers::parse_headers(&h) {
            Ok(map) => match map.keys().find(|n| checkers::grpc::is_reserved_metadata(n.as_str())) {
                Some(name) => {
                    e.insert("http_headers", crate::i18n::tf("grpckw.err_metadata_reserved", &[("name", name.as_str())]));
                    None
                }
                None => Some(h),
            },
            Err(msg) => {
                e.insert("http_headers", first_line(&msg));
                None
            }
        },
    };

    // Request body: a JSON object (empty = every field at its default).
    m.http_body = (!v.http_body.trim().is_empty()).then(|| v.http_body.clone());
    m.http_body_encoding = "json".into();
    if let Some(body) = &m.http_body {
        if body.len() > MAX_BODY_LEN {
            e.insert("http_body", crate::i18n::t("validation.body_long").into());
        } else if !matches!(serde_json::from_str::<serde_json::Value>(body), Ok(serde_json::Value::Object(_))) {
            e.insert("http_body", crate::i18n::t("grpckw.err_body_json").into());
        }
    }

    m.keyword = (!v.keyword.trim().is_empty()).then(|| v.keyword.clone());
    m.keyword_invert = v.keyword_invert;
    m.expiry_notify = v.expiry_notify;
    m.expiry_days = int_in(e, "expiry_days", &v.expiry_days, 1, 365, crate::i18n::t("validation.what_days")).unwrap_or(m.expiry_days);

    // The .proto: compiled now, then the method and body checked against it.
    m.grpc_proto = non_empty(&v.grpc_proto.replace("\r\n", "\n"));
    if let Some(proto) = &m.grpc_proto {
        if proto.len() > schema::MAX_PROTO_LEN {
            e.insert("grpc_proto", crate::i18n::tf("grpckw.err_proto_long", &[("max", &(schema::MAX_PROTO_LEN / 1024).to_string())]));
        } else {
            match schema::compile_proto(proto) {
                Err(msg) => {
                    e.insert("grpc_proto", crate::i18n::tf("grpckw.err_proto", &[("err", &msg)]));
                }
                Ok(pool) => {
                    if let Ok(target) = grpc_keyword::parse_target(&m.target) {
                        match schema::find_method(&pool, &target.service, &target.method) {
                            Err(msg) => {
                                e.entry("target").or_insert_with(|| first_line(&msg));
                            }
                            Ok(method) => {
                                if !e.contains_key("http_body")
                                    && let Err(msg) = schema::encode_request(&method, m.http_body.as_deref().unwrap_or_default())
                                {
                                    e.insert("http_body", first_line(&msg));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // HTTP request options don't apply: no stale secret stays behind.
    m.proxy_url = None;
    m.auth_method = "none".into();
    m.basic_auth_user = None;
    m.basic_auth_pass = None;
    m.oauth_token_url = None;
    m.oauth_client_id = None;
    m.oauth_client_secret = None;
    m.oauth_scope = None;
    m.oauth_audience = None;
    m.oauth_client_auth = "client_secret_basic".into();
    m.tls_client_cert = None;
    m.tls_client_key = None;
    m.tls_ca_cert = None;
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::db::models::MonitorType;

    const PROTO: &str = "syntax = \"proto3\";\r\npackage demo.v1;\r\nservice Greeter {\r\n  rpc SayHello (HelloRequest) returns (HelloReply);\r\n  rpc Watch (HelloRequest) returns (stream HelloReply);\r\n}\r\nmessage HelloRequest { string name = 1; }\r\nmessage HelloReply { string message = 1; }\r\n";

    fn pairs(extra: &[(&str, &str)]) -> Pairs {
        let mut list: Vec<(String, String)> = [
            ("name", "Greeter"),
            ("monitor_type", "grpc_keyword"),
            ("target_grpc_keyword", "grpc://api.local:50051/demo.v1.Greeter/SayHello"),
            ("interval_seconds", "60"),
            ("timeout_seconds", "10"),
            ("retries", "0"),
            ("resend_interval", "0"),
            ("escalation_minutes", "0"),
            ("expiry_days", "14"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for (k, v) in extra {
            list.retain(|(name, _)| name != k);
            list.push((k.to_string(), v.to_string()));
        }
        Pairs(list)
    }

    fn run(extra: &[(&str, &str)]) -> Result<Validated, Errors> {
        validate(&FormValues::from_pairs(&pairs(extra), &[]), &NewMonitor::default(), &FormContext::default())
    }

    #[test]
    fn a_complete_monitor_validates() {
        let ok = run(&[
            ("grpc_proto", PROTO),
            ("http_body", "{\"name\": \"vole\", \"extra\": 1}"),
            ("http_headers", "{\"authorization\": \"Bearer x\"}"),
            ("keyword", "Hello"),
            ("keyword_invert", "true"),
            ("ignore_tls", "true"),
        ])
        .unwrap()
        .monitor;
        assert_eq!(ok.monitor_type, MonitorType::GrpcKeyword);
        assert_eq!(ok.grpc_proto.as_deref(), Some(PROTO.replace("\r\n", "\n").trim()), "line endings normalised");
        assert_eq!((ok.keyword.as_deref(), ok.keyword_invert, ok.ignore_tls), (Some("Hello"), true, true));
        assert_eq!(ok.http_headers.as_deref(), Some("{\"authorization\": \"Bearer x\"}"));
        assert_eq!(ok.http_body_encoding, "json");
        // Without a .proto (server reflection) only the shape is checked.
        let ok = run(&[("target_grpc_keyword", "grpcs://api.example.com/Greeter/sayHello")]).unwrap().monitor;
        assert_eq!((ok.grpc_proto, ok.keyword), (None, None));
    }

    #[test]
    fn proto_mistakes_show_on_the_field() {
        let err = run(&[("grpc_proto", "syntax = \"proto3\";\nmessage X {\n  strin a = 1;\n}\n")]).unwrap_err();
        let msg = &err["grpc_proto"];
        assert!(msg.contains("line 3") && msg.contains("name 'strin' is not defined"), "{msg}");
        let err = run(&[("grpc_proto", "syntax = \"proto3\";\nimport \"other.proto\";\n")]).unwrap_err();
        assert!(err["grpc_proto"].contains("import 'other.proto' not found"), "{}", err["grpc_proto"]);
        let long = "x".repeat(crate::monitor::checkers::grpc_keyword::schema::MAX_PROTO_LEN + 1);
        assert!(run(&[("grpc_proto", &long)]).unwrap_err().contains_key("grpc_proto"));
    }

    #[test]
    fn method_and_body_are_checked_against_the_proto() {
        let err = run(&[("grpc_proto", PROTO), ("target_grpc_keyword", "grpc://h/demo.v1.Greeter/Watch")]).unwrap_err();
        assert!(err["target"].contains("v1.Greeter.Watch is a streaming method"), "{}", err["target"]);
        let err = run(&[("grpc_proto", PROTO), ("target_grpc_keyword", "grpc://h/demo.v1.Greeter/Nope")]).unwrap_err();
        assert!(err["target"].contains("has no method \"Nope\" (methods: SayHello, Watch)"), "{}", err["target"]);
        let err = run(&[("grpc_proto", PROTO), ("target_grpc_keyword", "grpc://h/Other/SayHello")]).unwrap_err();
        assert!(err["target"].contains("No service \"Other\""), "{}", err["target"]);
        let err = run(&[("grpc_proto", PROTO), ("http_body", "{\"name\": 5}")]).unwrap_err();
        assert!(err["http_body"].contains("demo.v1.HelloRequest"), "{}", err["http_body"]);
        assert!(run(&[("http_body", "[1]")]).unwrap_err().contains_key("http_body"));
        assert!(run(&[("http_body", "{")]).unwrap_err().contains_key("http_body"));
    }

    #[test]
    fn targets_need_a_service_and_method() {
        for bad in ["grpc://h:50051", "grpc://h/OnlyService", "grpc://h/a b/C", "http://h/S/M", "grpc://h/S/M.x"] {
            assert!(run(&[("target_grpc_keyword", bad)]).unwrap_err().contains_key("target"), "{bad}");
        }
    }

    #[test]
    fn reserved_metadata_and_stale_secrets() {
        let err = run(&[("http_headers", "{\"content-type\": \"text/plain\"}")]).unwrap_err();
        assert!(err["http_headers"].contains("content-type"), "{}", err["http_headers"]);
        let base = NewMonitor {
            monitor_type: MonitorType::Http,
            auth_method: "basic".into(),
            basic_auth_user: Some("bot".into()),
            basic_auth_pass: Some("pw".into()),
            proxy_url: Some("http://proxy:3128".into()),
            ..NewMonitor::default()
        };
        let ok = validate(&FormValues::from_pairs(&pairs(&[]), &[]), &base, &FormContext::default()).unwrap().monitor;
        assert_eq!((ok.auth_method.as_str(), ok.basic_auth_pass, ok.proxy_url), ("none", None, None));
    }

    #[test]
    fn grpc_settings_round_trip_through_the_form() {
        let m = NewMonitor {
            name: "Greeter".into(),
            monitor_type: MonitorType::GrpcKeyword,
            target: "grpcs://api.local/demo.v1.Greeter/SayHello".into(),
            grpc_proto: Some(PROTO.replace("\r\n", "\n").trim().to_string()),
            http_body: Some("{\"name\":\"vole\"}".into()),
            http_headers: Some("{\"x-team\":\"ops\"}".into()),
            keyword: Some("Hello vole".into()),
            ..NewMonitor::default()
        };
        let v = FormValues::from_new(&m, Vec::new(), &[]);
        let back = validate(&v, &m, &FormContext::default()).unwrap().monitor;
        assert_eq!(back, m);
    }
}
