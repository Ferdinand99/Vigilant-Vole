//! The provider form as the settings pages show it: the view-model behind
//! the kit's `partials/notification_*.html` templates, so every product
//! renders the add and edit forms of its notification channels the same
//! way.
//!
//! * [`provider_groups`]: the provider picker, grouped by category and
//!   filtered by a search (`kit/partials/notification_picker.html`, which
//!   includes `notification_provider_grid.html`; both need `query` and
//!   `groups`).
//! * [`field_views`]: one provider's fields
//!   (`kit/partials/notification_fields.html`, which needs `form.fields`).
//! * `kit/partials/notification_name.html`: the channel's name field (needs
//!   `form.name`, `form.name_error` and `form.label`).
//!
//! The partials load the picker from `/settings/notifications/providers`
//! (with `?grid=true&q=` for the search) and a provider's form from
//! `/settings/notifications/form?kind=`, into `#channel-sheet-body`; the
//! product serves those routes.
//!
//! Secrets never reach the browser: a saved secret renders as an empty
//! input with [`SECRET_MASK`] as its placeholder, and leaving it empty
//! keeps it ([`super::schema::merge_secrets_checked`]).

use std::borrow::Cow;

use serde_json::Value;

use super::PROVIDERS;
use super::schema::{self, Category, FieldError, FieldKind, ProviderSchema, SECRET_MASK};

/// One `<option>` of a select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionView {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

/// One provider field, ready to render.
#[derive(Debug, Clone)]
pub struct FieldView {
    /// The form control's name (`c_<key>`).
    pub name: String,
    /// The "remove the saved value" checkbox's name.
    pub clear_name: String,
    pub label: &'static str,
    /// "text" | "url" | "password" | "number" | "select" | "bool" |
    /// "textarea" | "kv"
    pub input: &'static str,
    pub required: bool,
    /// A secret is stored and the input is empty: leaving it empty keeps it.
    pub saved: bool,
    pub placeholder: String,
    pub help: Cow<'static, str>,
    pub value: String,
    pub checked: bool,
    pub options: Vec<OptionView>,
    /// Rows of a key–value field (at least one, maybe empty).
    pub rows: Vec<(String, String)>,
    pub error: String,
    /// `data-show-when` rule with form names (`c_security=starttls,tls`).
    pub show_when: String,
    pub autocomplete: &'static str,
}

impl FieldView {
    pub fn has_help(&self) -> bool {
        !self.help.is_empty() || self.saved
    }

    /// Ids for `aria-describedby` (help, error), space separated.
    pub fn describedby(&self) -> String {
        let mut ids = Vec::new();
        if self.has_help() {
            ids.push(format!("f-{}-help", self.name));
        }
        if !self.error.is_empty() {
            ids.push(format!("f-{}-error", self.name));
        }
        ids.join(" ")
    }
}

/// A provider in the picker.
#[derive(Debug, Clone)]
pub struct ProviderCard {
    pub kind: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub description: &'static str,
}

/// The providers of one category.
#[derive(Debug, Clone)]
pub struct ProviderGroup {
    pub label: &'static str,
    /// Element id of the group's heading (`cat-chat`).
    pub id: String,
    pub providers: Vec<ProviderCard>,
}

/// Does the provider match the picker's search? By name, kind, description
/// (English and translated) or category.
pub fn matches_query(p: &ProviderSchema, q: &str) -> bool {
    let q = q.trim().to_lowercase();
    q.is_empty()
        || p.label.to_lowercase().contains(&q)
        || p.kind.contains(&q)
        || p.description.to_lowercase().contains(&q)
        || p.tr_description().to_lowercase().contains(&q)
        || p.category.label().to_lowercase().contains(&q)
}

