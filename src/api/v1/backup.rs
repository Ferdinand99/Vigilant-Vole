//! `/settings/backup` (admins): database snapshot download, config export,
//! config import with a dry-run plan, Uptime Kuma import, and "restore on
//! next start" from an uploaded database.

use crate::i18n::Tr as _;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use askama::Template;
use axum::Router;
use axum::body::Body as HttpBody;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::SignedCookieJar;
use rand::RngExt;
use rand::distr::Alphanumeric;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;

use crate::api::{ApiError, ApiResult, AppState, HtmlTemplate, PageCtx, flash};
use crate::auth::CurrentUser;
use crate::config_io::{self, Action, Document, ImportError, Mode, Plan, RESTORE_PENDING_FILE};
use crate::kuma_import::{self, Skipped};

/// Uploads of whole databases (Kuma or our own backups).
const MAX_DB_UPLOAD: usize = 512 * 1024 * 1024;
/// Config documents (pasted or uploaded), plus room for the other form fields.
const MAX_DOC_UPLOAD: usize = super::config::MAX_DOCUMENT_BYTES + 64 * 1024;
/// Temporary files older than this are leftovers of interrupted requests.
const STALE_TMP: Duration = Duration::from_secs(3600);
/// How long a previewed import can be applied.
const PENDING_TTL: Duration = Duration::from_secs(30 * 60);

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/settings/backup", get(page))
        .route("/settings/backup/database", get(download_database))
        .route("/settings/backup/import/preview", post(import_preview).layer(DefaultBodyLimit::max(MAX_DOC_UPLOAD)))
        .route("/settings/backup/import/apply", post(import_apply))
        .route("/settings/backup/kuma/preview", post(kuma_preview).layer(DefaultBodyLimit::max(MAX_DB_UPLOAD)))
        .route(
            "/settings/backup/restore",
            post(restore_upload).delete(restore_cancel).layer(DefaultBodyLimit::max(MAX_DB_UPLOAD)),
        )
}

// ---------------------------------------------------------------------------
// Previewed imports waiting for "Apply"
// ---------------------------------------------------------------------------

struct Pending {
    user_id: i64,
    created: Instant,
    doc: Document,
    mode: Mode,
    label: String,
}

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn random_token(len: usize) -> String {
    rand::rng().sample_iter(&Alphanumeric).take(len).map(char::from).collect()
}

fn stash(user_id: i64, doc: Document, mode: Mode, label: String) -> String {
    let token = random_token(32);
    if let Ok(mut map) = PENDING.lock() {
        map.retain(|_, p| p.created.elapsed() < PENDING_TTL);
        while map.len() >= 20 {
            let Some(oldest) = map.iter().min_by_key(|(_, p)| p.created).map(|(k, _)| k.clone()) else { break };
            map.remove(&oldest);
        }
        map.insert(token.clone(), Pending { user_id, created: Instant::now(), doc, mode, label });
    }
    token
}

