//! Provider form schemas: what each channel kind needs, how its form is parsed
//! into the stored JSON config, and how that config is validated, redacted and
//! merged on edit. Rendering is generic (see `api::notifications`), so adding a
//! provider never needs a new template.

use serde_json::{Map, Value};

use super::http::OutRequest;
use crate::kit::notify::alert::NotificationEvent;

/// Shown in place of a saved secret. Submitting it back (or leaving the field
/// empty) keeps the stored value.
pub const SECRET_MASK: &str = "••••••";

/// Form field name prefix, so provider keys never collide with `name`, `kind`…
pub const FIELD_PREFIX: &str = "c_";

/// Builds the outgoing HTTP requests for one event. An empty list means "this
/// provider has nothing to send for this event" (e.g. PagerDuty and maintenance).
pub type BuildFn = fn(&Value, &NotificationEvent) -> Result<Vec<OutRequest>, String>;

/// Extra provider-specific validation on top of the schema's generic checks.
pub type CheckFn = fn(&Value) -> Vec<FieldError>;

#[derive(Clone, Copy)]
pub enum Transport {
    Http(BuildFn),
    Smtp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Category {
    Chat,
    Push,
    Messaging,
    Incident,
    Email,
    Automation,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Chat,
        Category::Push,
        Category::Messaging,
        Category::Email,
        Category::Incident,
        Category::Automation,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::Chat => crate::kit::i18n::t("notif.cat_chat"),
            Category::Push => crate::kit::i18n::t("notif.cat_push"),
            Category::Messaging => crate::kit::i18n::t("notif.cat_messaging"),
            Category::Incident => crate::kit::i18n::t("notif.cat_incident"),
            Category::Email => crate::kit::i18n::t("notif.cat_email"),
            Category::Automation => crate::kit::i18n::t("notif.cat_automation"),
        }
    }
}

pub struct ProviderSchema {
    pub kind: &'static str,
    pub label: &'static str,
    /// Icon name from `static/icons.svg`.
    pub icon: &'static str,
    pub category: Category,
    /// One line for the provider picker.
    pub description: &'static str,
    pub docs_url: &'static str,
    pub fields: &'static [Field],
    pub transport: Transport,
    pub check: Option<CheckFn>,
}

impl ProviderSchema {
    pub fn field(&self, key: &str) -> Option<&'static Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// Display name in the current language (brand names stay as they are).
    pub fn tr_label(&self) -> &'static str {
        if self.kind == "email" {
            crate::kit::i18n::t("notif.email_label")
        } else {
            self.label
        }
    }

    /// Picker description in the current language (`notif.<kind>.desc`).
    pub fn tr_description(&self) -> &'static str {
        crate::kit::i18n::t_or(&format!("notif.{}.desc", self.kind), self.description)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    Text,
    Url,
    /// Password-style input; always secret.
    Secret,
    /// Whole number.
    Number,
    /// `(value, label)` options.
    Select(&'static [(&'static str, &'static str)]),
    Bool,
    Textarea,
    /// String → string map (e.g. HTTP headers), stored as a JSON object.
    KeyValue,
}

#[derive(Debug, Clone, Copy)]
pub struct Field {
    pub key: &'static str,
    pub label: &'static str,
    pub kind: FieldKind,
    pub required: bool,
    /// Never echoed back to the browser; empty on save keeps the stored value.
    pub secret: bool,
    pub placeholder: &'static str,
    pub help: &'static str,
    /// Pre-filled for new channels ("true" for a Bool that starts on).
    pub default: &'static str,
    /// `data-show-when` rule using provider keys, e.g. `"security=tls,starttls"`.
    pub show_when: &'static str,
    /// Decides where the stored secrets are sent (a server URL, SMTP host…).
    /// Changing it means saved secrets aren't reused: they must be retyped.
    pub destination: bool,
}

