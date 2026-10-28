//! `/settings/notifications`: channel list, add sheet (provider picker →
//! generic schema-driven form), edit page, test sends, pause/resume, delete.
//! Owned by the notifications agent.
//!
//! Secrets never leave the server: forms render saved secrets as empty
//! inputs with a "••••••" placeholder, and an empty secret on save keeps the
//! stored value. Viewers may see the channel list but no config at all.
//!
//! The provider picker and a provider's fields are vole-kit's (the
//! view-model in `kit::notify::form`, the `kit/partials/notification_*`
//! templates); the channel list, the toggles about monitors and the routes
//! are Vigilant Vole's.

use crate::i18n::Tr as _;
use askama::Template;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::SignedCookieJar;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::{CurrentUser, Role};
use crate::db::models::NotificationChannel;
use crate::db::notifications as db_notifications;
use crate::kit::notify::form::{FieldView, ProviderGroup, field_views, provider_groups};
use crate::notification;
use crate::notification::schema::{self, FieldError, ProviderSchema};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/notifications", get(list_page).post(create))
        .route("/settings/notifications/providers", get(providers))
        .route("/settings/notifications/form", get(new_form))
        .route("/settings/notifications/test", post(test_unsaved))
        .route(
            "/settings/notifications/{id}",
            get(show).post(update).delete(remove),
        )
        .route("/settings/notifications/{id}/edit", get(edit_page))
        .route("/settings/notifications/{id}/test", post(test_saved))
        .route("/settings/notifications/{id}/active", post(toggle_active))
}

const NAME_MAX: usize = 100;

// -- view models ------------------------------------------------------------------

pub struct ChannelRow {
    pub id: i64,
    pub name: String,
    /// Provider kind, for the `provider-{kind}` tile.
    pub kind: String,
    pub kind_label: String,
    pub icon: &'static str,
    pub active: bool,
    pub is_default: bool,
    pub links_label: String,
}

impl ChannelRow {
    fn new(channel: NotificationChannel, links: i64) -> Self {
        let schema = notification::provider(&channel.kind);
        ChannelRow {
            id: channel.id,
            kind_label: schema.map_or_else(|| channel.kind.clone(), |s| s.tr_label().to_string()),
            icon: schema.map_or("bell", |s| s.icon),
            kind: channel.kind,
            name: channel.name,
            active: channel.active,
            is_default: channel.is_default,
            links_label: monitors_label(links),
        }
    }
}

fn monitors_label(n: i64) -> String {
    match n {
        0 => crate::i18n::t("notif.no_monitors").to_string(),
        n => crate::i18n::tn("count.monitors", n),
    }
}

/// Everything the add/edit form needs.
pub struct ChannelForm {
    pub id: i64,
    pub kind: &'static str,
    pub label: &'static str,
    pub icon: &'static str,
    pub description: &'static str,
    pub docs_url: &'static str,
    pub name: String,
    pub name_error: String,
    pub active: bool,
    pub is_default: bool,
    pub apply_existing: bool,
    pub links_label: String,
    pub fields: Vec<FieldView>,
}

// -- templates ----------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings_notifications.html")]
struct ListPage {
    ctx: PageCtx,
    rows: Vec<ChannelRow>,
    can_edit: bool,
    query: String,
    groups: Vec<ProviderGroup>,
}

#[derive(Template)]
#[template(path = "partials/notification_list.html")]
struct ListPartial {
    rows: Vec<ChannelRow>,
    can_edit: bool,
}

#[derive(Template)]
#[template(path = "kit/partials/notification_picker.html")]
struct PickerPartial {
    query: String,
    groups: Vec<ProviderGroup>,
}

#[derive(Template)]
#[template(path = "kit/partials/notification_provider_grid.html")]
struct GridPartial {
    query: String,
    groups: Vec<ProviderGroup>,
}

#[derive(Template)]
#[template(path = "partials/notification_form.html")]
struct FormPartial {
    form: ChannelForm,
}

#[derive(Template)]
#[template(path = "settings_notifications_edit.html")]
struct EditPage {
    ctx: PageCtx,
    form: ChannelForm,
    delete_url: String,
    delete_question: String,
}

// -- helpers -------------------------------------------------------------------------

fn is_htmx(headers: &HeaderMap) -> bool {
    headers.contains_key("hx-request")
}