fn take(token: &str, user_id: i64) -> Option<Pending> {
    let mut map = PENDING.lock().ok()?;
    let p = map.remove(token)?;
    (p.user_id == user_id && p.created.elapsed() < PENDING_TTL).then_some(p)
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings_backup.html")]
struct BackupTemplate {
    ctx: PageCtx,
    restore_pending: bool,
    monitor_count: i64,
    db_size: String,
}

fn data_dir(state: &AppState) -> PathBuf {
    state.config.data_dir.clone()
}

fn restore_path(state: &AppState) -> PathBuf {
    data_dir(state).join(RESTORE_PENDING_FILE)
}

async fn page(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar) -> ApiResult<Response> {
    user.require_admin()?;
    let monitor_count = state.with_conn(|c| c.query_row("SELECT COUNT(*) FROM monitors", [], |r| r.get(0))).await?;
    let db_size = std::fs::metadata(state.config.db_path())
        .map(|m| crate::api::settings::format_bytes(i64::try_from(m.len()).unwrap_or(i64::MAX)))
        .unwrap_or_else(|_| "—".into());
    let (ctx, jar) = PageCtx::build(&state, Some(&user), &jar, crate::i18n::t("nav.backup"), "settings", "backup").await;
    let tpl = BackupTemplate { ctx, restore_pending: restore_path(&state).exists(), monitor_count, db_size };
    Ok((jar, HtmlTemplate(tpl)).into_response())
}

// ---------------------------------------------------------------------------
// Database snapshot
// ---------------------------------------------------------------------------

/// Deletes the file when dropped (temporary uploads and snapshots).
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `DATA_DIR/tmp`, with stale snapshots and uploads cleared out. File names
/// there are always ours (`snapshot-…`, `upload-…` + random token); nothing
/// the client sends ends up in a path.
fn tmp_dir(state: &AppState) -> std::io::Result<PathBuf> {
    let dir = data_dir(state).join("tmp");
    std::fs::create_dir_all(&dir)?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let ours = name.starts_with("snapshot-") || name.starts_with("upload-");
            let stale = e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t.elapsed().is_ok_and(|age| age > STALE_TMP));
            if ours && stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(dir)
}

async fn download_database(State(state): State<AppState>, user: CurrentUser) -> ApiResult<Response> {
    user.require_admin()?;
    let dir = tmp_dir(&state).map_err(ApiError::internal)?;
    let path = dir.join(format!("snapshot-{}.db", random_token(12)));
    let target = path.to_string_lossy().into_owned();
    // VACUUM INTO writes a consistent, compacted copy while the app keeps running.
    state.with_conn(move |c| c.execute("VACUUM INTO ?1", [&target]).map(|_| ())).await?;
    let file = tokio::fs::File::open(&path).await.map_err(ApiError::internal)?;
    let len = file.metadata().await.map(|m| m.len()).ok();
    let guard = TempFile(path);
    let stream = futures_util::stream::unfold((file, Some(guard)), |(mut file, guard)| async move {
        use tokio::io::AsyncReadExt;
        guard.as_ref()?;
        let mut buf = vec![0u8; 64 * 1024];
        match file.read(&mut buf).await {
            Ok(0) => None, // guard drops here: the snapshot is deleted
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(buf)), (file, guard)))
            }
            Err(e) => Some((Err(e), (file, None))),
        }
    });
    let now = time::OffsetDateTime::now_utc();
    let name = format!(
        "vigilant-vole-backup-{:04}{:02}{:02}-{:02}{:02}.db",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute()
    );
    tracing::info!(user = %user.username, "database backup downloaded");
    let mut res = HttpBody::from_stream(stream).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/vnd.sqlite3"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    if let Some(len) = len
        && let Ok(v) = HeaderValue::from_str(&len.to_string())
    {
        h.insert(header::CONTENT_LENGTH, v);
    }
    Ok(res)
}

// ---------------------------------------------------------------------------
// Plan preview partial
// ---------------------------------------------------------------------------

/// One row of the plan list.
struct ItemView {
    action: Action,
    kind: &'static str,
    key: String,
    detail: String,
}

struct ErrorView {
    what: String,
    message: String,
}

#[derive(Template)]
#[template(path = "partials/backup_plan.html")]
struct PlanPartial {
    /// Empty when the plan can't be applied.
    token: String,
    label: String,
    mode: &'static str,
    create: usize,
    update: usize,
    delete: usize,
    unchanged: usize,
    changes: Vec<ItemView>,
    same: Vec<ItemView>,
    errors: Vec<ErrorView>,
    warnings: Vec<String>,
    skipped: Vec<Skipped>,
    /// Shown instead of a plan (unreadable upload etc.).
    problem: String,
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        config_io::KIND_SETTING => crate::i18n::t("backup.kind_setting"),
        config_io::KIND_TAG => crate::i18n::t("backup.kind_tag"),
        config_io::KIND_CHANNEL => crate::i18n::t("backup.kind_channel"),
        config_io::KIND_MONITOR => crate::i18n::t("backup.kind_monitor"),
        config_io::KIND_PAGE => crate::i18n::t("backup.kind_page"),
        config_io::KIND_MAINTENANCE => crate::i18n::t("backup.kind_maintenance"),
        config_io::KIND_BROWSER => crate::i18n::t("backup.kind_browser"),
        "notification" => crate::i18n::t("backup.kind_channel"),
        _ => crate::i18n::t("backup.kind_item"),
    }
}