impl Field {
    pub const fn new(key: &'static str, label: &'static str, kind: FieldKind) -> Self {
        Field {
            key,
            label,
            kind,
            required: false,
            secret: matches!(kind, FieldKind::Secret),
            placeholder: "",
            help: "",
            default: "",
            show_when: "",
            destination: matches!(kind, FieldKind::Url),
        }
    }
    pub const fn text(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Text)
    }
    pub const fn url(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Url)
    }
    pub const fn secret(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Secret)
    }
    pub const fn number(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Number)
    }
    pub const fn select(
        key: &'static str,
        label: &'static str,
        options: &'static [(&'static str, &'static str)],
    ) -> Self {
        Self::new(key, label, FieldKind::Select(options))
    }
    pub const fn boolean(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Bool)
    }
    pub const fn textarea(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::Textarea)
    }
    pub const fn key_value(key: &'static str, label: &'static str) -> Self {
        Self::new(key, label, FieldKind::KeyValue)
    }

    pub const fn required(mut self) -> Self {
        self.required = true;
        self
    }
    /// Marks a non-password field (a webhook URL with a token in it, headers) as secret.
    pub const fn sensitive(mut self) -> Self {
        self.secret = true;
        self
    }
    pub const fn placeholder(mut self, p: &'static str) -> Self {
        self.placeholder = p;
        self
    }
    pub const fn help(mut self, h: &'static str) -> Self {
        self.help = h;
        self
    }
    pub const fn default(mut self, d: &'static str) -> Self {
        self.default = d;
        self
    }
    pub const fn show_when(mut self, rule: &'static str) -> Self {
        self.show_when = rule;
        self
    }
    /// Marks a non-URL field (SMTP host, account id) as deciding where secrets go.
    pub const fn destination(mut self) -> Self {
        self.destination = true;
        self
    }
    /// A URL that's only displayed (avatar, icon) — secrets never go there.
    pub const fn cosmetic(mut self) -> Self {
        self.destination = false;
        self
    }

    /// Name of the form control(s) for this field.
    pub fn form_name(&self) -> String {
        format!("{FIELD_PREFIX}{}", self.key)
    }

    /// Label in the current language (`notif.<kind>.<key>.label`).
    pub fn tr_label(&self, kind: &str) -> &'static str {
        crate::kit::i18n::t_or(&format!("notif.{kind}.{}.label", self.key), self.label)
    }

    /// Help text in the current language (`notif.<kind>.<key>.help`).
    pub fn tr_help(&self, kind: &str) -> &'static str {
        if self.help.is_empty() {
            return "";
        }
        crate::kit::i18n::t_or(&format!("notif.{kind}.{}.help", self.key), self.help)
    }

    /// "Enter the SMTP server" (`notif.<kind>.<key>.required`); falls back
    /// to a message built from the label for fields without one.
    pub fn tr_required(&self, kind: &str) -> String {
        let key = format!("notif.{kind}.{}.required", self.key);
        let explicit = crate::kit::i18n::t_or(&key, "");
        if explicit.is_empty() {
            crate::kit::i18n::tf(
                "notif.err_required",
                &[("field", &self.tr_label(kind).to_lowercase())],
            )
        } else {
            explicit.to_string()
        }
    }

    /// Placeholder in the current language (most are untranslated examples).
    pub fn tr_placeholder(&self, kind: &str) -> &'static str {
        crate::kit::i18n::t_or(
            &format!("notif.{kind}.{}.placeholder", self.key),
            self.placeholder,
        )
    }

    /// A select option's label in the current language.
    pub fn tr_option(&self, kind: &str, value: &str, label: &'static str) -> &'static str {
        crate::kit::i18n::t_or(&format!("notif.{kind}.{}.opt_{value}", self.key), label)
    }
}

/// `(field key, message)`; key `""` is a form-level error.
pub type FieldError = (String, String);

pub fn field_error(key: &str, msg: impl Into<String>) -> FieldError {
    (key.to_string(), msg.into())
}

/// A submitted provider form, parsed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ParsedForm {
    pub config: Value,
    /// Secret keys the user explicitly asked to remove.
    pub clear: Vec<String>,
}

