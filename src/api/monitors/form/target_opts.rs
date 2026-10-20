//! Target options the form shows as choices beside the target input: the
//! SNMP version and v3 protocols (`?version=3&auth=sha256&priv=aes`) and the
//! Tailscale ping type (`?type=tsmp`). They are stored in the target's query;
//! the form shows the target without them and puts them back on save.

use super::{Errors, Pairs};
use crate::api::monitors::FormData;
use crate::db::models::{MonitorType, NewMonitor};
use crate::monitor::checkers;

/// One option: its query key, form field, accepted values and default.
struct Opt {
    key: &'static str,
    field: &'static str,
    choices: &'static [&'static str],
    default: &'static str,
}

const SNMP: &[Opt] = &[
    Opt { key: "version", field: "snmp_version", choices: &["1", "2c", "3"], default: "2c" },
    Opt { key: "auth", field: "snmp_auth", choices: &["md5", "sha", "sha224", "sha256", "sha384", "sha512"], default: "sha" },
    Opt { key: "priv", field: "snmp_priv", choices: &["des", "aes", "aes192", "aes256"], default: "aes" },
];
const TAILSCALE: &[Opt] = &[Opt { key: "type", field: "tailscale_type", choices: &["disco", "tsmp", "icmp"], default: "disco" }];

/// SNMPv3 authentication protocols as `(value, label)`.
pub const SNMP_AUTHS: &[(&str, &str)] =
    &[("md5", "MD5"), ("sha", "SHA-1"), ("sha224", "SHA-224"), ("sha256", "SHA-256"), ("sha384", "SHA-384"), ("sha512", "SHA-512")];
/// SNMPv3 privacy protocols as `(value, label)`.
pub const SNMP_PRIVS: &[(&str, &str)] = &[("des", "DES"), ("aes", "AES-128"), ("aes192", "AES-192"), ("aes256", "AES-256")];

fn opts(kind: MonitorType) -> &'static [Opt] {
    match kind {
        MonitorType::Snmp => SNMP,
        MonitorType::TailscalePing => TAILSCALE,
        _ => &[],
    }
}

/// The spelling the choices use (`SHA-256` → `sha256`, `v3` → `3`).
fn canonical(key: &str, value: &str) -> String {
    let v = value.trim().to_ascii_lowercase().replace('-', "");
    match (key, v.as_str()) {
        ("version", "v1") => "1".into(),
        ("version", "2" | "v2c" | "v2") => "2c".into(),
        ("version", "v3") => "3".into(),
        ("auth", "sha1") => "sha".into(),
        ("priv", "aes128") => "aes".into(),
        _ => v,
    }
}

fn query_key(segment: &str) -> String {
    let key = segment.split('=').next().unwrap_or_default();
    url::form_urlencoded::parse(key.as_bytes()).next().map(|(k, _)| k.to_ascii_lowercase()).unwrap_or_default()
}

/// Splits `target` at its query: `(before, segments)`.
fn split_query(target: &str) -> (&str, Vec<&str>) {
    match target.split_once('?') {
        Some((base, query)) => (base, query.split('&').filter(|s| !s.is_empty()).collect()),
        None => (target, Vec::new()),
    }
}

fn join(base: &str, segments: &[String]) -> String {
    if segments.is_empty() { base.to_string() } else { format!("{base}?{}", segments.join("&")) }
}