impl PlanPartial {
    fn problem(message: impl Into<String>) -> Self {
        PlanPartial {
            token: String::new(),
            label: String::new(),
            mode: "merge",
            create: 0,
            update: 0,
            delete: 0,
            unchanged: 0,
            changes: Vec::new(),
            same: Vec::new(),
            errors: Vec::new(),
            warnings: Vec::new(),
            skipped: Vec::new(),
            problem: message.into(),
        }
    }

    fn from_plan(plan: &Plan, token: String, label: String, skipped: Vec<Skipped>, warnings: Vec<String>) -> Self {
        let s = plan.summary();
        let view = |i: &config_io::PlanItem| ItemView {
            action: i.action,
            kind: kind_label(&i.kind),
            key: i.key.clone(),
            detail: match (&i.note, i.changes.is_empty()) {
                (Some(n), _) => n.clone(),
                (None, false) => i.changes.join(", "),
                _ => String::new(),
            },
        };
        let mut warn = plan.warnings.clone();
        warn.extend(warnings);
        PlanPartial {
            token: if plan.is_ok() && plan.has_changes() { token } else { String::new() },
            label,
            mode: plan.mode.as_str(),
            create: s.create,
            update: s.update,
            delete: s.delete,
            unchanged: s.unchanged,
            changes: plan.items.iter().filter(|i| !matches!(i.action, Action::Unchanged)).map(view).collect(),
            same: plan.items.iter().filter(|i| matches!(i.action, Action::Unchanged)).map(view).collect(),
            errors: plan
                .errors
                .iter()
                .map(|e| ErrorView {
                    what: match &e.field {
                        Some(f) => format!("{} “{}” · {f}", kind_label(&e.kind), e.key),
                        None => format!("{} “{}”", kind_label(&e.kind), e.key),
                    },
                    message: e.message.clone(),
                })
                .collect(),
            warnings: warn,
            skipped,
            problem: String::new(),
        }
    }

    fn has_changes(&self) -> bool {
        self.create + self.update + self.delete > 0
    }
}

fn partial(p: PlanPartial) -> Response {
    // Always 200 so htmx swaps it into the sheet.
    HtmlTemplate(p).into_response()
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

/// Collected multipart fields: small text fields, and one file streamed to disk.
#[derive(Default)]
struct Upload {
    fields: HashMap<String, String>,
    file: Option<TempFile>,
    file_name: String,
}

async fn read_upload(state: &AppState, mut mp: Multipart, file_field: &str, to: Option<PathBuf>) -> Result<Upload, String> {
    let mut up = Upload::default();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(format!("The upload failed: {}", e.body_text())),
        };
        let name = field.name().unwrap_or_default().to_string();
        if name == file_field && up.file.is_some() {
            return Err(crate::i18n::t("backup.err_one_file").to_string());
        }
        if name == file_field {
            let file_name = field.file_name().unwrap_or_default().to_string();
            let path = match &to {
                Some(p) => p.clone(),
                None => tmp_dir(state).map_err(|e| e.to_string())?.join(format!("upload-{}", random_token(12))),
            };
            let guard = TempFile(path.clone());
            let mut out = tokio::fs::File::create(&path).await.map_err(|e| format!("Can’t store the upload: {e}"))?;
            let mut field = field;
            let mut total = 0usize;
            loop {
                match field.chunk().await {
                    Ok(Some(bytes)) => {
                        total += bytes.len();
                        out.write_all(&bytes).await.map_err(|e| format!("Can’t store the upload: {e}"))?;
                    }
                    Ok(None) => break,
                    Err(e) => return Err(format!("The upload failed: {}", e.body_text())),
                }
            }
            out.flush().await.map_err(|e| e.to_string())?;
            if total > 0 {
                up.file = Some(guard);
                up.file_name = file_name;
            }
        } else {
            let text = field.text().await.map_err(|e| format!("The upload failed: {}", e.body_text()))?;
            up.fields.insert(name, text);
        }
    }
    Ok(up)
}

