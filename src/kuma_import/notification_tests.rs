//! Kuma notification providers that map onto ours: each mapped config must
//! hold the right fields and pass our own validation.

use serde_json::{Value, json};

use super::*;
use crate::notification::{provider, schema};

/// Maps a Kuma notification config and checks the result against our schema.
fn mapped(kuma: Value) -> (&'static str, Value) {
    let Value::Object(row) = kuma else { panic!("object expected") };
    let (kind, config, _warning) = map_notification(&row).unwrap_or_else(|e| panic!("{row:?}: {e}"));
    let config = Value::Object(config);
    let schema = provider(kind).unwrap_or_else(|| panic!("no provider {kind}"));
    schema::validate(schema, &config).unwrap_or_else(|errors| panic!("{kind} {config}: {errors:?}"));
    (kind, config)
}

#[test]
fn chat_and_messaging_providers_map_across() {
    let (kind, c) = mapped(json!({"type": "threema", "threemaSenderIdentity": "*VOLE123", "threemaSecret": "S3", "threemaRecipientType": "phone", "threemaRecipient": "4712345678"}));
    assert_eq!(kind, "threema");
    assert_eq!((c["gateway_id"].as_str(), c["secret"].as_str(), c["recipient_type"].as_str(), c["recipient"].as_str()), (Some("*VOLE123"), Some("S3"), Some("phone"), Some("4712345678")));
    // An unknown recipient type falls back to a Threema ID.
    let (_, c) = mapped(json!({"type": "threema", "threemaSenderIdentity": "*VOLE123", "threemaSecret": "S3", "threemaRecipientType": "carrier-pigeon", "threemaRecipient": "ABCD1234"}));
    assert_eq!(c["recipient_type"], "identity");

    let (kind, c) = mapped(json!({"type": "nextcloudtalk", "host": "https://cloud.example.com", "conversationToken": "abcd1234", "botSecret": "x".repeat(40), "sendSilentUp": true}));
    assert_eq!(kind, "nextcloudtalk");
    assert_eq!((c["host"].as_str(), c["conversation_token"].as_str(), c["silent_up"].as_bool()), (Some("https://cloud.example.com"), Some("abcd1234"), Some(true)));

    let (kind, c) = mapped(json!({"type": "waha", "wahaApiUrl": "http://waha:3000", "wahaSession": "work", "wahaChatId": "4712345678@c.us", "wahaApiKey": "K"}));
    assert_eq!((kind, c["session"].as_str(), c["chat_id"].as_str(), c["api_key"].as_str()), ("waha", Some("work"), Some("4712345678@c.us"), Some("K")));

    let (kind, c) = mapped(json!({"type": "evolution", "evolutionApiUrl": "https://evo.example.com", "evolutionInstanceName": "vole", "evolutionAuthToken": "K", "evolutionRecipient": "4712345678"}));
    assert_eq!((kind, c["instance"].as_str(), c["number"].as_str(), c["api_key"].as_str()), ("evolution", Some("vole"), Some("4712345678"), Some("K")));
}

#[test]
fn zulip_has_no_kuma_counterpart() {
    let Value::Object(row) = json!({"type": "zulip"}) else { unreachable!() };
    assert!(map_notification(&row).is_err());
}

#[test]
fn incident_tools_map_across() {
    let (kind, c) = mapped(json!({"type": "SIGNL4", "webhookURL": "https://connect.signl4.com/webhook/team"}));
    assert_eq!((kind, c["url"].as_str()), ("signl4", Some("https://connect.signl4.com/webhook/team")));
    let (kind, c) = mapped(json!({"type": "squadcast", "squadcastWebhookURL": "https://api.squadcast.com/v3/incidents/api/abc"}));
    assert_eq!((kind, c["url"].as_str()), ("squadcast", Some("https://api.squadcast.com/v3/incidents/api/abc")));
    let (kind, c) = mapped(json!({"type": "GrafanaOncall", "GrafanaOncallURL": "https://oncall.example/integrations/v1/webhook/abc/"}));
    assert_eq!((kind, c["url"].as_str()), ("grafana_oncall", Some("https://oncall.example/integrations/v1/webhook/abc/")));
    let (kind, c) = mapped(json!({"type": "GoAlert", "goAlertBaseURL": "https://goalert.example", "goAlertToken": "T"}));
    assert_eq!((kind, c["server"].as_str(), c["token"].as_str()), ("goalert", Some("https://goalert.example"), Some("T")));
    let (kind, c) = mapped(json!({"type": "Keep", "webhookURL": "https://keep.example", "webhookAPIKey": "K"}));
    assert_eq!((kind, c["server"].as_str(), c["api_key"].as_str()), ("keep", Some("https://keep.example"), Some("K")));
}

#[test]
fn splunk_on_call_splits_the_rest_endpoint_into_key_and_routing_key() {
    let (kind, c) = mapped(json!({"type": "Splunk", "splunkRestURL": "https://alert.victorops.com/integrations/generic/20131114/alert/API-KEY/everyone", "splunkSeverity": "CRITICAL"}));
    assert_eq!((kind, c["api_key"].as_str(), c["routing_key"].as_str(), c["severity"].as_str()), ("splunk_oncall", Some("API-KEY"), Some("everyone"), Some("CRITICAL")));
    let (_, c) = mapped(json!({"type": "Splunk", "splunkRestURL": "https://alert.victorops.com/integrations/generic/20131114/alert/K/r/", "splunkSeverity": "INFO"}));
    assert_eq!(c["severity"], "WARNING");
    for bad in ["https://example.com/hook", "https://alert.victorops.com/integrations/generic/20131114/alert/onlykey", ""] {
        let Value::Object(row) = json!({"type": "Splunk", "splunkRestURL": bad}) else { unreachable!() };
        assert!(map_notification(&row).is_err(), "{bad}");
    }
}