/// A stored target as the form shows it: the target without the options it
/// offers as choices, and every option's value (the default when absent).
/// A value outside the choices stays in the target, so validation names it.
pub fn split(kind: MonitorType, target: &str) -> (String, Vec<(String, String)>) {
    let opts = opts(kind);
    if opts.is_empty() {
        return (target.to_string(), Vec::new());
    }
    let (base, segments) = split_query(target);
    let mut values: Vec<(String, String)> = opts.iter().map(|o| (o.field.to_string(), o.default.to_string())).collect();
    let mut kept = Vec::new();
    for segment in segments {
        let key = query_key(segment);
        let value = url::form_urlencoded::parse(segment.as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default();
        match opts.iter().position(|o| o.key == key) {
            Some(i) if opts[i].choices.contains(&canonical(&key, &value).as_str()) => values[i].1 = canonical(&key, &value),
            _ => kept.push(segment.to_string()),
        }
    }
    (join(base, &kept), values)
}

/// The option fields the form submitted (none for a raw API-style post).
pub fn read(kind: MonitorType, p: &Pairs) -> Vec<(String, String)> {
    opts(kind).iter().filter_map(|o| p.raw(o.field).map(|v| (o.field.to_string(), v.trim().to_string()))).collect()
}

/// The submitted value of `field` (`None` when not submitted).
fn value<'a>(values: &'a [(String, String)], field: &str) -> Option<&'a str> {
    values.iter().find(|(f, _)| f == field).map(|(_, v)| v.as_str())
}

/// `target` with the submitted options in its query (replacing what the
/// typed target says about them). Values outside the choices go in as typed,
/// so validation reports them. SNMP protocols are only written for v3, and
/// the Tailscale ping type only when it isn't the default.
pub fn merge(kind: MonitorType, target: &str, values: &[(String, String)]) -> String {
    let opts = opts(kind);
    if opts.is_empty() || values.is_empty() {
        return target.to_string();
    }
    let (base, segments) = split_query(target.trim());
    let managed = |key: &str| opts.iter().any(|o| o.key == key && value(values, o.field).is_some());
    let mut out: Vec<String> = Vec::new();
    let v3 = value(values, "snmp_version").is_some_and(|v| canonical("version", v) == "3");
    for o in opts {
        let Some(v) = value(values, o.field) else { continue };
        let v = canonical(o.key, v);
        let write = match o.key {
            "auth" | "priv" => v3,
            "type" => v != o.default,
            _ => true,
        };
        if write && !v.is_empty() {
            out.push(format!("{}={}", o.key, url::form_urlencoded::byte_serialize(v.as_bytes()).collect::<String>()));
        }
    }
    out.extend(segments.into_iter().filter(|s| !managed(&query_key(s))).map(str::to_string));
    join(base, &out)
}

/// `target` without the query keys in `keys` (an SNMPv3 protocol whose
/// password isn't set).
pub fn drop_keys(target: &str, keys: &[&str]) -> String {
    let (base, segments) = split_query(target);
    let kept: Vec<String> = segments.into_iter().filter(|s| !keys.contains(&query_key(s).as_str())).map(str::to_string).collect();
    join(base, &kept)
}

/// SNMP after the credential fields are settled: v1/v2c only use the
/// community (the secret), so the v3 user and password go; v3 needs a user,
/// a privacy password only with an authentication password, passwords of
/// [`MIN_PASSWORD_LEN`](checkers::snmp::MIN_PASSWORD_LEN) characters, and
/// keeps a protocol in the target only when its password is set.
pub fn snmp_security(m: &mut NewMonitor, e: &mut Errors) {
    let Some(opts) = checkers::endpoint::parse(MonitorType::Snmp, &m.target).ok().and_then(|ep| checkers::snmp::options(&ep).ok()) else {
        return; // the target's error is reported already
    };
    if opts.version != checkers::snmp::Version::V3 {
        m.basic_auth_user = None;
        m.basic_auth_pass = None;
        return;
    }
    if m.basic_auth_user.is_none() {
        e.insert("basic_auth_user", crate::i18n::t("validation.snmp_user").into());
    }
    if m.secret.is_some() && m.basic_auth_pass.is_none() {
        e.insert("secret", crate::i18n::t("validation.snmp_priv_needs_auth").into());
    }
    let min = checkers::snmp::MIN_PASSWORD_LEN;
    for (field, pw) in [("basic_auth_pass", &m.basic_auth_pass), ("secret", &m.secret)] {
        if pw.as_deref().is_some_and(|pw| pw.chars().count() < min) {
            e.insert(field, crate::i18n::tf("validation.snmp_password_short", &[("min", &min.to_string())]));
        }
    }
    let unused: &[&str] = match (&m.basic_auth_pass, &m.secret) {
        (None, _) => &["auth", "priv"],
        (Some(_), None) => &["priv"],
        _ => &[],
    };
    m.target = drop_keys(&m.target, unused);
}