/// JSON with every non-ASCII character escaped, so it fits in a header.
fn ascii_json(value: &Value) -> String {
    let mut out = String::new();
    for c in value.to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

fn trigger(value: &Value) -> (HeaderName, HeaderValue) {
    let header = HeaderValue::from_str(&ascii_json(value))
        .unwrap_or_else(|_| HeaderValue::from_static("{}"));
    (HeaderName::from_static("hx-trigger"), header)
}

fn toast(message: &str, kind: &str) -> (HeaderName, HeaderValue) {
    trigger(&json!({ "vv:toast": { "message": message, "kind": kind } }))
}

fn parse_pairs(body: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body).into_owned().collect()
}

fn pair<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

fn pair_flag(pairs: &[(String, String)], key: &str) -> bool {
    matches!(pair(pairs, key), Some("true" | "on" | "1"))
}

fn schema_for(kind: &str) -> ApiResult<&'static ProviderSchema> {
    notification::provider(kind)
        .ok_or_else(|| ApiError::bad_request(crate::i18n::t("notif.err_service")))
}

fn blank_form(schema: &'static ProviderSchema) -> ChannelForm {
    ChannelForm {
        id: 0,
        kind: schema.kind,
        label: schema.tr_label(),
        icon: schema.icon,
        description: schema.tr_description(),
        docs_url: schema.docs_url,
        name: String::new(),
        name_error: String::new(),
        active: true,
        is_default: true,
        apply_existing: true,
        links_label: String::new(),
        fields: field_views(schema, &Value::Null, &Value::Null, &[], true),
    }
}

/// What the page says when a second Hedgerow app channel would be active.
fn hedgerow_second(other: &str) -> String {
    crate::i18n::tf("notif.hedgerow_second", &[("name", other)])
}

fn validate_name(name: &str) -> Option<String> {
    if name.is_empty() {
        Some(crate::i18n::t("notif.err_name").into())
    } else if name.chars().count() > NAME_MAX {
        Some(crate::i18n::tf(
            "notif.err_name_long",
            &[("max", &NAME_MAX.to_string())],
        ))
    } else {
        None
    }
}

async fn list_rows(state: &AppState) -> ApiResult<Vec<ChannelRow>> {
    let rows = state
        .with_conn(|c| db_notifications::list_channels_with_counts(c))
        .await?;
    Ok(rows
        .into_iter()
        .map(|(c, n)| ChannelRow::new(c, n))
        .collect())
}

async fn load_channel(state: &AppState, id: i64) -> ApiResult<NotificationChannel> {
    state
        .with_conn(move |c| db_notifications::get_channel(c, id))
        .await?
        .ok_or(ApiError::NotFound)
}

// -- handlers ---------------------------------------------------------------------------

async fn list_page(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
) -> ApiResult<impl IntoResponse> {
    let rows = list_rows(&state).await?;
    let (ctx, jar) = PageCtx::build(
        &state,
        Some(&user),
        &jar,
        crate::i18n::t("nav.notifications"),
        "settings",
        "notifications",
    )
    .await;
    Ok((
        jar,
        HtmlTemplate(ListPage {
            ctx,
            rows,
            can_edit: user.can_edit(),
            query: String::new(),
            groups: provider_groups(""),
        }),
    ))
}

#[derive(Deserialize)]
struct ProvidersQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    grid: bool,
}

async fn providers(user: CurrentUser, Query(q): Query<ProvidersQuery>) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let groups = provider_groups(&q.q);
    Ok(if q.grid {
        HtmlTemplate(GridPartial { query: q.q, groups }).into_response()
    } else {
        HtmlTemplate(PickerPartial { query: q.q, groups }).into_response()
    })
}

#[derive(Deserialize)]
struct KindQuery {
    #[serde(default)]
    kind: String,
}

async fn new_form(user: CurrentUser, Query(q): Query<KindQuery>) -> ApiResult<impl IntoResponse> {
    user.require(Role::Editor)?;
    let schema = schema_for(&q.kind)?;
    Ok(HtmlTemplate(FormPartial {
        form: blank_form(schema),
    }))
}