/// Turns submitted `(name, value)` pairs into a typed config object. Empty
/// values are left out, numbers become JSON numbers (or stay text so
/// validation can flag them), switches become booleans.
pub fn parse_form(schema: &ProviderSchema, pairs: &[(String, String)]) -> ParsedForm {
    let get = |name: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let mut map = Map::new();
    let mut clear = Vec::new();
    for field in schema.fields {
        let name = field.form_name();
        if get(&format!("{name}__clear")) == Some("true") {
            clear.push(field.key.to_string());
        }
        match field.kind {
            FieldKind::Bool => {
                map.insert(
                    field.key.into(),
                    Value::Bool(get(&name).is_some_and(|v| v == "true" || v == "on")),
                );
            }
            FieldKind::KeyValue => {
                let keys = pairs
                    .iter()
                    .filter(|(k, _)| *k == format!("{name}__k"))
                    .map(|(_, v)| v);
                let values = pairs
                    .iter()
                    .filter(|(k, _)| *k == format!("{name}__v"))
                    .map(|(_, v)| v);
                let mut obj = Map::new();
                for (k, v) in keys.zip(values) {
                    let k = k.trim();
                    if !k.is_empty() {
                        obj.insert(k.to_string(), Value::String(v.trim().to_string()));
                    }
                }
                // No-JS fallback / API: a JSON object in the plain field.
                if obj.is_empty()
                    && let Some(Ok(Value::Object(o))) =
                        get(&name).map(serde_json::from_str::<Value>)
                {
                    obj = o;
                }
                if !obj.is_empty() {
                    map.insert(field.key.into(), Value::Object(obj));
                }
            }
            FieldKind::Number => {
                if let Some(v) = get(&name).map(str::trim).filter(|v| !v.is_empty()) {
                    let value = v
                        .parse::<i64>()
                        .map(Value::from)
                        .unwrap_or_else(|_| Value::String(v.into()));
                    map.insert(field.key.into(), value);
                }
            }
            FieldKind::Textarea => {
                if let Some(v) = get(&name)
                    .map(str::trim_end)
                    .filter(|v| !v.trim().is_empty())
                {
                    map.insert(field.key.into(), Value::String(v.replace("\r\n", "\n")));
                }
            }
            _ => {
                if let Some(v) = get(&name).map(str::trim).filter(|v| !v.is_empty()) {
                    map.insert(field.key.into(), Value::String(v.into()));
                }
            }
        }
    }
    ParsedForm {
        config: Value::Object(map),
        clear,
    }
}

fn is_blank(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.trim().is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        _ => false,
    }
}

/// Keeps stored secrets the user didn't retype: an empty or masked secret
/// field takes the old value, unless its key is in `clear`. For secret
/// key–value maps, masked values take the old value of the same key.
pub fn merge_secrets(schema: &ProviderSchema, new: &mut Value, old: &Value, clear: &[String]) {
    let Some(obj) = new.as_object_mut() else {
        return;
    };
    for field in schema.fields.iter().filter(|f| f.secret) {
        if clear.iter().any(|k| k == field.key) {
            obj.remove(field.key);
            continue;
        }
        let old_value = old.get(field.key);
        if field.kind == FieldKind::KeyValue {
            let Some(Value::Object(map)) = obj.get_mut(field.key) else {
                continue;
            };
            map.retain(|k, v| {
                if v.as_str() != Some(SECRET_MASK) {
                    return true;
                }
                match old_value.and_then(|o| o.get(k)) {
                    Some(prev) => {
                        *v = prev.clone();
                        true
                    }
                    None => false,
                }
            });
            continue;
        }
        let current = obj.get(field.key);
        let masked = current.and_then(Value::as_str) == Some(SECRET_MASK);
        if (is_blank(current) || masked) && !is_blank(old_value) {
            if let Some(prev) = old_value {
                obj.insert(field.key.into(), prev.clone());
            }
        } else if masked {
            obj.remove(field.key);
        }
    }
}

fn is_masked(v: Option<&Value>) -> bool {
    v.and_then(Value::as_str) == Some(SECRET_MASK)
}

/// Comparable form of a destination value: blank → `None`, strings trimmed.
fn destination_value(v: Option<&Value>) -> Option<String> {
    match v {
        _ if is_blank(v) => None,
        Some(Value::String(s)) => Some(s.trim().to_string()),
        Some(other) => Some(other.to_string()),
        None => None,
    }
}

/// Whether `new` sends to a different place than `old` (any destination field
/// differs). A blank or masked *secret* destination keeps the stored value, so
/// it counts as unchanged.
pub fn destination_changed(schema: &ProviderSchema, new: &Value, old: &Value) -> bool {
    schema.fields.iter().filter(|f| f.destination).any(|field| {
        let current = new.get(field.key);
        if field.secret && (is_blank(current) || is_masked(current)) {
            return false;
        }
        destination_value(current) != destination_value(old.get(field.key))
    })
}

