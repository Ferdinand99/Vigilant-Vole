//! Shared by the upgrade and backup/restore drills: fixture loading, temp
//! data directories, an in-process server on a free port, a tiny HTTP client
//! that keeps cookies, and whole-database comparisons.

#![allow(dead_code)] // each test binary uses a different subset

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use rusqlite::{Connection, OpenFlags, types::Value as SqlValue};
use serde_json::Value;
use vigilant_vole::api::{self, AppState};
use vigilant_vole::config::Config;

pub const DB_FILE: &str = "vigilant-vole.db";

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A database saved by a released version (see tools/upgrade-fixtures).
pub struct Fixture {
    /// Directory name, e.g. `v0.4.0`.
    pub version: String,
    pub db: PathBuf,
    pub manifest: Value,
}

impl Fixture {
    pub fn credential(&self, pointer: &str) -> String {
        self.manifest["credentials"]
            .pointer(pointer)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{}: manifest has no credentials{pointer}", self.version))
            .to_string()
    }

    pub fn count(&self, table: &str) -> i64 {
        self.manifest["counts"][table].as_i64().unwrap_or(0)
    }

    /// Migrations the release had applied.
    pub fn migrations(&self) -> Vec<String> {
        self.manifest["migrations"]
            .as_array()
            .expect("manifest migrations")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    }

    /// Id of a seeded monitor, by name (absent when the release lacked its type).
    pub fn monitor_id(&self, name: &str) -> Option<i64> {
        self.manifest["ids"]["monitors"][name].as_i64()
    }
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/upgrade")
}

/// Every fixture, oldest first.
pub fn fixtures() -> Vec<Fixture> {
    let mut list: Vec<Fixture> = std::fs::read_dir(fixtures_root())
        .expect("tests/fixtures/upgrade exists")
        .filter_map(Result::ok)
        .filter(|e| e.path().join("manifest.json").exists())
        .map(|e| {
            let dir = e.path();
            let manifest: Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                    .expect("manifest is JSON");
            Fixture {
                version: e.file_name().to_string_lossy().into_owned(),
                db: dir.join(DB_FILE),
                manifest,
            }
        })
        .collect();
    list.sort_by_key(|f| version_key(&f.version));
    list
}

fn version_key(v: &str) -> Vec<u64> {
    v.trim_start_matches('v')
        .split('.')
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

/// Migration files in this tree, in order (what a fresh database gets).
pub fn embedded_migrations() -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".sql"))
            .collect();
    names.sort();
    names
}

// ---------------------------------------------------------------------------
// Temp data directories
// ---------------------------------------------------------------------------

/// A data directory under the system temp dir, removed on drop.
pub struct DataDir(pub PathBuf);

impl DataDir {
    pub fn new(label: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vv-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        DataDir(dir)
    }

    /// A data directory holding a copy of the fixture's database.
    pub fn with_fixture(f: &Fixture) -> Self {
        let dir = Self::new(&format!("up-{}", f.version));
        std::fs::copy(&f.db, dir.db()).unwrap();
        dir
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn db(&self) -> PathBuf {
        self.0.join(DB_FILE)
    }

    pub fn backups(&self) -> PathBuf {
        self.0.join("backups")
    }

    /// Files in `backups/` (sorted).
    pub fn backup_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.backups())
            .map(|rd| rd.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default();
        files.retain(|p| p.extension().is_some_and(|e| e == "db"));
        files.sort();
        files
    }

    pub fn config(&self) -> Config {
        Config {
            data_dir: self.0.clone(),
            bind_addr: "127.0.0.1:0".into(),
        }
    }
}

impl Drop for DataDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Database helpers
// ---------------------------------------------------------------------------

pub fn open_ro(path: &Path) -> Connection {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap()
}