async fn create(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    jar: SignedCookieJar,
    body: Bytes,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let pairs = parse_pairs(&body);
    let schema = schema_for(pair(&pairs, "kind").unwrap_or_default())?;
    let name = pair(&pairs, "name").unwrap_or_default().trim().to_string();
    let parsed = schema::parse_form(schema, &pairs);
    let is_default = pair_flag(&pairs, "is_default");
    let apply_existing = pair_flag(&pairs, "apply_existing");

    let name_error = validate_name(&name);
    let field_errors = schema::validate(schema, &parsed.config)
        .err()
        .unwrap_or_default();
    if name_error.is_some() || !field_errors.is_empty() {
        if !is_htmx(&headers) {
            let msg = name_error.unwrap_or_else(|| field_errors[0].1.clone());
            return Ok((
                flash(jar, "error", msg),
                Redirect::to("/settings/notifications"),
            )
                .into_response());
        }
        let mut form = blank_form(schema);
        form.name = name;
        form.name_error = name_error.unwrap_or_default();
        form.is_default = is_default;
        form.apply_existing = apply_existing;
        // Echo what the user just typed (including secrets they typed now).
        form.fields = field_views(schema, &parsed.config, &Value::Null, &field_errors, false);
        return Ok(HtmlTemplate(FormPartial { form }).into_response());
    }

    let config = notification::seal_config(&state.session_key, schema.kind, &parsed.config);
    let kind = schema.kind;
    let saved_name = name.clone();
    let created = state
        .with_conn(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if kind == db_notifications::HEDGEROW
                && let Some(other) = db_notifications::other_active_hedgerow(&tx, None)?
            {
                return Ok(Err(other));
            }
            let id = db_notifications::insert_channel(&tx, &saved_name, kind, &config, is_default)?;
            if apply_existing && !is_default {
                db_notifications::apply_to_all(&tx, id)?;
            }
            tx.commit()?;
            Ok(Ok(id))
        })
        .await?;
    if let Err(other) = created {
        // Only one Hedgerow app channel can be active.
        let message = hedgerow_second(&other);
        if !is_htmx(&headers) {
            return Ok((
                flash(jar, "error", message),
                Redirect::to("/settings/notifications"),
            )
                .into_response());
        }
        let mut form = blank_form(schema);
        form.name = name;
        form.name_error = message;
        form.is_default = is_default;
        form.apply_existing = apply_existing;
        form.fields = field_views(schema, &parsed.config, &Value::Null, &[], false);
        return Ok(HtmlTemplate(FormPartial { form }).into_response());
    }
    tracing::info!(user = %user.username, kind, "notification channel created");

    let message = crate::i18n::tf("notif.added", &[("name", &name)]);
    if !is_htmx(&headers) {
        return Ok((
            flash(jar, "success", message),
            Redirect::to("/settings/notifications"),
        )
            .into_response());
    }
    let rows = list_rows(&state).await?;
    let headers = [
        (
            HeaderName::from_static("hx-retarget"),
            HeaderValue::from_static("#channels"),
        ),
        (
            HeaderName::from_static("hx-reswap"),
            HeaderValue::from_static("outerHTML"),
        ),
        trigger(
            &json!({ "vv:toast": { "message": message, "kind": "success" }, "vv:close-sheet": true }),
        ),
    ];
    Ok((
        headers,
        HtmlTemplate(ListPartial {
            rows,
            can_edit: true,
        }),
    )
        .into_response())
}

async fn show(Path(id): Path<i64>) -> Redirect {
    Redirect::to(&format!("/settings/notifications/{id}/edit"))
}

/// The edit form of a saved channel. A secret that can't be opened (the
/// instance key changed) shows as not saved, with "enter it again".
fn edit_form(
    state: &AppState,
    channel: &NotificationChannel,
    schema: &'static ProviderSchema,
    links: i64,
) -> ChannelForm {
    let opened = notification::open_config(&state.session_key, channel);
    let shown = schema::redact(Some(schema), &opened.config);
    ChannelForm {
        id: channel.id,
        name: channel.name.clone(),
        active: channel.active,
        is_default: channel.is_default,
        apply_existing: false,
        links_label: monitors_label(links),
        fields: field_views(schema, &shown, &opened.config, &opened.errors(), false),
        ..blank_form(schema)
    }
}