// ---------------------------------------------------------------------------
// Config import
// ---------------------------------------------------------------------------

async fn import_preview(State(state): State<AppState>, user: CurrentUser, mp: Multipart) -> ApiResult<Response> {
    user.require_admin()?;
    let up = match read_upload(&state, mp, "file", None).await {
        Ok(u) => u,
        Err(msg) => return Ok(partial(PlanPartial::problem(msg))),
    };
    let mode = Mode::parse(up.fields.get("mode").map(String::as_str).unwrap_or("merge")).unwrap_or_default();
    let (text, label) = match &up.file {
        Some(f) => {
            let meta = std::fs::metadata(&f.0).map(|m| m.len()).unwrap_or(0);
            if meta > super::config::MAX_DOCUMENT_BYTES as u64 {
                return Ok(partial(PlanPartial::problem(crate::i18n::t("backup.err_too_large"))));
            }
            match std::fs::read_to_string(&f.0) {
                Ok(t) => (t, if up.file_name.is_empty() { crate::i18n::t("backup.uploaded_file").to_string() } else { up.file_name.clone() }),
                Err(_) => return Ok(partial(PlanPartial::problem(crate::i18n::t("backup.err_not_text")))),
            }
        }
        None => (up.fields.get("document").cloned().unwrap_or_default(), crate::i18n::t("backup.pasted").to_string()),
    };
    if text.trim().is_empty() {
        return Ok(partial(PlanPartial::problem(crate::i18n::t("backup.err_nothing"))));
    }
    let doc = match config_io::parse_document(&text) {
        Ok(d) => d,
        Err(msg) => return Ok(partial(PlanPartial::problem(msg))),
    };
    let planned = doc.clone();
    let plan = state.with_conn(move |c| config_io::plan(c, &planned, mode)).await?;
    let token = stash(user.id, doc, mode, label.clone());
    Ok(partial(PlanPartial::from_plan(&plan, token, label, Vec::new(), Vec::new())))
}

#[derive(Debug, Deserialize)]
struct ApplyForm {
    #[serde(default)]
    token: String,
    #[serde(default)]
    confirm: bool,
}

