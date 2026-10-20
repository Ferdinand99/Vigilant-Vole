//! Kuma SNMPv3 and `tailscale-ping` monitors.

use super::*;

fn doc<'a>(import: &'a KumaImport, name: &str) -> &'a MonitorDoc {
    import.doc.monitors.as_ref().unwrap().iter().find(|m| m.name == name).unwrap_or_else(|| panic!("{name}: {:?}", import.skipped))
}

#[test]
fn snmpv3_imports_its_user_without_passwords() {
    let json = r#"{
      "version": "2.0.0",
      "monitorList": [
        {"id": 1, "name": "Core", "type": "snmp", "hostname": "core.local", "port": 161, "snmpOid": ".1.3.6.1.2.1.1.5.0", "snmpVersion": "3", "snmp_v3_username": "watcher", "radiusPassword": "public"},
        {"id": 2, "name": "Edge", "type": "snmp", "hostname": "edge.local", "snmpVersion": "3", "snmpV3Username": "ops"},
        {"id": 3, "name": "Nameless", "type": "snmp", "hostname": "x.local", "snmpVersion": "3"}
      ]
    }"#;
    let import = read_json(json).unwrap();
    let core = doc(&import, "Core");
    assert_eq!((core.monitor_type.as_str(), core.target.as_str()), ("snmp", "snmp://core.local:161/1.3.6.1.2.1.1.5.0?version=3"));
    assert_eq!(core.options["basic_auth_user"], "watcher");
    assert!(!core.options.contains_key("secret") && !core.options.contains_key("basic_auth_pass"), "noAuthNoPriv: no community, no passwords");
    assert_eq!(doc(&import, "Edge").target, "snmp://edge.local?version=3");
    assert_eq!(doc(&import, "Edge").options["basic_auth_user"], "ops");
    let skipped = import.skipped.iter().find(|s| s.name == "Nameless").unwrap();
    assert!(skipped.reason.contains("no user name"), "{skipped:?}");
}

#[test]
fn tailscale_ping_keeps_its_peer() {
    let json = r#"{
      "version": "2.0.0",
      "monitorList": [
        {"id": 1, "name": "NAS over Tailscale", "type": "tailscale-ping", "hostname": "nas", "interval": 60}
      ]
    }"#;
    let import = read_json(json).unwrap();
    let nas = doc(&import, "NAS over Tailscale");
    assert_eq!((nas.monitor_type.as_str(), nas.target.as_str()), ("tailscale_ping", "nas"));
    assert!(import.skipped.is_empty(), "{:?}", import.skipped);
}