async fn render_edit(
    state: &AppState,
    user: &CurrentUser,
    jar: &SignedCookieJar,
    form: ChannelForm,
) -> (SignedCookieJar, HtmlTemplate<EditPage>) {
    let (ctx, jar) = PageCtx::build(
        state,
        Some(user),
        jar,
        form.name.clone(),
        "settings",
        "notifications",
    )
    .await;
    let delete_url = format!("/settings/notifications/{}", form.id);
    let delete_question = crate::i18n::tf("detail.delete_q", &[("name", &form.name)]);
    (
        jar,
        HtmlTemplate(EditPage {
            ctx,
            form,
            delete_url,
            delete_question,
        }),
    )
}

async fn edit_page(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let channel = load_channel(&state, id).await?;
    let Some(schema) = notification::provider(&channel.kind) else {
        let jar = flash(
            jar,
            "error",
            crate::i18n::tf("notif.cant_edit", &[("name", &channel.name)]),
        );
        return Ok((jar, Redirect::to("/settings/notifications")).into_response());
    };
    let links = state
        .with_conn(move |c| db_notifications::count_links(c, id))
        .await?;
    let form = edit_form(&state, &channel, schema, links);
    Ok(render_edit(&state, &user, &jar, form).await.into_response())
}

async fn update(
    State(state): State<AppState>,
    user: CurrentUser,
    jar: SignedCookieJar,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let channel = load_channel(&state, id).await?;
    let schema = schema_for(&channel.kind)?;
    let pairs = parse_pairs(&body);
    let name = pair(&pairs, "name").unwrap_or_default().trim().to_string();
    let parsed = schema::parse_form(schema, &pairs);
    let opened = notification::open_config(&state.session_key, &channel);
    let stored = opened.config.clone();
    let mut config = parsed.config.clone();
    let secret_errors = merge_errors(
        &opened,
        &parsed,
        schema::merge_secrets_checked(schema, &mut config, &stored, &parsed.clear).err(),
    );
    let active = pair_flag(&pairs, "active");
    let is_default = pair_flag(&pairs, "is_default");
    let apply_existing = pair_flag(&pairs, "apply_existing");

    let name_error = validate_name(&name);
    let field_errors = combine_errors(secret_errors, schema::validate(schema, &config).err());
    if name_error.is_some() || !field_errors.is_empty() {
        let links = state
            .with_conn(move |c| db_notifications::count_links(c, id))
            .await?;
        let mut form = edit_form(&state, &channel, schema, links);
        form.name = name;
        form.name_error = name_error.unwrap_or_default();
        form.active = active;
        form.is_default = is_default;
        form.apply_existing = apply_existing;
        form.fields = field_views(schema, &parsed.config, &stored, &field_errors, false);
        let (jar, page) = render_edit(&state, &user, &jar, form).await;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, page).into_response());
    }

    let config = notification::seal_config(&state.session_key, schema.kind, &config);
    let saved_name = name.clone();
    let kind = schema.kind;
    let saved = state
        .with_conn(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // Only a channel that becomes active needs the check: two active
            // ones can already exist (config as code, an import), and then
            // the rest of an edit must still go through.
            if kind == db_notifications::HEDGEROW
                && active
                && !db_notifications::channel_active(&tx, id)?
                && let Some(other) = db_notifications::other_active_hedgerow(&tx, Some(id))?
            {
                return Ok(Err(other));
            }
            db_notifications::update_channel(&tx, id, &saved_name, &config, is_default)?;
            db_notifications::set_active(&tx, id, active)?;
            if apply_existing {
                db_notifications::apply_to_all(&tx, id)?;
            }
            tx.commit()?;
            Ok(Ok(()))
        })
        .await?;
    if let Err(other) = saved {
        let links = state
            .with_conn(move |c| db_notifications::count_links(c, id))
            .await?;
        let mut form = edit_form(&state, &channel, schema, links);
        form.name = name;
        form.name_error = hedgerow_second(&other);
        form.active = active;
        form.is_default = is_default;
        form.apply_existing = apply_existing;
        form.fields = field_views(schema, &parsed.config, &stored, &[], false);
        let (jar, page) = render_edit(&state, &user, &jar, form).await;
        return Ok((StatusCode::UNPROCESSABLE_ENTITY, jar, page).into_response());
    }
    tracing::info!(user = %user.username, channel_id = id, "notification channel updated");
    let jar = flash(
        jar,
        "success",
        crate::i18n::tf("notif.saved", &[("name", &name)]),
    );
    Ok((jar, Redirect::to("/settings/notifications")).into_response())
}