/// Secret fields whose stored value [`merge_secrets`] would restore.
fn restored_secrets(
    schema: &ProviderSchema,
    new: &Value,
    old: &Value,
    clear: &[String],
) -> Vec<&'static str> {
    schema
        .fields
        .iter()
        .filter(|f| f.secret && !clear.iter().any(|k| k == f.key))
        .filter(|field| {
            let current = new.get(field.key);
            let old_value = old.get(field.key);
            if field.kind == FieldKind::KeyValue {
                return current.and_then(Value::as_object).is_some_and(|map| {
                    map.iter().any(|(k, v)| {
                        v.as_str() == Some(SECRET_MASK)
                            && old_value.and_then(|o| o.get(k)).is_some()
                    })
                });
            }
            (is_blank(current) || is_masked(current)) && !is_blank(old_value)
        })
        .map(|f| f.key)
        .collect()
}

/// [`merge_secrets`], but stored secrets are only reused while the destination
/// stays the same. Otherwise anyone who may edit a channel could point it at
/// their own server and "send a test" to read the saved token. Each secret
/// that would have been restored gets a field error asking to retype it.
pub fn merge_secrets_checked(
    schema: &ProviderSchema,
    new: &mut Value,
    old: &Value,
    clear: &[String],
) -> Result<(), Vec<FieldError>> {
    if destination_changed(schema, new, old) {
        let errors: Vec<FieldError> = restored_secrets(schema, new, old, clear)
            .into_iter()
            .map(|key| field_error(key, crate::kit::i18n::t("notif.err_reenter_secret")))
            .collect();
        if !errors.is_empty() {
            return Err(errors);
        }
    }
    merge_secrets(schema, new, old, clear);
    Ok(())
}

/// A copy of `config` that is safe to show anyone: secret values become
/// [`SECRET_MASK`]. Unknown kinds have every string masked.
pub fn redact(schema: Option<&ProviderSchema>, config: &Value) -> Value {
    let Some(obj) = config.as_object() else {
        return Value::Object(Map::new());
    };
    let mut out = Map::new();
    for (key, value) in obj {
        let secret = schema.is_none_or(|s| s.field(key).is_none_or(|f| f.secret));
        let shown = if !secret {
            value.clone()
        } else {
            match value {
                Value::Object(map) => Value::Object(
                    map.iter()
                        .map(|(k, _)| (k.clone(), Value::String(SECRET_MASK.into())))
                        .collect(),
                ),
                Value::String(s) if s.is_empty() => Value::String(String::new()),
                Value::Bool(_) | Value::Number(_) | Value::Null => value.clone(),
                _ => Value::String(SECRET_MASK.into()),
            }
        };
        out.insert(key.clone(), shown);
    }
    Value::Object(out)
}

/// Every non-empty secret string in a config — used to scrub error messages.
pub fn secret_values(schema: Option<&ProviderSchema>, config: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(obj) = config.as_object() else {
        return out;
    };
    for (key, value) in obj {
        if !schema.is_none_or(|s| s.field(key).is_none_or(|f| f.secret)) {
            continue;
        }
        match value {
            Value::String(s) if s.len() >= 4 => out.push(s.clone()),
            Value::Object(map) => out.extend(
                map.values()
                    .filter_map(Value::as_str)
                    .filter(|s| s.len() >= 4)
                    .map(str::to_string),
            ),
            _ => {}
        }
    }
    out
}