/// The picker's groups for a search (`""` for all), in category order;
/// categories without a match are left out.
pub fn provider_groups(query: &str) -> Vec<ProviderGroup> {
    Category::ALL
        .iter()
        .filter_map(|cat| {
            let providers: Vec<ProviderCard> = PROVIDERS
                .iter()
                .filter(|p| p.category == *cat && matches_query(p, query))
                .map(|p| ProviderCard {
                    kind: p.kind,
                    label: p.tr_label(),
                    icon: p.icon,
                    description: p.tr_description(),
                })
                .collect();
            (!providers.is_empty()).then(|| ProviderGroup {
                label: cat.label(),
                id: format!("cat-{cat:?}").to_lowercase(),
                providers,
            })
        })
        .collect()
}

/// The `input` of a field kind.
pub fn input_type(kind: FieldKind) -> &'static str {
    match kind {
        FieldKind::Text => "text",
        FieldKind::Url => "url",
        FieldKind::Secret => "password",
        FieldKind::Number => "number",
        FieldKind::Select(_) => "select",
        FieldKind::Bool => "bool",
        FieldKind::Textarea => "textarea",
        FieldKind::KeyValue => "kv",
    }
}

fn display_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn blank(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Object(o)) => o.is_empty(),
        _ => false,
    }
}