async fn remove(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: HeaderMap,
    Path(id): Path<i64>,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let channel = load_channel(&state, id).await?;
    state
        .with_conn(move |c| db_notifications::delete_channel(c, id))
        .await?;
    tracing::info!(user = %user.username, channel_id = id, "notification channel deleted");
    let message = crate::i18n::tf("notif.deleted", &[("name", &channel.name)]);
    let from_list = headers.get("hx-target").and_then(|v| v.to_str().ok()) == Some("channels");
    if from_list {
        let rows = list_rows(&state).await?;
        return Ok((
            [toast(&message, "success")],
            HtmlTemplate(ListPartial {
                rows,
                can_edit: true,
            }),
        )
            .into_response());
    }
    let redirect = (
        HeaderName::from_static("hx-redirect"),
        HeaderValue::from_static("/settings/notifications"),
    );
    Ok(([redirect, toast(&message, "success")], StatusCode::OK).into_response())
}

async fn toggle_active(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let active = pair_flag(&parse_pairs(&body), "active");
    let channel = load_channel(&state, id).await?;
    let hedgerow = channel.kind == db_notifications::HEDGEROW;
    let refused = state
        .with_conn(move |c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            if active
                && hedgerow
                && !db_notifications::channel_active(&tx, id)?
                && let Some(other) = db_notifications::other_active_hedgerow(&tx, Some(id))?
            {
                return Ok(Some(other));
            }
            db_notifications::set_active(&tx, id, active)?;
            tx.commit()?;
            Ok(None)
        })
        .await?;
    if let Some(other) = refused {
        // Show the list again, so the switch goes back to what is stored.
        let rows = list_rows(&state).await?;
        let headers = [
            (
                HeaderName::from_static("hx-retarget"),
                HeaderValue::from_static("#channels"),
            ),
            (
                HeaderName::from_static("hx-reswap"),
                HeaderValue::from_static("outerHTML"),
            ),
            toast(&hedgerow_second(&other), "error"),
        ];
        return Ok((
            headers,
            HtmlTemplate(ListPartial {
                rows,
                can_edit: true,
            }),
        )
            .into_response());
    }
    let message = if active {
        crate::i18n::tf("notif.resumed", &[("name", &channel.name)])
    } else {
        crate::i18n::tf("notif.paused", &[("name", &channel.name)])
    };
    Ok(([toast(&message, "success")], StatusCode::OK).into_response())
}

/// Sends a test and reports the result as a toast (always 200 so htmx shows it).
async fn run_test(state: &AppState, kind: &str, config: &Value, label: &str) -> Response {
    let settings = match state.settings().await {
        Ok(s) => s,
        Err(err) => return err.into_response(),
    };
    let event = notification::test_event(&settings.site_name, settings.base_url.as_deref());
    match notification::send_config(&state.http, kind, config, &event).await {
        Ok(()) => (
            [toast(
                &crate::i18n::tf("notif.test_sent", &[("label", label)]),
                "success",
            )],
            StatusCode::OK,
        )
            .into_response(),
        Err(err) => {
            tracing::info!(kind, %err, "test notification failed");
            let message = crate::i18n::tf("notif.test_failed", &[("err", &err)]);
            ([toast(&message, "error")], message).into_response()
        }
    }
}

/// Secrets that can't be opened and weren't typed again, then the merge's
/// own re-entry errors (a changed destination).
fn merge_errors(
    opened: &notification::sealed::Opened,
    parsed: &schema::ParsedForm,
    merge: Option<Vec<FieldError>>,
) -> Option<Vec<FieldError>> {
    let mut errors = opened.reentry_errors(&parsed.config, &parsed.clear);
    for (key, msg) in merge.unwrap_or_default() {
        if !errors.iter().any(|(k, _)| *k == key) {
            errors.push((key, msg));
        }
    }
    (!errors.is_empty()).then_some(errors)
}

/// Secret re-entry errors first, then validation errors for other fields.
fn combine_errors(
    secret: Option<Vec<FieldError>>,
    validation: Option<Vec<FieldError>>,
) -> Vec<FieldError> {
    let mut errors = secret.unwrap_or_default();
    for (key, msg) in validation.unwrap_or_default() {
        if !errors.iter().any(|(k, _)| *k == key) {
            errors.push((key, msg));
        }
    }
    errors
}