/// Generic checks from the schema, then the provider's own.
pub fn validate(schema: &ProviderSchema, config: &Value) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    for field in schema.fields {
        let value = config.get(field.key);
        if is_blank(value) {
            if field.required && field.kind != FieldKind::Bool {
                let msg = match field.kind {
                    FieldKind::Select(_) => crate::kit::i18n::t("notif.err_choose").to_string(),
                    _ => field.tr_required(schema.kind),
                };
                errors.push(field_error(field.key, msg));
            }
            continue;
        }
        let Some(value) = value else { continue };
        match field.kind {
            FieldKind::Url => {
                let ok = value.as_str().is_some_and(is_http_url);
                if !ok {
                    errors.push(field_error(field.key, crate::kit::i18n::t("notif.err_url")));
                }
            }
            FieldKind::Number => {
                if value.as_i64().is_none() {
                    errors.push(field_error(
                        field.key,
                        crate::kit::i18n::t("notif.err_number"),
                    ));
                }
            }
            FieldKind::Select(options) => {
                let ok = value
                    .as_str()
                    .is_some_and(|v| options.iter().any(|(o, _)| *o == v));
                if !ok {
                    errors.push(field_error(
                        field.key,
                        crate::kit::i18n::t("notif.err_option"),
                    ));
                }
            }
            _ => {}
        }
    }
    if let Some(check) = schema.check {
        for (key, msg) in check(config) {
            if !errors.iter().any(|(k, _)| *k == key) {
                errors.push((key, msg));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

pub fn is_http_url(s: &str) -> bool {
    url::Url::parse(s.trim())
        .is_ok_and(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const FIELDS: &[Field] = &[
        Field::url("url", "Webhook URL").required().sensitive(),
        Field::text("username", "Username"),
        Field::secret("token", "Token"),
        Field::number("priority", "Priority"),
        Field::select("mode", "Mode", &[("a", "A"), ("b", "B")]).default("a"),
        Field::boolean("silent", "Silent"),
        Field::key_value("headers", "Headers").sensitive(),
    ];

    fn no_build(_: &Value, _: &NotificationEvent) -> Result<Vec<OutRequest>, String> {
        Ok(Vec::new())
    }

    const SCHEMA: ProviderSchema = ProviderSchema {
        kind: "test",
        label: "Test",
        icon: "bell",
        category: Category::Automation,
        description: "",
        docs_url: "",
        fields: FIELDS,
        transport: Transport::Http(no_build),
        check: None,
    };

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parse_form_types_values_and_drops_empties() {
        let form = parse_form(
            &SCHEMA,
            &pairs(&[
                ("c_url", " https://x.test/hook "),
                ("c_username", ""),
                ("c_priority", "5"),
                ("c_mode", "b"),
                ("c_silent", "true"),
                ("c_headers__k", "X-A"),
                ("c_headers__v", "1"),
                ("c_headers__k", ""),
                ("c_headers__v", "ignored"),
                ("name", "not a provider field"),
            ]),
        );
        assert_eq!(
            form.config,
            json!({"url": "https://x.test/hook", "priority": 5, "mode": "b", "silent": true, "headers": {"X-A": "1"}})
        );
        assert!(form.clear.is_empty());
    }

    #[test]
    fn unchecked_switch_is_false_and_bad_number_is_kept_for_validation() {
        let form = parse_form(
            &SCHEMA,
            &pairs(&[("c_url", "https://x.test"), ("c_priority", "high")]),
        );
        assert_eq!(form.config["silent"], json!(false));
        let errors = validate(&SCHEMA, &form.config).unwrap_err();
        assert_eq!(
            errors,
            vec![field_error("priority", "Enter a whole number")]
        );
    }

    #[test]
    fn validate_reports_required_url_and_select() {
        let errors = validate(&SCHEMA, &json!({"url": "ftp://nope", "mode": "z"})).unwrap_err();
        let keys: Vec<_> = errors.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["url", "mode"]);
        let errors = validate(&SCHEMA, &json!({})).unwrap_err();
        assert_eq!(errors[0], field_error("url", "Enter the webhook url"));
    }

    #[test]
    fn redact_masks_secrets_only() {
        let config = json!({"url": "https://hook/secret", "username": "bob", "token": "", "silent": true,
            "headers": {"Authorization": "Bearer abc"}});
        let shown = redact(Some(&SCHEMA), &config);
        assert_eq!(
            shown,
            json!({"url": SECRET_MASK, "username": "bob", "token": "", "silent": true,
                "headers": {"Authorization": SECRET_MASK}})
        );
        assert!(!shown.to_string().contains("secret"));
        assert!(!shown.to_string().contains("abc"));
        // Unknown kind: everything textual is hidden.
        assert_eq!(
            redact(None, &json!({"username": "bob"})),
            json!({"username": SECRET_MASK})
        );
    }

    #[test]
    fn merge_keeps_old_secrets_when_left_blank_or_masked() {
        let old = json!({"url": "https://old", "token": "t0k3n", "headers": {"Authorization": "Bearer abc", "X-Old": "1"}});
        let mut new = json!({"username": "bob", "token": SECRET_MASK,
            "headers": {"Authorization": SECRET_MASK, "X-New": "2", "X-Gone": SECRET_MASK}});
        merge_secrets(&SCHEMA, &mut new, &old, &[]);
        assert_eq!(
            new,
            json!({"url": "https://old", "username": "bob", "token": "t0k3n",
                "headers": {"Authorization": "Bearer abc", "X-New": "2"}})
        );
    }

    #[test]
    fn merge_replaces_and_clears_on_request() {
        let old = json!({"url": "https://old", "token": "t0k3n"});
        let mut new = json!({"url": "https://new"});
        merge_secrets(&SCHEMA, &mut new, &old, &["token".to_string()]);
        assert_eq!(new, json!({"url": "https://new"}));
    }

    #[test]
    fn changed_destination_never_reuses_stored_secrets() {
        let gotify = &crate::kit::notify::gotify::SCHEMA;
        let old = json!({"server": "https://push.example.com", "app_token": "T0KEN"});
        // Same server, blank token: the stored token is kept.
        let mut same = json!({"server": " https://push.example.com "});
        assert!(merge_secrets_checked(gotify, &mut same, &old, &[]).is_ok());
        assert_eq!(same["app_token"], "T0KEN");
        // New server, blank or masked token: refused, nothing restored.
        for token in [None, Some(SECRET_MASK)] {
            let mut moved = json!({"server": "https://attacker.example"});
            if let Some(t) = token {
                moved["app_token"] = json!(t);
            }
            let errors = merge_secrets_checked(gotify, &mut moved, &old, &[]).unwrap_err();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0].0, "app_token");
            assert!(!moved.to_string().contains("T0KEN"));
        }
        // New server with a retyped token is fine.
        let mut retyped = json!({"server": "https://new.example", "app_token": "NEW"});
        assert!(merge_secrets_checked(gotify, &mut retyped, &old, &[]).is_ok());
        assert_eq!(retyped["app_token"], "NEW");
    }

    #[test]
    fn smtp_host_port_and_security_are_destinations() {
        let email = &crate::kit::notify::email::SCHEMA;
        let old = json!({"host": "smtp.example.com", "port": 587, "security": "starttls",
            "username": "a", "password": "PW", "from": "a@example.com", "to": "b@example.com"});
        for (key, value) in [
            ("host", json!("evil.example")),
            ("port", json!(25)),
            ("security", json!("none")),
        ] {
            let mut new = old.clone();
            new["password"] = json!("");
            new[key] = value;
            let errors = merge_secrets_checked(email, &mut new, &old, &[]).unwrap_err();
            assert_eq!(errors[0].0, "password", "{key}");
        }
        // Only the subject prefix changed: password kept.
        let mut new = old.clone();
        new["password"] = json!("");
        new["subject_prefix"] = json!("[Prod]");
        assert!(merge_secrets_checked(email, &mut new, &old, &[]).is_ok());
        assert_eq!(new["password"], "PW");
    }

    #[test]
    fn webhook_url_is_secret_and_masked_headers_need_retyping_after_move() {
        let webhook = &crate::kit::notify::webhook::SCHEMA;
        assert!(webhook.field("url").unwrap().secret);
        let old = json!({"url": "https://hooks.example/a?key=K", "headers": {"Authorization": "Bearer H"}});
        // Masked URL (unchanged) + masked header: both kept.
        let mut same = json!({"url": SECRET_MASK, "headers": {"Authorization": SECRET_MASK}});
        assert!(merge_secrets_checked(webhook, &mut same, &old, &[]).is_ok());
        assert_eq!(same, old);
        // New URL + masked header: the header must be retyped.
        let mut moved =
            json!({"url": "https://attacker.example", "headers": {"Authorization": SECRET_MASK}});
        let errors = merge_secrets_checked(webhook, &mut moved, &old, &[]).unwrap_err();
        assert_eq!(errors[0].0, "headers");
        // Cosmetic URLs (avatars) aren't destinations.
        let discord = &crate::kit::notify::discord::SCHEMA;
        let old = json!({"url": "https://discord.com/api/webhooks/1/S"});
        let mut new = json!({"avatar_url": "https://img.example/a.png"});
        assert!(merge_secrets_checked(discord, &mut new, &old, &[]).is_ok());
        assert_eq!(new["url"], "https://discord.com/api/webhooks/1/S");
    }

    #[test]
    fn secret_values_lists_secret_strings() {
        let config =
            json!({"url": "https://hook/abc", "username": "bob", "headers": {"A": "Bearer xyz1"}});
        let mut values = secret_values(Some(&SCHEMA), &config);
        values.sort();
        assert_eq!(values, ["Bearer xyz1", "https://hook/abc"]);
    }
}