pub fn tables(conn: &Connection) -> Vec<String> {
    conn.prepare(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .unwrap()
    .query_map([], |r| r.get(0))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

pub fn columns(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare(&format!(
        "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
    ))
    .unwrap()
    .query_map([], |r| r.get(0))
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

pub fn count(conn: &Connection, table: &str) -> i64 {
    conn.query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
        r.get(0)
    })
    .unwrap()
}

pub fn applied_migrations(conn: &Connection) -> Vec<String> {
    conn.prepare("SELECT name FROM schema_migrations ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Every schema object (tables, indexes, triggers, views) except the
/// migration bookkeeping table: `(type, name, table, sql)`. Line endings are
/// normalised: SQLite keeps statements as written, and some release images
/// were built from a checkout with CRLF migration files.
pub fn schema(conn: &Connection) -> Vec<(String, String, String, String)> {
    conn.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_master
         WHERE sql IS NOT NULL AND name != 'schema_migrations' AND name NOT LIKE 'sqlite_%'
         ORDER BY type, name",
    )
    .unwrap()
    .query_map([], |r| {
        let sql: String = r.get(3)?;
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, sql.replace("\r\n", "\n")))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// The row key of `table`: its primary key columns, or `rowid` without one.
fn key_columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut pk: Vec<(i64, String)> = conn
        .prepare(&format!(
            "SELECT pk, name FROM pragma_table_info('{table}') WHERE pk > 0"
        ))
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    pk.sort();
    if pk.is_empty() {
        vec!["rowid".into()]
    } else {
        pk.into_iter().map(|(_, n)| format!("\"{n}\"")).collect()
    }
}

/// All rows of `table` keyed by primary key (rendered as text), restricted
/// to `cols`.
pub fn rows(conn: &Connection, table: &str, cols: &[String]) -> BTreeMap<String, Vec<SqlValue>> {
    let key = key_columns(conn, table);
    let mut select: Vec<String> = key.clone();
    select.extend(cols.iter().map(|c| format!("\"{c}\"")));
    let mut stmt = conn
        .prepare(&format!("SELECT {} FROM \"{table}\"", select.join(", ")))
        .unwrap();
    let (k, n) = (key.len(), cols.len());
    stmt.query_map([], |r| {
        let id = (0..k)
            .map(|i| r.get::<_, SqlValue>(i).map(|v| format!("{v:?}")))
            .collect::<Result<Vec<_>, _>>()?
            .join("/");
        let values = (0..n)
            .map(|i| r.get::<_, SqlValue>(k + i))
            .collect::<Result<Vec<_>, _>>()?;
        Ok((id, values))
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

/// A consistent single-file copy of a (possibly open, WAL-mode) database.
pub fn copy_db(from: &Path, to: &Path) {
    let _ = std::fs::remove_file(to);
    let conn = open_ro(from);
    conn.execute("VACUUM INTO ?1", [to.to_str().unwrap()])
        .unwrap();
}

/// Waits until SQLite has closed `db` (the last connection removes the WAL
/// file), as when the process exits. Pooled connections close on blocking
/// threads, after any monitor task that still uses one has been cancelled.
pub async fn wait_closed(db: &Path) {
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    for _ in 0..1200 {
        if !wal.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("{} is still open", db.display());
}

/// Every row of every table in `a` compared with `b` (all columns). Returns
/// the differences, empty when the databases hold the same data.
pub fn diff_databases(a: &Connection, b: &Connection) -> Vec<String> {
    let mut out = Vec::new();
    let (ta, tb) = (tables(a), tables(b));
    if ta != tb {
        out.push(format!("tables differ: {ta:?} vs {tb:?}"));
        return out;
    }
    for t in ta {
        let cols = columns(a, &t);
        if cols != columns(b, &t) {
            out.push(format!("{t}: columns differ"));
            continue;
        }
        if rows(a, &t, &cols) != rows(b, &t, &cols) {
            out.push(format!("{t}: rows differ"));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// In-process server
// ---------------------------------------------------------------------------

/// The real app (startup path, router, middleware) on `127.0.0.1:<free port>`.
/// The scheduler isn't started: checks would add heartbeats while tests compare data.
pub struct Server {
    pub addr: SocketAddr,
    pub state: AppState,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Server {
    pub async fn start(dir: &DataDir) -> Result<Server, vigilant_vole::db::UpgradeError> {
        let state = vigilant_vole::server::prepare(dir.config()).await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = api::build_router(state.clone());
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        });
        Ok(Server {
            addr,
            state,
            stop: Some(tx),
            task: Some(task),
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn client(&self) -> Client {
        Client::new(self)
    }

    /// Stops serving, stops the scheduler and closes the connection pool.
    pub async fn stop(mut self) {
        self.shutdown().await;
    }

    /// [`Server::stop`], then waits until the database file is closed, so it
    /// can be moved as on a real restart. Only for instances whose scheduler
    /// never ran monitors (a cancelled check can keep its connection a while).
    pub async fn stop_and_release(mut self) {
        self.shutdown().await;
        wait_closed(&self.state.config.db_path()).await;
    }

    async fn shutdown(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        self.state.scheduler.shutdown().await;
        self.state.db.close();
    }
}

/// HTTP client that keeps cookies and never follows redirects. Requests carry
/// an `Origin` of the server (as a browser would), which the CSRF guard needs.
pub struct Client {
    http: reqwest::Client,
    base: String,
    cookies: BTreeMap<String, String>,
}

pub struct Response {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub body: Vec<u8>,
}

impl Response {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }

    pub fn location(&self) -> String {
        self.headers
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }
}

impl Client {
    pub fn new(server: &Server) -> Self {
        Client {
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()
                .unwrap(),
            base: format!("http://{}", server.addr),
            cookies: BTreeMap::new(),
        }
    }

    fn prepare(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let req = req.header("origin", &self.base);
        if self.cookies.is_empty() {
            return req;
        }
        let cookie = self
            .cookies
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("; ");
        req.header("cookie", cookie)
    }

    pub async fn send(&mut self, req: reqwest::RequestBuilder) -> Response {
        let res = self.prepare(req).send().await.unwrap();
        for v in res.headers().get_all("set-cookie") {
            let Ok(v) = v.to_str() else { continue };
            let pair = v.split(';').next().unwrap_or_default();
            if let Some((k, val)) = pair.split_once('=') {
                if val.is_empty() || v.contains("Max-Age=0") {
                    self.cookies.remove(k);
                } else {
                    self.cookies.insert(k.to_string(), val.to_string());
                }
            }
        }
        let status = res.status().as_u16();
        let headers = res.headers().clone();
        let body = res.bytes().await.unwrap().to_vec();
        Response {
            status,
            headers,
            body,
        }
    }

    pub async fn get(&mut self, path: &str) -> Response {
        let req = self.http.get(format!("{}{path}", self.base));
        self.send(req).await
    }

    pub async fn get_with(&mut self, path: &str, headers: &[(&str, &str)]) -> Response {
        let mut req = self.http.get(format!("{}{path}", self.base));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        self.send(req).await
    }

    pub async fn post_form(&mut self, path: &str, form: &[(&str, &str)]) -> Response {
        let req = self.http.post(format!("{}{path}", self.base)).form(form);
        self.send(req).await
    }

    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        self.http.request(method, format!("{}{path}", self.base))
    }

    /// Signs in with a password (and a TOTP secret when the account has 2FA).
    /// Returns the final response of the login flow.
    pub async fn login(&mut self, username: &str, password: &str, totp: Option<&str>) -> Response {
        let mut res = self
            .post_form("/login", &[("username", username), ("password", password)])
            .await;
        // The login rate limiter is per process and counts attempts up front,
        // so tests signing in as the same user in parallel can be told to wait.
        for _ in 0..20 {
            if res.status == 303 || !res.text().contains("Too many attempts") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            res = self
                .post_form("/login", &[("username", username), ("password", password)])
                .await;
        }
        assert_eq!(res.status, 303, "login {username}: {}", res.text());
        let Some(secret) = totp else {
            return res;
        };
        assert!(
            res.location().starts_with("/login/2fa"),
            "{username} should be asked for a code, went to {}",
            res.location()
        );
        let code = totp_now(secret, username);
        self.post_form("/login/2fa", &[("code", &code)]).await
    }
}

/// The current TOTP code for a base32 secret (SHA-1, 6 digits, 30 s).
pub fn totp_now(secret: &str, account: &str) -> String {
    let totp = vigilant_vole::auth::totp::build(secret, account, "Vole Fixture").unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    vigilant_vole::auth::totp::code_at(&totp, now)
}