fn first_error_toast(schema: &ProviderSchema, errors: &[FieldError]) -> Response {
    let (key, msg) = &errors[0];
    let label = schema
        .field(key)
        .map_or(crate::i18n::t("notif.settings_label"), |f| {
            f.tr_label(schema.kind)
        });
    ([toast(&format!("{label}: {msg}"), "error")], StatusCode::OK).into_response()
}

async fn test_unsaved(
    State(state): State<AppState>,
    user: CurrentUser,
    body: Bytes,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let pairs = parse_pairs(&body);
    let schema = schema_for(pair(&pairs, "kind").unwrap_or_default())?;
    let parsed = schema::parse_form(schema, &pairs);
    if let Err(errors) = schema::validate(schema, &parsed.config) {
        return Ok(first_error_toast(schema, &errors));
    }
    Ok(run_test(&state, schema.kind, &parsed.config, schema.tr_label()).await)
}

/// Tests a saved channel. When the edit form is included (it carries `kind`),
/// its unsaved values are tested, with blank secrets taken from storage.
async fn test_saved(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<i64>,
    body: Bytes,
) -> ApiResult<Response> {
    user.require(Role::Editor)?;
    let channel = load_channel(&state, id).await?;
    let label = format!("“{}”", channel.name);
    let pairs = parse_pairs(&body);
    let Some(schema) = notification::provider(&channel.kind) else {
        let msg = crate::i18n::tf("notif.cant_send", &[("label", &label)]);
        return Ok(([toast(&msg, "error")], StatusCode::OK).into_response());
    };
    let opened = notification::open_config(&state.session_key, &channel);
    let stored = opened.config.clone();
    let config = if pair(&pairs, "kind").is_some() {
        let parsed = schema::parse_form(schema, &pairs);
        let mut config = parsed.config.clone();
        let secret_errors = merge_errors(
            &opened,
            &parsed,
            schema::merge_secrets_checked(schema, &mut config, &stored, &parsed.clear).err(),
        );
        let errors = combine_errors(secret_errors, schema::validate(schema, &config).err());
        if !errors.is_empty() {
            return Ok(first_error_toast(schema, &errors));
        }
        config
    } else if !opened.is_readable() {
        tracing::info!(
            channel_id = id,
            "test notification not sent: a secret can't be opened"
        );
        return Ok(first_error_toast(schema, &opened.errors()));
    } else {
        stored
    };
    Ok(run_test(&state, schema.kind, &config, &label).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notification::schema::SECRET_MASK;
    use crate::notification::{PROVIDERS, discord, telegram, webhook};

    #[test]
    fn ascii_json_escapes_non_ascii_for_headers() {
        let out = ascii_json(&json!({"m": "Added “Ops” 🔔"}));
        assert!(out.is_ascii());
        let back: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back["m"], "Added “Ops” 🔔");
        assert!(HeaderValue::from_str(&out).is_ok());
    }

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
        let views = field_views(
            &crate::notification::email::SCHEMA,
            &Value::Null,
            &Value::Null,
            &[],
            true,
        );
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
    }

    #[test]
    fn errors_attach_to_their_fields() {
        let errors = vec![("chat_id".to_string(), "Enter the chat id".to_string())];
        let views = field_views(&telegram::SCHEMA, &json!({}), &Value::Null, &errors, false);
        assert_eq!(
            views.iter().find(|v| v.name == "c_chat_id").unwrap().error,
            "Enter the chat id"
        );
    }

    #[test]
    fn channel_rows_carry_the_kind_for_the_provider_tile() {
        let channel = NotificationChannel {
            id: 3,
            name: "Ops".into(),
            kind: "discord".into(),
            config: "{}".into(),
            active: true,
            is_default: false,
            created_at: String::new(),
        };
        let row = ChannelRow::new(channel, 0);
        assert_eq!(row.kind, "discord");
        let html = ListPartial {
            rows: vec![row],
            can_edit: true,
        }
        .render()
        .unwrap();
        assert!(html.contains("provider-tile provider-discord"), "{html}");
    }

    #[test]
    fn name_validation() {
        assert!(validate_name("").is_some());
        assert!(validate_name(&"x".repeat(101)).is_some());
        assert!(validate_name("Ops").is_none());
    }

    // -- the Hedgerow app channel through the real routes -------------------

    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::header;
    use axum::middleware::{self, Next};
    use axum_extra::extract::cookie::Key;
    use tower::ServiceExt as _;

    static NEXT_DB: AtomicU32 = AtomicU32::new(0);

    struct App {
        state: AppState,
        router: Router,
        path: PathBuf,
    }

    impl Drop for App {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let mut p = self.path.clone().into_os_string();
                p.push(suffix);
                let _ = std::fs::remove_file(p);
            }
        }
    }

    async fn app() -> App {
        let path = std::env::temp_dir().join(format!(
            "vv-notif-test-{}-{}.db",
            std::process::id(),
            NEXT_DB.fetch_add(1, Ordering::Relaxed)
        ));
        let pool = crate::db::build_pool(&path);
        crate::db::run_migrations(&pool, &std::env::temp_dir()).await;
        let (tx, _) = tokio::sync::broadcast::channel(16);
        let http = reqwest::Client::new();
        let key = Key::generate();
        let state = AppState {
            scheduler: crate::monitor::Scheduler::new(
                pool.clone(),
                tx.clone(),
                http.clone(),
                key.clone(),
            ),
            db: pool,
            session_key: key,
            update_tx: tx,
            http,
            sign_in_limits: Default::default(),
            config: Arc::new(crate::config::Config {
                data_dir: std::env::temp_dir(),
                bind_addr: "127.0.0.1:0".into(),
            }),
        };
        let user = CurrentUser {
            id: 1,
            username: "ada".into(),
            display_name: "Ada".into(),
            role: Role::Admin,
            account_role: Role::Admin,
            scopes: Default::default(),
        };
        let router = Router::new()
            .merge(super::router())
            .layer(middleware::from_fn(move |mut req: Request, next: Next| {
                let user = user.clone();
                async move {
                    req.extensions_mut().insert(user);
                    next.run(req).await
                }
            }))
            .with_state(state.clone());
        App {
            state,
            router,
            path,
        }
    }

    impl App {
        async fn send(&self, req: Request<Body>) -> Response {
            self.router.clone().oneshot(req).await.unwrap()
        }

        async fn get(&self, uri: &str) -> (StatusCode, String) {
            let res = self
                .send(Request::get(uri).body(Body::empty()).unwrap())
                .await;
            (res.status(), body_text(res).await)
        }

        async fn htmx_post(&self, uri: &str, body: &str) -> Response {
            self.send(
                Request::post(uri)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("hx-request", "true")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
        }

        async fn channels(&self) -> Vec<(i64, String, bool)> {
            self.state
                .with_conn(|c| {
                    Ok(db_notifications::list_channels(c)?
                        .into_iter()
                        .map(|ch| (ch.id, ch.name, ch.active))
                        .collect())
                })
                .await
                .unwrap()
        }
    }

    async fn body_text(res: Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn the_hedgerow_channel_form_has_no_fields_and_saves() {
        let app = app().await;
        let (status, html) = app.get("/settings/notifications/form?kind=hedgerow").await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains("Hedgerow app"), "{html}");
        assert!(html.contains(r#"name="name""#), "the name is asked for");
        assert_eq!(
            html.matches(r#"class="field"#).count(),
            1,
            "a provider with no fields renders only the name: {html}"
        );
        // It is in the picker too.
        let (_, picker) = app.get("/settings/notifications/providers").await;
        assert!(picker.contains("provider-hedgerow"), "{picker}");

        let res = app
            .htmx_post(
                "/settings/notifications",
                "kind=hedgerow&name=Phones&is_default=on",
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        let channels = app.channels().await;
        assert_eq!(channels.len(), 1, "{channels:?}");
        assert!(channels[0].2, "new channels are active");
        let (is_default, kind): (bool, String) = app
            .state
            .with_conn(|c| {
                c.query_row(
                    "SELECT is_default, kind FROM notification_channels",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .await
            .unwrap();
        assert_eq!(kind, "hedgerow");
        assert!(is_default);

        // The edit page works without a card of fields.
        let (status, html) = app
            .get(&format!("/settings/notifications/{}/edit", channels[0].0))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(html.contains(r#"id="sec-provider""#), "{html}");
        assert_eq!(
            html.matches(r#"class="field"#).count(),
            1,
            "only the name: {html}"
        );
    }

    #[tokio::test]
    async fn a_second_active_hedgerow_channel_is_refused_in_the_forms() {
        let app = app().await;
        let res = app
            .htmx_post("/settings/notifications", "kind=hedgerow&name=Phones")
            .await;
        assert_eq!(res.status(), StatusCode::OK);

        // Adding another: the sheet comes back with the reason, nothing saved.
        let res = app
            .htmx_post("/settings/notifications", "kind=hedgerow&name=More")
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        let html = body_text(res).await;
        assert!(html.contains("channel-form"), "the form is shown again");
        assert!(
            html.contains("Phones"),
            "the reason names the channel: {html}"
        );
        assert_eq!(app.channels().await.len(), 1);

        // A paused one can exist, but switching it on is refused: the list
        // comes back so the switch shows what is stored.
        let second = app
            .state
            .with_conn(|c| {
                let id = db_notifications::insert_channel(c, "Spare", "hedgerow", "{}", false)?;
                db_notifications::set_active(c, id, false)?;
                Ok(id)
            })
            .await
            .unwrap();
        let res = app
            .htmx_post(
                &format!("/settings/notifications/{second}/active"),
                "active=true",
            )
            .await;
        assert_eq!(res.headers()["hx-retarget"], "#channels");
        let trigger = res.headers()["hx-trigger"].to_str().unwrap().to_string();
        assert!(trigger.contains("error"), "{trigger}");
        assert!(
            app.channels().await.iter().any(|c| c.0 == second && !c.2),
            "still paused"
        );

        // The edit form: the same.
        let res = app
            .htmx_post(
                &format!("/settings/notifications/{second}"),
                "kind=hedgerow&name=Spare&active=on",
            )
            .await;
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(app.channels().await.iter().any(|c| c.0 == second && !c.2));

        // Pausing the first makes room.
        let first = app.channels().await[0].0;
        let res = app
            .htmx_post(
                &format!("/settings/notifications/{first}/active"),
                "active=false",
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        let res = app
            .htmx_post(
                &format!("/settings/notifications/{second}/active"),
                "active=true",
            )
            .await;
        assert!(res.headers().get("hx-retarget").is_none());
        assert!(app.channels().await.iter().any(|c| c.0 == second && c.2));

        // Other kinds are not limited.
        let res = app
            .htmx_post(
                "/settings/notifications",
                "kind=discord&name=Chat&c_url=https%3A%2F%2Fdiscord.com%2Fapi%2Fwebhooks%2F1%2Fabc",
            )
            .await;
        assert_eq!(res.status(), StatusCode::OK);
        let reply = body_text(res).await;
        assert_eq!(app.channels().await.len(), 3, "{reply}");
    }

    #[tokio::test]
    async fn the_one_active_hedgerow_check_runs_only_when_a_channel_becomes_active() {
        let app = app().await;
        // Two active ones can exist already (config as code, an import).
        let (a, b, spare) = app
            .state
            .with_conn(|c| {
                let a = db_notifications::insert_channel(c, "Phones", "hedgerow", "{}", false)?;
                let b = db_notifications::insert_channel(c, "Tablets", "hedgerow", "{}", false)?;
                let spare = db_notifications::insert_channel(c, "Spare", "hedgerow", "{}", false)?;
                db_notifications::set_active(c, spare, false)?;
                Ok((a, b, spare))
            })
            .await
            .unwrap();

        // Renaming an active one still works, in the form and with the switch.
        let res = app
            .htmx_post(
                &format!("/settings/notifications/{a}"),
                "kind=hedgerow&name=Renamed&active=on",
            )
            .await;
        assert_ne!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let channels = app.channels().await;
        assert!(
            channels.iter().any(|c| c.0 == a && c.1 == "Renamed" && c.2),
            "{channels:?}"
        );
        let res = app
            .htmx_post(
                &format!("/settings/notifications/{b}/active"),
                "active=true",
            )
            .await;
        assert!(res.headers().get("hx-retarget").is_none());

        // Activating the inactive one is refused.
        let res = app
            .htmx_post(
                &format!("/settings/notifications/{spare}/active"),
                "active=true",
            )
            .await;
        assert_eq!(res.headers()["hx-retarget"], "#channels");
        assert!(app.channels().await.iter().any(|c| c.0 == spare && !c.2));
    }
}
