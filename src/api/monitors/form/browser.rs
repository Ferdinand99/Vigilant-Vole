//! The real-browser part of the monitor form: page checks, headers and
//! basic auth, waiting and viewport, and the remote browser to use.

use super::{Errors, FormContext, FormValues, first_line, non_empty, resolve_secret, same_destination};
use crate::db::models::NewMonitor;
use crate::monitor::checkers::{self, real_browser};

/// Longest accepted CSS selector.
pub const MAX_SELECTOR_LEN: usize = 1_024;

pub(super) fn validate(v: &FormValues, base: &NewMonitor, cx: &FormContext, m: &mut NewMonitor, e: &mut Errors) {
    let codes = v.accepted_status_codes.trim();
    match checkers::parse_status_codes(codes) {
        Ok(_) => m.accepted_status_codes = if codes.is_empty() { "200-399".into() } else { codes.to_string() },
        Err(msg) => {
            e.insert("accepted_status_codes", format!("{} {}", first_line(&msg), crate::i18n::t("validation.codes_hint")));
        }
    }
    m.http_headers = match non_empty(&v.http_headers) {
        None => None,
        Some(h) => match checkers::parse_headers(&h) {
            Ok(_) => Some(h),
            Err(msg) => {
                e.insert("http_headers", first_line(&msg));
                None
            }
        },
    };
    m.ignore_tls = v.ignore_tls;
    // A browser goes direct; HTTP request options don't apply.
    m.proxy_url = None;

    // Basic auth only (sent as an Authorization header).
    m.oauth_token_url = None;
    m.oauth_client_id = None;
    m.oauth_client_secret = None;
    m.oauth_scope = None;
    m.oauth_audience = None;
    m.oauth_client_auth = "client_secret_basic".into();
    m.tls_client_cert = None;
    m.tls_client_key = None;
    m.tls_ca_cert = None;
    m.basic_auth_user = non_empty(&v.basic_auth_user);
    let unchanged = same_destination(base, m);
    m.basic_auth_pass = resolve_secret(e, "basic_auth_pass", &v.basic_auth_pass, base.basic_auth_pass.as_ref(), unchanged, "validation.basic_pass_reenter");
    if m.basic_auth_user.is_none() && m.basic_auth_pass.is_some() {
        e.insert("basic_auth_user", crate::i18n::t("validation.basic_user").into());
    }
    m.auth_method = if m.basic_auth_user.is_some() { "basic" } else { "none" }.into();

    // What the page must show: a keyword or a CSS selector (both optional).
    match v.browser_check.trim() {
        "selector" => {
            m.keyword = None;
            m.browser_selector = non_empty(&v.browser_selector);
            match &m.browser_selector {
                None => {
                    e.insert("browser_selector", crate::i18n::t("validation.browser_selector").into());
                }
                Some(s) if s.chars().count() > MAX_SELECTOR_LEN => {
                    e.insert("browser_selector", crate::i18n::tf("validation.browser_selector_long", &[("max", &MAX_SELECTOR_LEN.to_string())]));
                }
                Some(_) => {}
            }
        }
        "keyword" | "" => {
            m.browser_selector = None;
            m.keyword = (!v.keyword.trim().is_empty()).then(|| v.keyword.clone());
        }
        _ => {
            e.insert("browser_check", crate::i18n::t("validation.browser_check").into());
        }
    }
    m.keyword_invert = v.keyword_invert;

    match v.browser_wait.trim() {
        "" => {}
        w if real_browser::WAITS.contains(&w) => m.browser_wait = w.to_string(),
        _ => {
            e.insert("browser_wait", crate::i18n::t("validation.browser_wait").into());
        }
    }
    match v.browser_viewport.trim() {
        "" => {}
        w if real_browser::VIEWPORTS.contains(&w) => m.browser_viewport = w.to_string(),
        _ => {
            e.insert("browser_viewport", crate::i18n::t("validation.browser_viewport").into());
        }
    }
    m.browser_js_errors = v.browser_js_errors;

    m.remote_browser_id = match v.remote_browser_id.trim() {
        "" => match cx.remote_browser_ids.first() {
            Some(id) => Some(*id),
            None => {
                if !cx.browser_optional {
                    e.insert("remote_browser_id", crate::i18n::t("validation.browser_none").into());
                }
                None
            }
        },
        raw => match raw.parse::<i64>() {
            Ok(id) if cx.remote_browser_ids.contains(&id) => Some(id),
            _ => {
                e.insert("remote_browser_id", crate::i18n::t("validation.browser_unknown").into());
                None
            }
        },
    };
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use crate::db::models::MonitorType;

    fn browser_pairs(extra: &[(&str, &str)]) -> Pairs {
        let mut list: Vec<(String, String)> = [
            ("name", "Shop"),
            ("monitor_type", "real_browser"),
            ("target_browser", "https://shop.example.com/"),
            ("interval_seconds", "60"),
            ("timeout_seconds", "30"),
            ("retries", "0"),
            ("resend_interval", "0"),
            ("escalation_minutes", "0"),
            ("accepted_status_codes", "200-299"),
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

    fn cx(ids: &[i64]) -> FormContext {
        FormContext { remote_browser_ids: ids.to_vec(), ..FormContext::default() }
    }

    fn run(extra: &[(&str, &str)], cx: &FormContext) -> Result<Validated, Errors> {
        validate(&FormValues::from_pairs(&browser_pairs(extra), &[]), &NewMonitor::default(), cx)
    }

    #[test]
    fn a_real_browser_monitor_defaults_to_the_first_remote_browser() {
        let ok = run(&[("keyword", "Add to cart"), ("browser_check", "keyword")], &cx(&[4, 9])).unwrap().monitor;
        assert_eq!(ok.monitor_type, MonitorType::RealBrowser);
        assert_eq!(ok.target, "https://shop.example.com/");
        assert_eq!(ok.remote_browser_id, Some(4));
        assert_eq!((ok.keyword.as_deref(), ok.browser_selector.as_deref()), (Some("Add to cart"), None));
        assert_eq!((ok.browser_wait.as_str(), ok.browser_viewport.as_str(), ok.browser_js_errors), ("load", "desktop", false));
        assert_eq!(ok.auth_method, "none");
    }

    #[test]
    fn remote_browser_is_required_and_must_exist() {
        let errors = run(&[], &cx(&[])).unwrap_err();
        assert!(errors.contains_key("remote_browser_id"), "{errors:?}");
        let errors = run(&[("remote_browser_id", "7")], &cx(&[4])).unwrap_err();
        assert!(errors.contains_key("remote_browser_id"));
        assert_eq!(run(&[("remote_browser_id", "9")], &cx(&[4, 9])).unwrap().monitor.remote_browser_id, Some(9));
        // Imports may leave it unset when there is none yet.
        let optional = FormContext { browser_optional: true, ..FormContext::default() };
        assert_eq!(run(&[], &optional).unwrap().monitor.remote_browser_id, None);
    }

    #[test]
    fn selector_mode_options_and_auth_are_validated() {
        let ok = run(
            &[
                ("browser_check", "selector"),
                ("browser_selector", " #app .ready "),
                ("keyword", "ignored"),
                ("keyword_invert", "true"),
                ("browser_wait", "network_idle"),
                ("browser_viewport", "mobile"),
                ("browser_js_errors", "true"),
                ("http_headers", r#"{"X-Probe":"1"}"#),
                ("basic_auth_user", "bot"),
                ("basic_auth_pass", "pw"),
            ],
            &cx(&[1]),
        )
        .unwrap()
        .monitor;
        assert_eq!((ok.browser_selector.as_deref(), ok.keyword.as_deref(), ok.keyword_invert), (Some("#app .ready"), None, true));
        assert_eq!((ok.browser_wait.as_str(), ok.browser_viewport.as_str(), ok.browser_js_errors), ("network_idle", "mobile", true));
        assert_eq!((ok.auth_method.as_str(), ok.basic_auth_pass.as_deref()), ("basic", Some("pw")));
        assert_eq!(ok.http_headers.as_deref(), Some(r#"{"X-Probe":"1"}"#));

        let errors = run(
            &[
                ("browser_check", "selector"),
                ("browser_wait", "forever"),
                ("browser_viewport", "watch"),
                ("accepted_status_codes", "abc"),
                ("http_headers", "[1]"),
                ("basic_auth_pass", "pw"),
            ],
            &cx(&[1]),
        )
        .unwrap_err();
        for field in ["browser_selector", "browser_wait", "browser_viewport", "accepted_status_codes", "http_headers", "basic_auth_user"] {
            assert!(errors.contains_key(field), "{field}: {errors:?}");
        }
        let errors = run(&[("target_browser", "ftp://x"), ("browser_check", "xpath")], &cx(&[1])).unwrap_err();
        assert!(errors.contains_key("target") && errors.contains_key("browser_check"), "{errors:?}");
    }

    #[test]
    fn stored_password_is_only_kept_for_the_same_site() {
        let stored = NewMonitor {
            monitor_type: MonitorType::RealBrowser,
            target: "https://shop.example.com/".into(),
            auth_method: "basic".into(),
            basic_auth_user: Some("bot".into()),
            basic_auth_pass: Some("secret".into()),
            remote_browser_id: Some(1),
            ..NewMonitor::default()
        };
        let mask = crate::notification::SECRET_MASK;
        let same = browser_pairs(&[("basic_auth_user", "bot"), ("basic_auth_pass", mask)]);
        let ok = validate(&FormValues::from_pairs(&same, &[]), &stored, &cx(&[1])).unwrap();
        assert_eq!(ok.monitor.basic_auth_pass.as_deref(), Some("secret"));
        let moved = browser_pairs(&[("target_browser", "https://evil.example/"), ("basic_auth_user", "bot"), ("basic_auth_pass", mask)]);
        let errors = validate(&FormValues::from_pairs(&moved, &[]), &stored, &cx(&[1])).unwrap_err();
        assert!(errors.contains_key("basic_auth_pass"));
    }

    #[test]
    fn browser_settings_round_trip_through_the_form() {
        let m = NewMonitor {
            name: "Shop".into(),
            monitor_type: MonitorType::RealBrowser,
            target: "https://shop.example.com/".into(),
            browser_selector: Some("#app".into()),
            browser_wait: "network_idle".into(),
            browser_viewport: "mobile".into(),
            browser_js_errors: true,
            remote_browser_id: Some(3),
            ..NewMonitor::default()
        };
        let v = FormValues::from_new(&m, Vec::new(), &[]);
        assert_eq!((v.browser_check.as_str(), v.remote_browser_id.as_str()), ("selector", "3"));
        let back = validate(&v, &m, &cx(&[3])).unwrap().monitor;
        assert_eq!(back, m);
    }
}