impl super::FormValues {
    /// A target option's value for the form (`snmp_version`, `snmp_auth`,
    /// `snmp_priv`, `tailscale_type`); its default when unset.
    pub fn opt(&self, field: &str) -> &str {
        value(&self.target_opts, field).unwrap_or_else(|| {
            [SNMP, TAILSCALE].iter().flat_map(|o| o.iter()).find(|o| o.field == field).map_or("", |o| o.default)
        })
    }
}

impl FormData {
    pub fn snmp_auths(&self) -> &'static [(&'static str, &'static str)] {
        SNMP_AUTHS
    }

    pub fn snmp_privs(&self) -> &'static [(&'static str, &'static str)] {
        SNMP_PRIVS
    }

    /// Whether the target option `field` has the value `value`.
    pub fn opt_is(&self, field: &str, value: &str) -> bool {
        self.v.opt(field) == value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn stored_targets_split_into_target_and_choices() {
        let (t, o) = split(MonitorType::Snmp, "snmp://r.local/1.3.6.1.2.1.1.5.0?version=3&auth=SHA-256&priv=aes&context=vlan5");
        assert_eq!(t, "snmp://r.local/1.3.6.1.2.1.1.5.0?context=vlan5");
        assert_eq!(o, pairs(&[("snmp_version", "3"), ("snmp_auth", "sha256"), ("snmp_priv", "aes")]));
        let (t, o) = split(MonitorType::Snmp, "router");
        assert_eq!((t.as_str(), o[0].1.as_str(), o[1].1.as_str()), ("router", "2c", "sha"));
        // Unknown values stay visible in the target.
        let (t, o) = split(MonitorType::Snmp, "router?version=4");
        assert_eq!((t.as_str(), o[0].1.as_str()), ("router?version=4", "2c"));
        let (t, o) = split(MonitorType::TailscalePing, "nas?type=TSMP");
        assert_eq!((t, o), ("nas".to_string(), pairs(&[("tailscale_type", "tsmp")])));
        assert_eq!(split(MonitorType::Redis, "redis://x?db=1"), ("redis://x?db=1".to_string(), Vec::new()));
    }

    #[test]
    fn choices_merge_back_into_the_target() {
        let v3 = pairs(&[("snmp_version", "3"), ("snmp_auth", "sha512"), ("snmp_priv", "aes256")]);
        assert_eq!(merge(MonitorType::Snmp, "r.local?context=x", &v3), "r.local?version=3&auth=sha512&priv=aes256&context=x");
        // The choices replace what the typed target says.
        assert_eq!(merge(MonitorType::Snmp, "r.local?version=1&auth=md5", &v3), "r.local?version=3&auth=sha512&priv=aes256");
        let v2 = pairs(&[("snmp_version", "2c"), ("snmp_auth", "sha512"), ("snmp_priv", "aes256")]);
        assert_eq!(merge(MonitorType::Snmp, "r.local/1.3.6.1.2.1.1.3.0", &v2), "r.local/1.3.6.1.2.1.1.3.0?version=2c");
        // Nothing submitted (API-style posts): the target as typed.
        assert_eq!(merge(MonitorType::Snmp, "r.local?version=3&auth=md5", &[]), "r.local?version=3&auth=md5");
        assert_eq!(merge(MonitorType::TailscalePing, "nas", &pairs(&[("tailscale_type", "disco")])), "nas");
        assert_eq!(merge(MonitorType::TailscalePing, "nas?type=icmp", &pairs(&[("tailscale_type", "tsmp")])), "nas?type=tsmp");
        assert_eq!(drop_keys("r?version=3&auth=sha&priv=aes&context=x", &["priv"]), "r?version=3&auth=sha&context=x");
        assert_eq!(drop_keys("r?auth=sha", &["auth", "priv"]), "r");
    }

    fn submit(base: &NewMonitor, list: &[(&str, &str)]) -> Result<super::super::Validated, Errors> {
        let mut pairs: Vec<(String, String)> =
            [("name", "Mon"), ("interval_seconds", "60"), ("timeout_seconds", "10"), ("retries", "0"), ("resend_interval", "0"), ("escalation_minutes", "0")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        pairs.extend(list.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let v = super::super::FormValues::from_pairs(&Pairs(pairs), &[]);
        super::super::validate(&v, base, &super::super::FormContext::default())
    }

    #[test]
    fn snmpv3_form_stores_the_choices_and_credentials() {
        let new = NewMonitor::default();
        let ok = submit(
            &new,
            &[
                ("monitor_type", "snmp"),
                ("target_snmp", "core.local/1.3.6.1.2.1.1.5.0"),
                ("snmp_version", "3"),
                ("snmp_auth", "sha256"),
                ("snmp_priv", "aes256"),
                ("basic_auth_user", "watcher"),
                ("basic_auth_pass", "authpass123"),
                ("secret", "privpass123"),
            ],
        )
        .unwrap()
        .monitor;
        assert_eq!(ok.target, "core.local/1.3.6.1.2.1.1.5.0?version=3&auth=sha256&priv=aes256");
        assert_eq!((ok.basic_auth_user.as_deref(), ok.basic_auth_pass.as_deref(), ok.secret.as_deref()), (Some("watcher"), Some("authpass123"), Some("privpass123")));

        // authNoPriv keeps only the authentication protocol; noAuthNoPriv neither.
        let auth_only = [("monitor_type", "snmp"), ("target_snmp", "core.local"), ("snmp_version", "3"), ("snmp_auth", "md5"), ("snmp_priv", "des"), ("basic_auth_user", "w"), ("basic_auth_pass", "authpass123")];
        assert_eq!(submit(&new, &auth_only).unwrap().monitor.target, "core.local?version=3&auth=md5");
        let none = [("monitor_type", "snmp"), ("target_snmp", "core.local"), ("snmp_version", "3"), ("snmp_auth", "md5"), ("snmp_priv", "des"), ("basic_auth_user", "w")];
        assert_eq!(submit(&new, &none).unwrap().monitor.target, "core.local?version=3");

        // v2c drops the v3 credentials and keeps the community.
        let v2 = submit(&new, &[("monitor_type", "snmp"), ("target_snmp", "core.local"), ("snmp_version", "2c"), ("basic_auth_user", "w"), ("secret", "private")]).unwrap().monitor;
        assert_eq!((v2.target.as_str(), v2.basic_auth_user.as_deref(), v2.secret.as_deref()), ("core.local?version=2c", None, Some("private")));

        // The edit form shows the target without the choices.
        let shown = super::super::FormValues::from_new(&ok, Vec::new(), &[]);
        assert_eq!((shown.target.as_str(), shown.opt("snmp_version"), shown.opt("snmp_auth"), shown.opt("snmp_priv")), ("core.local/1.3.6.1.2.1.1.5.0", "3", "sha256", "aes256"));
    }

    #[test]
    fn snmpv3_form_errors_and_secret_reentry() {
        let new = NewMonitor::default();
        let v3 = |extra: &[(&'static str, &'static str)]| {
            let mut list = vec![("monitor_type", "snmp"), ("target_snmp", "core.local"), ("snmp_version", "3"), ("snmp_auth", "sha"), ("snmp_priv", "aes")];
            list.extend_from_slice(extra);
            list
        };
        let e = submit(&new, &v3(&[])).unwrap_err();
        assert!(e.contains_key("basic_auth_user"), "{e:?}");
        let e = submit(&new, &v3(&[("basic_auth_user", "w"), ("secret", "privpass123")])).unwrap_err();
        assert!(e.contains_key("secret"), "{e:?}");
        let e = submit(&new, &v3(&[("basic_auth_user", "w"), ("basic_auth_pass", "short")])).unwrap_err();
        assert!(e.contains_key("basic_auth_pass"), "{e:?}");
        let e = submit(&new, &[("monitor_type", "snmp"), ("target_snmp", "core.local?version=4")]).unwrap_err();
        assert!(e.contains_key("target"), "{e:?}");

        // A stored v3 monitor: the masked passwords stay while the device stays…
        let stored = NewMonitor {
            monitor_type: MonitorType::Snmp,
            target: "core.local?version=3&auth=sha&priv=aes".into(),
            basic_auth_user: Some("w".into()),
            basic_auth_pass: Some("authpass123".into()),
            secret: Some("privpass123".into()),
            ..NewMonitor::default()
        };
        let mask = crate::notification::SECRET_MASK;
        let kept = submit(&stored, &v3(&[("basic_auth_user", "w"), ("basic_auth_pass", mask), ("secret", mask)])).unwrap().monitor;
        assert_eq!((kept.basic_auth_pass.as_deref(), kept.secret.as_deref()), (Some("authpass123"), Some("privpass123")));
        // …but not for another host, nor when going back to v2c (the community travels in the clear).
        let moved = [("monitor_type", "snmp"), ("target_snmp", "evil.example"), ("snmp_version", "3"), ("snmp_auth", "sha"), ("snmp_priv", "aes"), ("basic_auth_user", "w"), ("basic_auth_pass", mask), ("secret", mask)];
        assert!(submit(&stored, &moved).unwrap_err().contains_key("basic_auth_pass"));
        let downgraded = [("monitor_type", "snmp"), ("target_snmp", "core.local"), ("snmp_version", "2c"), ("secret", mask)];
        assert!(submit(&stored, &downgraded).unwrap_err().contains_key("secret"));
    }

    #[test]
    fn tailscale_form_stores_the_ping_type() {
        let new = NewMonitor::default();
        let m = submit(&new, &[("monitor_type", "tailscale_ping"), ("target_tailscale", "nas"), ("tailscale_type", "tsmp")]).unwrap().monitor;
        assert_eq!((m.monitor_type, m.target.as_str()), (MonitorType::TailscalePing, "nas?type=tsmp"));
        let m = submit(&new, &[("monitor_type", "tailscale_ping"), ("target_tailscale", "100.64.0.5"), ("tailscale_type", "disco")]).unwrap().monitor;
        assert_eq!(m.target, "100.64.0.5");
        let e = submit(&new, &[("monitor_type", "tailscale_ping"), ("target_tailscale", "https://nas")]).unwrap_err();
        assert!(e.contains_key("target"), "{e:?}");
        let shown = super::super::FormValues::from_new(&NewMonitor { monitor_type: MonitorType::TailscalePing, target: "nas?type=icmp".into(), ..NewMonitor::default() }, Vec::new(), &[]);
        assert_eq!((shown.target.as_str(), shown.opt("tailscale_type")), ("nas", "icmp"));
    }

    #[test]
    fn split_then_merge_keeps_the_meaning() {
        for target in ["snmp://r.local:1161/1.3.6.1.2.1.1.1.0?version=3&auth=md5&priv=des", "r.local?version=1", "r.local?version=2c&context=a%20b"] {
            let (t, o) = split(MonitorType::Snmp, target);
            assert_eq!(merge(MonitorType::Snmp, &t, &o), target, "{target}");
        }
    }
}