async fn import_apply(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar, axum::Form(form): axum::Form<ApplyForm>) -> ApiResult<Response> {
    user.require_admin()?;
    let Some(pending) = take(&form.token, user.id) else {
        return Ok(partial(PlanPartial::problem(crate::i18n::t("backup.err_expired"))));
    };
    if pending.mode == Mode::Replace && !form.confirm {
        let token = stash(user.id, pending.doc, pending.mode, pending.label);
        let mut p = PlanPartial::problem(crate::i18n::t("backup.err_confirm"));
        p.token = token;
        return Ok(partial(p));
    }
    match config_io::import(&state, pending.doc, pending.mode, false).await {
        Ok(plan) => {
            let s = plan.summary();
            tracing::info!(user = %user.username, source = %pending.label, "configuration imported from the backup page");
            let msg = crate::i18n::tf(
                "backup.imported",
                &[
                    ("new", &s.create.to_string()),
                    ("changed", &s.update.to_string()),
                    ("deleted", &s.delete.to_string()),
                ],
            );
            let jar = flash(jar, "success", msg);
            Ok((jar, [("hx-refresh", "true")], StatusCode::OK).into_response())
        }
        Err(ImportError::Invalid(plan)) => Ok(partial(PlanPartial::from_plan(&plan, String::new(), pending.label, Vec::new(), Vec::new()))),
        Err(ImportError::Api(e)) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Uptime Kuma
// ---------------------------------------------------------------------------

async fn kuma_preview(State(state): State<AppState>, user: CurrentUser, mp: Multipart) -> ApiResult<Response> {
    user.require_admin()?;
    let up = match read_upload(&state, mp, "file", None).await {
        Ok(u) => u,
        Err(msg) => return Ok(partial(PlanPartial::problem(msg))),
    };
    let Some(file) = up.file else {
        return Ok(partial(PlanPartial::problem(crate::i18n::t("backup.err_kuma_nothing"))));
    };
    let path = file.0.clone();
    let read = tokio::task::spawn_blocking(move || kuma_import::read_file(&path)).await.map_err(ApiError::internal)?;
    drop(file);
    let mut import = match read {
        Ok(i) => i,
        Err(msg) => return Ok(partial(PlanPartial::problem(msg))),
    };
    let (plan, import) = state
        .with_conn(move |c| {
            let plan = kuma_import::plan(c, &mut import)?;
            Ok((plan, import))
        })
        .await?;
    let label = if import.source == "sqlite" {
        crate::i18n::t("backup.kuma_db").to_string()
    } else {
        crate::i18n::t("backup.kuma_json").to_string()
    };
    let token = stash(user.id, import.doc.clone(), Mode::Merge, label.clone());
    Ok(partial(PlanPartial::from_plan(&plan, token, label, import.skipped, import.warnings)))
}

// ---------------------------------------------------------------------------
// Restore on next start
// ---------------------------------------------------------------------------

async fn restore_upload(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar, mp: Multipart) -> ApiResult<Response> {
    user.require_admin()?;
    // Uploaded under a random name; only a validated file becomes the pending restore.
    let part = tmp_dir(&state).map_err(ApiError::internal)?.join(format!("upload-{}", random_token(12)));
    let up = match read_upload(&state, mp, "file", Some(part.clone())).await {
        Ok(u) => u,
        Err(msg) => return Ok(toast_error(&msg)),
    };
    let Some(file) = up.file else {
        return Ok(toast_error(crate::i18n::t("backup.err_no_file")));
    };
    let path = file.0.clone();
    let checked = tokio::task::spawn_blocking(move || config_io::validate_backup_db(&path)).await.map_err(ApiError::internal)?;
    let monitors = match checked {
        Ok(n) => n,
        Err(msg) => return Ok(toast_error(&msg)),
    };
    let target = restore_path(&state);
    std::fs::rename(&file.0, &target).map_err(ApiError::internal)?;
    std::mem::forget(file); // renamed: nothing left to delete
    tracing::warn!(user = %user.username, monitors, "database restore scheduled for the next start");
    let jar = flash(jar, "success", crate::i18n::tf("backup.checked", &[("n", &monitors.to_string())]));
    Ok((jar, [("hx-refresh", "true")], StatusCode::OK).into_response())
}

async fn restore_cancel(State(state): State<AppState>, user: CurrentUser, jar: SignedCookieJar) -> ApiResult<Response> {
    user.require_admin()?;
    let path = restore_path(&state);
    if path.exists() {
        std::fs::remove_file(&path).map_err(ApiError::internal)?;
    }
    let jar = flash(jar, "info", crate::i18n::t("backup.cancelled"));
    Ok((jar, [("hx-refresh", "true")], StatusCode::OK).into_response())
}

fn toast_error(message: &str) -> Response {
    let trigger = serde_json::json!({ "vv:toast": { "message": message, "kind": "error" } }).to_string();
    // Header values must be ASCII; escape the rest as JSON \u sequences.
    let ascii: String = trigger
        .chars()
        .flat_map(|c| {
            if c.is_ascii() {
                vec![c]
            } else {
                let mut buf = [0u16; 2];
                c.encode_utf16(&mut buf).iter().flat_map(|u| format!("\\u{u:04x}").chars().collect::<Vec<_>>()).collect()
            }
        })
        .collect();
    let mut res = StatusCode::OK.into_response();
    if let Ok(v) = HeaderValue::from_str(&ascii) {
        res.headers_mut().insert("hx-trigger", v);
    }
    res
}