/// The provider's fields as the form shows them.
///
/// * `values`: what to show: ignored when `fresh` (a new channel shows the
///   defaults), else what was submitted or the redacted stored config
///   ([`schema::redact`]), never raw stored secrets.
/// * `stored`: the saved config (`Null` for a new channel), only to know
///   whether a secret is saved.
/// * `errors`: messages by field key ([`schema::validate`]).
pub fn field_views(
    schema: &ProviderSchema,
    values: &Value,
    stored: &Value,
    errors: &[FieldError],
    fresh: bool,
) -> Vec<FieldView> {
    schema
        .fields
        .iter()
        .map(|f| {
            let raw = values.get(f.key);
            let saved = f.secret && f.kind != FieldKind::KeyValue && !blank(stored.get(f.key));
            let default = f.default_value();
            let mut value = if fresh {
                default.to_string()
            } else {
                raw.map(display_value).unwrap_or_default()
            };
            if f.secret && value == SECRET_MASK {
                value.clear();
            }
            let options = match f.kind {
                FieldKind::Select(opts) => {
                    let chosen = if value.is_empty() {
                        default.as_ref()
                    } else {
                        value.as_str()
                    };
                    let chosen = if opts.iter().any(|(v, _)| *v == chosen) {
                        chosen
                    } else {
                        opts.first().map_or("", |o| o.0)
                    };
                    opts.iter()
                        .map(|(v, l)| OptionView {
                            value: (*v).to_string(),
                            label: f.tr_option(schema.kind, v, l).to_string(),
                            selected: *v == chosen,
                        })
                        .collect()
                }
                _ => Vec::new(),
            };
            let checked = if fresh {
                default == "true"
            } else {
                raw.is_some_and(|v| v.as_bool() == Some(true))
            };
            let mut rows: Vec<(String, String)> = match raw {
                Some(Value::Object(map)) if !fresh => map
                    .iter()
                    .map(|(k, v)| (k.clone(), display_value(v)))
                    .collect(),
                _ => Vec::new(),
            };
            if f.kind == FieldKind::KeyValue && rows.is_empty() {
                rows.push((String::new(), String::new()));
            }
            let show_when = if f.show_when.is_empty() {
                String::new()
            } else {
                format!("{}{}", schema::FIELD_PREFIX, f.show_when)
            };
            let placeholder = if saved {
                SECRET_MASK.to_string()
            } else {
                f.tr_placeholder(schema.kind).to_string()
            };
            FieldView {
                name: f.form_name(),
                clear_name: format!("{}__clear", f.form_name()),
                label: f.tr_label(schema.kind),
                input: input_type(f.kind),
                required: f.required,
                saved,
                placeholder,
                help: f.tr_help(schema.kind),
                value,
                checked,
                options,
                rows,
                error: errors
                    .iter()
                    .find(|(k, _)| k == f.key)
                    .map(|(_, m)| m.clone())
                    .unwrap_or_default(),
                show_when,
                autocomplete: if f.kind == FieldKind::Secret {
                    "new-password"
                } else {
                    "off"
                },
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::notify::{discord, email, telegram, webhook};

    #[test]
    fn provider_search_filters_and_groups() {
        let all = provider_groups("");
        assert_eq!(
            all.iter().map(|g| g.providers.len()).sum::<usize>(),
            PROVIDERS.len()
        );
        let chat = provider_groups("disc");
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].providers[0].kind, "discord");
        assert_eq!(chat[0].id, "cat-chat");
        assert!(provider_groups("zzz-nothing").is_empty());
        assert!(
            !provider_groups("push").is_empty(),
            "matches category labels too"
        );
    }

    #[test]
    fn saved_secrets_render_empty_with_mask_placeholder() {
        let stored = json!({"bot_token": "1:SECRET", "chat_id": "42", "silent": true});
        let shown = schema::redact(Some(&telegram::SCHEMA), &stored);
        let views = field_views(&telegram::SCHEMA, &shown, &stored, &[], false);
        let token = views.iter().find(|v| v.name == "c_bot_token").unwrap();
        assert!(token.value.is_empty());
        assert!(token.saved);
        assert_eq!(token.placeholder, SECRET_MASK);
        assert_eq!(token.input, "password");
        assert!(token.has_help());
        assert_eq!(token.describedby(), "f-c_bot_token-help");
        let chat = views.iter().find(|v| v.name == "c_chat_id").unwrap();
        assert_eq!(chat.value, "42");
        assert!(views.iter().find(|v| v.name == "c_silent").unwrap().checked);
        for v in &views {
            assert!(!v.value.contains("SECRET"));
        }
    }

    #[test]
    fn sensitive_urls_and_headers_are_never_rendered() {
        let stored = json!({"url": "https://discord.com/api/webhooks/1/SECRET"});
        let shown = schema::redact(Some(&discord::SCHEMA), &stored);
        let views = field_views(&discord::SCHEMA, &shown, &stored, &[], false);
        assert!(views.iter().all(|v| !v.value.contains("SECRET")));

        let stored = json!({"url": "https://x", "headers": {"Authorization": "Bearer SECRET"}});
        let shown = schema::redact(Some(&webhook::SCHEMA), &stored);
        let views = field_views(&webhook::SCHEMA, &shown, &stored, &[], false);
        let headers = views.iter().find(|v| v.input == "kv").unwrap();
        assert_eq!(
            headers.rows,
            [("Authorization".to_string(), SECRET_MASK.to_string())]
        );
    }

    #[test]
    fn fresh_form_uses_defaults_and_show_when_is_prefixed() {
        let views = field_views(&email::SCHEMA, &Value::Null, &Value::Null, &[], true);
        let port = views.iter().find(|v| v.name == "c_port").unwrap();
        assert_eq!(port.value, "587");
        let security = views.iter().find(|v| v.name == "c_security").unwrap();
        assert!(
            security
                .options
                .iter()
                .any(|o| o.selected && o.value == "starttls")
        );
        let ignore = views.iter().find(|v| v.name == "c_ignore_tls").unwrap();
        assert_eq!(ignore.show_when, "c_security=starttls,tls");
        let headers = field_views(&webhook::SCHEMA, &Value::Null, &Value::Null, &[], true);
        let kv = headers.iter().find(|v| v.input == "kv").unwrap();
        assert_eq!(kv.rows, [(String::new(), String::new())], "one empty row");
    }

    #[test]
    fn errors_attach_to_their_fields() {
        let errors = vec![("chat_id".to_string(), "Enter the chat id".to_string())];
        let views = field_views(&telegram::SCHEMA, &json!({}), &Value::Null, &errors, false);
        let chat = views.iter().find(|v| v.name == "c_chat_id").unwrap();
        assert_eq!(chat.error, "Enter the chat id");
        assert!(chat.describedby().ends_with("f-c_chat_id-error"));
    }
}
