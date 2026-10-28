//! Push: the proof of work against known vectors, registration and sending
//! against a fake relay, the device register, the stored texts and the three
//! routes (with read-only tokens).

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::middleware::from_fn_with_state;
use axum::routing::post;
use axum::{Json, response::IntoResponse};
use axum_extra::extract::cookie::Key;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt as _;

use super::relay::{self, Context};
use super::{api, devices, events};
use crate::auth::{CurrentUser, Role, TokenScope, guard, tokens};
use crate::crypto::{OpenError, Sealer};
use crate::db::{self, api_tokens, settings, users};
use crate::notify::alert::{AlertKind, NotificationChannel};
use crate::notify::tests::event;
use crate::state::Core;

// ---------------------------------------------------------------------------
// Proof of work
// ---------------------------------------------------------------------------

/// Nonces found by an independent implementation of the relay's documented
/// rule (`docs/api.md`, "POST /v1/instances"): the first counter in decimal
/// for which `SHA-256(challenge ‖ nonce)` has `difficulty` leading zero bits.
const VECTORS: &[(&str, u32, &str, &str)] = &[
    (
        "vole-kit-vector-1",
        8,
        "254",
        "009104651f058cd9c70df8aaa7a6b3837f1b36c1746d9eced3c4f4392d6c033c",
    ),
    (
        "vole-kit-vector-2",
        12,
        "100",
        "0000da159ef7c669915fa0089db3dc01bcff264db1206d6d64797cbfafee68c6",
    ),
    (
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        16,
        "5180",
        "0000c1388dd440efcb9e19186c5bcd7c08b8e8d0f9b2b990343dfeda76538a11",
    ),
    (
        "vole-kit-vector-3",
        20,
        "246834",
        "0000074e59a121b7ed4f62239a164b67b15e85c1a3eb79376c4b2586e2184626",
    ),
    (
        "x",
        0,
        "0",
        "b70a14ee1e15d7aa94bd810ec06f4cb77a346e8f33aef6bfeae3d7c4442d7a93",
    ),
];

fn zero_bits(digest: &[u8]) -> u32 {
    let mut bits = 0;
    for byte in digest {
        if *byte == 0 {
            bits += 8;
        } else {
            bits += byte.leading_zeros();
            break;
        }
    }
    bits
}

#[test]
fn the_proof_of_work_solver_matches_known_vectors() {
    let never = std::sync::atomic::AtomicBool::new(false);
    for (challenge, difficulty, nonce, digest) in VECTORS {
        assert_eq!(
            relay::solve(challenge, *difficulty, &never).as_deref(),
            Some(*nonce),
            "{challenge} at {difficulty} bits"
        );
        let hash = Sha256::digest([challenge.as_bytes(), nonce.as_bytes()].concat());
        assert_eq!(hex::encode(hash), *digest);
        assert!(zero_bits(&hash) >= *difficulty);
    }
}

#[test]
fn a_nonce_is_the_first_counter_that_works() {
    // One less than the vector's nonce does not meet the difficulty.
    let (challenge, difficulty, nonce, _) = VECTORS[2];
    for n in 0..nonce.parse::<u64>().unwrap() {
        let hash = Sha256::digest([challenge.as_bytes(), n.to_string().as_bytes()].concat());
        assert!(zero_bits(&hash) < difficulty, "{n}");
    }
}

#[test]
fn the_solver_stops_when_cancelled() {
    let cancel = std::sync::atomic::AtomicBool::new(true);
    // 60 bits would never finish; a cancelled search ends at once.
    assert_eq!(relay::solve("never", 60, &cancel), None);
}

#[test]
fn only_the_products_the_relay_knows_are_mapped() {
    for slug in ["vigilant-vole", "keen-kestrel", "wise-wren"] {
        assert_eq!(relay::relay_product(slug), Some(slug));
    }
    assert_eq!(relay::relay_product("kit-demo"), None);
    assert_eq!(relay::relay_product(""), None);
}

#[test]
fn relay_urls_are_https_or_local() {
    for ok in [
        "https://pigeon.hedgerow.tools",
        "https://pigeon.example.com/",
        "http://127.0.0.1:8080",
        "http://localhost:3000/",
        "http://10.0.3.26:8080",
        "http://[::1]:8080",
    ] {
        assert!(relay::validate_relay_url(ok).is_ok(), "{ok}");
    }
    assert_eq!(
        relay::validate_relay_url(" https://pigeon.example.com/// ").unwrap(),
        "https://pigeon.example.com"
    );
    for bad in [
        "http://pigeon.example.com",
        "http://8.8.8.8",
        "ftp://pigeon.example.com",
        "pigeon.example.com",
        "https://user:pass@pigeon.example.com",
        "https://pigeon.example.com/?token=1",
        "",
    ] {
        assert!(relay::validate_relay_url(bad).is_err(), "{bad}");
    }
}

// ---------------------------------------------------------------------------
// A fake relay
// ---------------------------------------------------------------------------

type PushScript = Box<dyn Fn(&Value) -> (StatusCode, Value) + Send + Sync>;

struct RelayState {
    difficulty: AtomicU32,
    challenges: AtomicUsize,
    registrations: Mutex<Vec<Value>>,
    credentials: Mutex<Vec<String>>,
    pushes: Mutex<Vec<(String, Value)>>,
    script: Mutex<Option<PushScript>>,
    push_delay_ms: AtomicU64,
    challenge_delay_ms: AtomicU64,
    /// Answer every push `401 unknown_instance`, whatever the credential.
    always_unknown: std::sync::atomic::AtomicBool,
}

struct FakeRelay {
    url: String,
    state: Arc<RelayState>,
}

impl FakeRelay {
    async fn start() -> FakeRelay {
        let state = Arc::new(RelayState {
            difficulty: AtomicU32::new(8),
            challenges: AtomicUsize::new(0),
            registrations: Mutex::default(),
            credentials: Mutex::default(),
            pushes: Mutex::default(),
            script: Mutex::default(),
            push_delay_ms: AtomicU64::new(0),
            challenge_delay_ms: AtomicU64::new(0),
            always_unknown: std::sync::atomic::AtomicBool::new(false),
        });
        let app = Router::new()
            .route("/v1/challenge", axum::routing::get(fake_challenge))
            .route("/v1/instances", post(fake_instances))
            .route("/v1/push", post(fake_push))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        FakeRelay {
            url: format!("http://{addr}"),
            state,
        }
    }

    fn script(&self, f: impl Fn(&Value) -> (StatusCode, Value) + Send + Sync + 'static) {
        *self.state.script.lock().unwrap() = Some(Box::new(f));
    }

    /// The relay loses its database: every credential becomes unknown.
    fn forget_everyone(&self) {
        self.state.credentials.lock().unwrap().clear();
    }

    fn challenges(&self) -> usize {
        self.state.challenges.load(Ordering::SeqCst)
    }

    fn registrations(&self) -> Vec<Value> {
        self.state.registrations.lock().unwrap().clone()
    }

    fn pushes(&self) -> Vec<(String, Value)> {
        self.state.pushes.lock().unwrap().clone()
    }
}

fn error(status: StatusCode, code: &str) -> axum::response::Response {
    (status, Json(json!({"error": code, "message": code}))).into_response()
}

async fn fake_challenge(State(st): State<Arc<RelayState>>) -> impl IntoResponse {
    let delay = st.challenge_delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let n = st.challenges.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "challenge": format!("challenge-{n}"),
        "difficulty": st.difficulty.load(Ordering::SeqCst),
        "expires_at": "2099-01-01T00:00:00Z",
    }))
}

async fn fake_instances(
    State(st): State<Arc<RelayState>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let challenge = body["challenge"].as_str().unwrap_or_default();
    let nonce = body["nonce"].as_str().unwrap_or_default();
    let hash = Sha256::digest([challenge.as_bytes(), nonce.as_bytes()].concat());
    if !challenge.starts_with("challenge-")
        || zero_bits(&hash) < st.difficulty.load(Ordering::SeqCst)
    {
        return error(StatusCode::BAD_REQUEST, "invalid_proof");
    }
    let mut registrations = st.registrations.lock().unwrap();
    let credential = format!("hrk_registered{}", registrations.len());
    registrations.push(body);
    st.credentials.lock().unwrap().push(credential.clone());
    (
        StatusCode::CREATED,
        Json(json!({
            "instance_credential": credential,
            "tier": "new",
            "limits": {"sends_per_hour": 600, "handles_per_request": 25, "unknown_handles_per_hour": 20},
        })),
    )
        .into_response()
}

async fn fake_push(
    State(st): State<Arc<RelayState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let delay = st.push_delay_ms.load(Ordering::SeqCst);
    if delay > 0 {
        tokio::time::sleep(Duration::from_millis(delay)).await;
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    if st.always_unknown.load(Ordering::SeqCst) || !st.credentials.lock().unwrap().contains(&bearer)
    {
        return error(StatusCode::UNAUTHORIZED, "unknown_instance");
    }
    st.pushes.lock().unwrap().push((bearer, body.clone()));
    if let Some(script) = st.script.lock().unwrap().as_ref() {
        let (status, reply) = script(&body);
        return (status, Json(reply)).into_response();
    }
    let n = body["handles"].as_array().map_or(0, Vec::len);
    let results: Vec<Value> = (0..n).map(|_| json!({"status": "accepted"})).collect();
    Json(json!({ "results": results })).into_response()
}

// ---------------------------------------------------------------------------
// A server: database, core, delivery context
// ---------------------------------------------------------------------------

static N: AtomicUsize = AtomicUsize::new(0);

struct World {
    core: Core,
    ctx: Arc<Context>,
    dir: std::path::PathBuf,
    relay: FakeRelay,
    user: i64,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl World {
    async fn new() -> World {
        Self::with_deadline(relay::DELIVERY_DEADLINE).await
    }

    async fn with_deadline(deadline: Duration) -> World {
        let dir = std::env::temp_dir().join(format!(
            "vole-kit-push-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pool = db::build_pool(&dir.join("kit.db"));
        db::migrate::run_migrations(&pool, &dir.join("backups")).await;
        let core = Core::new(
            pool,
            Key::generate(),
            reqwest::Client::new(),
            &crate::testing::CONFIG,
        );
        let relay = FakeRelay::start().await;
        let url = relay.url.clone();
        let user = core
            .with_conn(move |c| {
                settings::set(c, relay::SETTING_RELAY_URL, &url)?;
                users::insert_user(c, "ada", "x", Role::Admin)
            })
            .await
            .unwrap();
        let ctx = Arc::new(Context::new(&core).unwrap().with_deadline(deadline));
        World {
            core,
            ctx,
            dir,
            relay,
            user,
        }
    }

    /// Registers `n` devices, 16 to a user (the first user, then others);
    /// their handles.
    async fn add_devices(&self, n: usize) -> Vec<String> {
        let sealer = Sealer::new(&self.core.session_key, devices::PURPOSE);
        let first = self.user;
        let handles: Vec<String> = (0..n).map(|i| handle(1000 + i)).collect();
        let list = handles.clone();
        self.core
            .with_conn(move |c| {
                let per_user = devices::MAX_PER_USER as usize;
                let mut user = first;
                for (i, h) in list.iter().enumerate() {
                    if i > 0 && i % per_user == 0 {
                        user = users::insert_user(c, &format!("user{i}"), "x", Role::Viewer)?;
                    }
                    devices::register(c, &sealer, user, None, h)
                        .unwrap()
                        .unwrap();
                }
                Ok(())
            })
            .await
            .unwrap();
        handles
    }

    async fn deliver(&self, kind: AlertKind) -> Result<(), String> {
        relay::deliver(&self.ctx, &event(kind)).await
    }

    async fn device_count(&self) -> i64 {
        self.core.with_conn(|c| devices::count(c)).await.unwrap()
    }

    async fn setting(&self, key: &'static str) -> Option<String> {
        self.core
            .with_conn(move |c| settings::get(c, key))
            .await
            .unwrap()
    }

    async fn add_channel(&self, kind: &'static str, active: bool) {
        self.core
            .with_conn(move |c| {
                c.execute(
                    "INSERT INTO notification_channels (name, kind, config, active) VALUES (?1, ?1, '{}', ?2)",
                    rusqlite::params![kind, active],
                )
            })
            .await
            .unwrap();
    }
}

/// A handle shaped like the relay's: `hrh_` and 54 URL-safe characters.
fn handle(n: usize) -> String {
    format!("hrh_{n:A>54}")
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn handles_of(push: &Value) -> Vec<String> {
    push["handles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h.as_str().unwrap().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Registration and sending
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_first_push_registers_once_and_keeps_the_credential_sealed() {
    let w = World::new().await;
    w.add_devices(2).await;

    w.deliver(AlertKind::Problem).await.unwrap();
    w.deliver(AlertKind::Recovered).await.unwrap();

    // One registration, for the product, with a valid proof of work.
    assert_eq!(w.relay.challenges(), 1);
    let registrations = w.relay.registrations();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0]["product"], "vigilant-vole");
    assert_eq!(registrations[0]["version"], crate::testing::CONFIG.version);
    // Both pushes were sent with the credential the relay issued.
    let pushes = w.relay.pushes();
    assert_eq!(pushes.len(), 2);
    assert!(pushes.iter().all(|(bearer, _)| bearer == "hrk_registered0"));

    // The credential is sealed (purpose push-relay) and belongs to this relay.
    let stored = w.setting(relay::SETTING_CREDENTIAL).await.unwrap();
    assert!(crate::crypto::is_sealed(&stored), "{stored}");
    assert!(!stored.contains("registered0"));
    assert_eq!(
        crate::crypto::open(&w.core.session_key, "push-relay", &stored).unwrap(),
        "hrk_registered0"
    );
    assert_eq!(
        w.setting(relay::SETTING_CREDENTIAL_URL).await.unwrap(),
        w.relay.url
    );
}

#[tokio::test]
async fn a_credential_for_another_relay_is_not_used() {
    let w = World::new().await;
    w.add_devices(1).await;
    w.deliver(AlertKind::Problem).await.unwrap();
    assert_eq!(w.relay.registrations().len(), 1);
    // The administrator points the server at another relay: register there.
    let other = FakeRelay::start().await;
    let url = other.url.clone();
    w.core
        .with_conn(move |c| settings::set(c, relay::SETTING_RELAY_URL, &url))
        .await
        .unwrap();
    w.deliver(AlertKind::Problem).await.unwrap();
    assert_eq!(other.registrations().len(), 1);
    assert_eq!(w.relay.registrations().len(), 1);
    assert_eq!(other.pushes().len(), 1);
}

#[tokio::test]
async fn pushes_go_out_in_batches_of_25_with_one_ref_and_new_event_ids() {
    let w = World::new().await;
    let all = w.add_devices(60).await;

    w.deliver(AlertKind::Problem).await.unwrap();

    let pushes = w.relay.pushes();
    let sizes: Vec<usize> = pushes.iter().map(|(_, p)| handles_of(p).len()).collect();
    assert_eq!(sizes, [25, 25, 10]);
    let sent: Vec<String> = pushes.iter().flat_map(|(_, p)| handles_of(p)).collect();
    assert_eq!(sent, all);
    let reference = pushes[0].1["ref"].as_str().unwrap().to_string();
    let mut ids = Vec::new();
    for (_, push) in &pushes {
        assert_eq!(push["kind"], "alert");
        assert_eq!(push["ttl"], 3600);
        assert_eq!(push["ref"], reference);
        let id = push["event_id"].as_str().unwrap();
        assert!(is_uuid(id), "{id}");
        ids.push(id.to_string());
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 3, "a new event id for every call");
    // Every device was reached.
    let used: i64 = w
        .core
        .with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM push_devices WHERE last_used_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(used, 60);
}

#[tokio::test]
async fn the_ref_of_a_push_opens_the_text_of_the_alert() {
    let w = World::new().await;
    w.add_devices(1).await;
    let alert = event(AlertKind::Problem);
    relay::deliver(&w.ctx, &alert).await.unwrap();

    let reference = w.relay.pushes()[0].1["ref"].as_str().unwrap().to_string();
    assert_eq!(reference.len(), 22);
    assert!(events::valid_ref(&reference));
    let stored = w
        .core
        .with_conn(move |c| events::get(c, &reference))
        .await
        .unwrap()
        .expect("the text is stored under the ref");
    assert_eq!(stored.title, crate::notify::message::title(&alert));
    assert_eq!(stored.body, crate::notify::message::body_text(&alert));
    assert_eq!(stored.kind, "problem");
    assert_eq!(stored.level, "critical");
    assert!(stored.occurred_at.ends_with('Z'), "{}", stored.occurred_at);
    // Nothing of the text went to the relay.
    let sent = w.relay.pushes()[0].1.to_string();
    assert!(!sent.contains(&stored.title) && !sent.contains("title"));
}

#[tokio::test]
async fn gone_forgets_the_handle_and_accepted_marks_it_used() {
    let w = World::new().await;
    let all = w.add_devices(4).await;
    let gone = vec![all[1].clone(), all[3].clone()];
    let flagged = gone.clone();
    w.relay.script(move |push| {
        let results: Vec<Value> = handles_of(push)
            .iter()
            .map(|h| {
                if flagged.contains(h) {
                    json!({"status": "gone"})
                } else {
                    json!({"status": "accepted"})
                }
            })
            .collect();
        (StatusCode::OK, json!({ "results": results }))
    });

    w.deliver(AlertKind::Problem).await.unwrap();

    assert_eq!(w.device_count().await, 2);
    let left: Vec<String> = w
        .core
        .with_conn(|c| {
            let mut stmt = c.prepare("SELECT handle_hash FROM push_devices ORDER BY id")?;
            stmt.query_map([], |r| r.get(0))?.collect()
        })
        .await
        .unwrap();
    assert_eq!(left, [devices::hash(&all[0]), devices::hash(&all[2])]);
    // The next alert presents only the two that are left.
    w.deliver(AlertKind::Recovered).await.unwrap();
    assert_eq!(
        handles_of(&w.relay.pushes()[1].1),
        [all[0].clone(), all[2].clone()]
    );
}

#[tokio::test]
async fn all_handles_gone_is_not_an_error_and_a_retry_has_nothing_to_send() {
    let w = World::new().await;
    w.add_devices(3).await;
    w.relay.script(|push| {
        let results: Vec<Value> = handles_of(push)
            .iter()
            .map(|_| json!({"status": "gone"}))
            .collect();
        (StatusCode::OK, json!({ "results": results }))
    });
    // Nothing got through, but nothing a retry could change: Ok.
    w.deliver(AlertKind::Problem).await.unwrap();
    assert_eq!(w.device_count().await, 0);
    w.deliver(AlertKind::Problem).await.unwrap();
    assert_eq!(w.relay.pushes().len(), 1, "no devices, no second request");
}

#[tokio::test]
async fn a_push_that_reached_someone_is_never_an_error() {
    let w = World::new().await;
    let all = w.add_devices(4).await;
    let limited = all[2].clone();
    w.relay.script(move |push| {
        let results: Vec<Value> = handles_of(push)
            .iter()
            .map(|h| {
                if *h == limited {
                    json!({"status": "limited", "retry_after": 600})
                } else {
                    json!({"status": "accepted"})
                }
            })
            .collect();
        (StatusCode::OK, json!({ "results": results }))
    });
    // One handle was limited and gave up; the others were reached. An `Err`
    // here would make a product's retry push to them a second time.
    w.deliver(AlertKind::Problem).await.unwrap();
    assert_eq!(w.device_count().await, 4, "limited handles are kept");
}

#[tokio::test]
async fn nothing_delivered_for_a_transient_reason_is_an_error() {
    let w = World::new().await;
    w.add_devices(2).await;
    w.relay.script(|push| {
        let results: Vec<Value> = handles_of(push)
            .iter()
            .map(|_| json!({"status": "limited", "retry_after": 600}))
            .collect();
        (StatusCode::OK, json!({ "results": results }))
    });
    let err = w.deliver(AlertKind::Problem).await.unwrap_err();
    assert!(err.contains("600"), "{err}");

    w.relay.script(|_| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({"error": "overloaded", "message": "busy"}),
        )
    });
    let err = w.deliver(AlertKind::Problem).await.unwrap_err();
    assert!(err.contains("overloaded"), "{err}");

    w.relay.script(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            json!({"error": "rate_limited", "message": "slow down"}),
        )
    });
    let err = w.deliver(AlertKind::Problem).await.unwrap_err();
    assert!(err.contains("rate_limited"), "{err}");
    assert_eq!(w.device_count().await, 2);
}

#[tokio::test]
async fn unknown_statuses_count_as_unavailable() {
    let w = World::new().await;
    w.add_devices(1).await;
    w.relay
        .script(|_| (StatusCode::OK, json!({"results": [{"status": "teapot"}]})));
    assert!(w.deliver(AlertKind::Problem).await.is_err());
    // A reply with fewer results than handles leaves the rest unavailable.
    w.relay.script(|_| (StatusCode::OK, json!({"results": []})));
    assert!(w.deliver(AlertKind::Problem).await.is_err());
}

#[tokio::test]
async fn unknown_instance_registers_once_more_and_sends_again() {
    let w = World::new().await;
    w.add_devices(2).await;
    w.deliver(AlertKind::Problem).await.unwrap();
    assert_eq!(w.relay.registrations().len(), 1);

    // The relay lost its database.
    w.relay.forget_everyone();
    w.deliver(AlertKind::Recovered).await.unwrap();
    assert_eq!(w.relay.registrations().len(), 2);
    let pushes = w.relay.pushes();
    assert_eq!(pushes.len(), 2);
    assert_eq!(pushes[1].0, "hrk_registered1");
    // The new credential is the one kept.
    let stored = w.setting(relay::SETTING_CREDENTIAL).await.unwrap();
    assert_eq!(
        crate::crypto::open(&w.core.session_key, "push-relay", &stored).unwrap(),
        "hrk_registered1"
    );
}

#[tokio::test]
async fn a_relay_that_keeps_saying_unknown_instance_gets_one_new_registration() {
    let w = World::new().await;
    w.add_devices(1).await;
    w.deliver(AlertKind::Problem).await.unwrap();
    assert_eq!(w.relay.registrations().len(), 1);

    // Every credential it issues is unknown to it at once.
    w.relay.state.always_unknown.store(true, Ordering::SeqCst);
    let err = w.deliver(AlertKind::Recovered).await.unwrap_err();
    assert!(err.contains("unknown") || err.contains("know"), "{err}");
    // One registration more, not a loop.
    assert_eq!(w.relay.registrations().len(), 2);
    assert_eq!(w.relay.challenges(), 2);
}

#[tokio::test]
async fn suspended_stops_everything_without_registering_again() {
    let w = World::new().await;
    w.add_devices(2).await;
    w.deliver(AlertKind::Problem).await.unwrap();

    w.relay.script(|_| {
        (
            StatusCode::FORBIDDEN,
            json!({"error": "suspended", "message": "suspended"}),
        )
    });
    let err = w.deliver(AlertKind::Problem).await.unwrap_err();
    assert!(err.contains("suspended"), "{err}");
    let (pushes, registrations) = (w.relay.pushes().len(), w.relay.registrations().len());

    // Nothing is tried again for a while: no push, no registration.
    let err = w.deliver(AlertKind::Problem).await.unwrap_err();
    assert!(err.contains("suspended"), "{err}");
    assert_eq!(w.relay.pushes().len(), pushes);
    assert_eq!(w.relay.registrations().len(), registrations);
    assert_eq!(w.device_count().await, 2, "the devices stay");
}

#[tokio::test]
async fn a_slow_relay_hits_the_deadline_and_the_other_channels_still_go() {
    let w = World::with_deadline(Duration::from_millis(1500)).await;
    w.add_devices(2).await;
    w.deliver(AlertKind::Problem).await.unwrap();
    w.relay.state.push_delay_ms.store(10_000, Ordering::SeqCst);

    // A webhook channel next to the Hedgerow channel.
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let hook = Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            "ok"
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, hook).await;
    });
    let channel = |kind: &str, config: Value| NotificationChannel {
        id: 1,
        name: kind.into(),
        kind: kind.into(),
        config: config.to_string(),
        active: true,
        is_default: false,
        created_at: "2026-10-28 00:00:00".into(),
    };
    let hedgerow = channel("hedgerow", json!({}));
    let webhook = channel("webhook", json!({"url": format!("http://{addr}/hook")}));
    let e = event(AlertKind::Problem);
    let http = reqwest::Client::new();

    let started = Instant::now();
    let (pigeon, hook_result) = relay::scoped(w.ctx.clone(), async {
        tokio::join!(
            crate::notify::send(&http, &hedgerow, &e),
            crate::notify::send(&http, &webhook, &e)
        )
    })
    .await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(pigeon.is_err(), "{pigeon:?}");
    hook_result.unwrap();
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_slow_registration_gives_up_on_time_and_finishes_in_the_background() {
    let w = World::with_deadline(Duration::from_millis(500)).await;
    w.add_devices(1).await;
    w.relay
        .state
        .challenge_delay_ms
        .store(1500, Ordering::SeqCst);

    let started = Instant::now();
    let err = w.deliver(AlertKind::Problem).await.unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert!(!err.is_empty());
    assert!(w.relay.pushes().is_empty());

    // The registration goes on and is kept; the next push uses it.
    let mut kept = false;
    for _ in 0..100 {
        if w.setting(relay::SETTING_CREDENTIAL).await.is_some() {
            kept = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(kept, "the registration finished in the background");
    w.relay.state.challenge_delay_ms.store(0, Ordering::SeqCst);
    let again = Context::new(&w.core).unwrap();
    relay::deliver(&Arc::new(again), &event(AlertKind::Problem))
        .await
        .unwrap();
    assert_eq!(w.relay.challenges(), 1, "no second registration");
    assert_eq!(w.relay.pushes().len(), 1);
}

#[tokio::test]
async fn an_unreachable_relay_is_an_error_not_a_hang() {
    let w = World::new().await;
    w.add_devices(1).await;
    // Nothing listens on this port.
    w.core
        .with_conn(|c| settings::set(c, relay::SETTING_RELAY_URL, "http://127.0.0.1:9"))
        .await
        .unwrap();
    let started = Instant::now();
    let err = w.deliver(AlertKind::Problem).await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(15));
    assert!(err.contains("push relay"), "{err}");
    // The URL is not part of the message.
    assert!(!err.contains("127.0.0.1"), "{err}");
}

#[tokio::test]
async fn no_devices_is_fine_for_an_alert_and_a_hint_for_a_test() {
    let w = World::new().await;
    w.deliver(AlertKind::Problem).await.unwrap();
    let err = w.deliver(AlertKind::Test).await.unwrap_err();
    assert!(err.contains("Hedgerow"), "{err}");
    assert_eq!(w.relay.challenges(), 0, "no devices, no registration");
}

#[tokio::test]
async fn without_a_registered_context_the_channel_refuses_to_send() {
    let channel = NotificationChannel {
        id: 1,
        name: "app".into(),
        kind: "hedgerow".into(),
        config: "{}".into(),
        active: true,
        is_default: false,
        created_at: "2026-10-28 00:00:00".into(),
    };
    let err = crate::notify::send(
        &reqwest::Client::new(),
        &channel,
        &event(AlertKind::Problem),
    )
    .await
    .unwrap_err();
    assert!(err.contains("isn’t available"), "{err}");
    assert!(crate::notify::is_supported_kind("hedgerow"));
}

// ---------------------------------------------------------------------------
// Devices and texts in the database
// ---------------------------------------------------------------------------

#[tokio::test]
async fn handles_are_sealed_in_the_database() {
    let w = World::new().await;
    let [h] = w.add_devices(1).await.try_into().unwrap();
    let (sealed, digest): (String, String) = w
        .core
        .with_conn(|c| {
            c.query_row(
                "SELECT handle_sealed, handle_hash FROM push_devices",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .await
        .unwrap();
    assert!(crate::crypto::is_sealed(&sealed), "{sealed}");
    assert!(!sealed.contains(&h[4..]));
    assert_eq!(digest, hex::encode(Sha256::digest(h.as_bytes())));
    assert_eq!(
        crate::crypto::open(&w.core.session_key, devices::PURPOSE, &sealed).unwrap(),
        h
    );
    // Not under another purpose, nor another key.
    assert_eq!(
        crate::crypto::open(&w.core.session_key, "push-relay", &sealed),
        Err(OpenError::Unreadable)
    );
    assert_eq!(
        crate::crypto::open(&Key::generate(), devices::PURPOSE, &sealed),
        Err(OpenError::Unreadable)
    );
    // The raw table holds the handle nowhere in plain text.
    let dump: String = w
        .core
        .with_conn(|c| {
            c.query_row(
                "SELECT group_concat(handle_sealed || handle_hash || ifnull(token_id, ''), '|') FROM push_devices",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert!(!dump.contains(&h[4..]));
}

#[tokio::test]
async fn the_register_is_per_user_and_limited() {
    let w = World::new().await;
    let sealer = Sealer::new(&w.core.session_key, devices::PURPOSE);
    let user = w.user;
    let other = w
        .core
        .with_conn(|c| users::insert_user(c, "bob", "x", Role::Viewer))
        .await
        .unwrap();
    let outcome = w
        .core
        .with_conn(move |c| {
            let h = handle(1);
            let first = devices::register(c, &sealer, user, None, &h)?;
            let again = devices::register(c, &sealer, user, None, &h)?;
            // The same handle for another user is a separate registration.
            let theirs = devices::register(c, &sealer, other, None, &h)?;
            let mut last = Ok(devices::Registered::Created(0));
            for n in 2..=devices::MAX_PER_USER as usize + 1 {
                last = devices::register(c, &sealer, user, None, &handle(n))?;
            }
            Ok((first, again, theirs, last, devices::count(c)?))
        })
        .await
        .unwrap();
    let (first, again, theirs, last, count) = outcome;
    let first = first.unwrap();
    assert!(matches!(first, devices::Registered::Created(_)));
    assert_eq!(again.unwrap(), devices::Registered::Existing(first.id()));
    assert_ne!(theirs.unwrap().id(), first.id());
    assert_eq!(last, Err(devices::LimitReached::User));
    assert_eq!(count, devices::MAX_PER_USER + 1);
    // Two users with one handle get one push.
    let sealer = Sealer::new(&w.core.session_key, devices::PURPOSE);
    let (list, unreadable) = w
        .core
        .with_conn(move |c| devices::handles(c, &sealer))
        .await
        .unwrap();
    assert_eq!(unreadable, 0);
    assert_eq!(list.len() as i64, devices::MAX_PER_USER);
}

#[tokio::test]
async fn disabled_users_get_no_push() {
    let w = World::new().await;
    let handles = w.add_devices(2).await;
    let other = w
        .core
        .with_conn(|c| users::insert_user(c, "bob", "x", Role::Viewer))
        .await
        .unwrap();
    let sealer = Sealer::new(&w.core.session_key, devices::PURPOSE);
    let theirs = handle(5000);
    let h = theirs.clone();
    w.core
        .with_conn(move |c| devices::register(c, &sealer, other, None, &h).map(|_| ()))
        .await
        .unwrap();
    w.deliver(AlertKind::Problem).await.unwrap();
    assert_eq!(handles_of(&w.relay.pushes()[0].1).len(), 3);

    w.core
        .with_conn(move |c| c.execute("UPDATE users SET active = 0 WHERE id = ?1", [other]))
        .await
        .unwrap();
    w.deliver(AlertKind::Problem).await.unwrap();
    assert_eq!(handles_of(&w.relay.pushes()[1].1), handles);
    assert!(!w.relay.pushes()[1].1.to_string().contains(&theirs));
}

#[tokio::test]
async fn push_events_expire_after_24_hours() {
    let w = World::new().await;
    let make = |title: &str| events::Event {
        title: title.into(),
        body: "b".into(),
        kind: "problem".into(),
        level: "critical".into(),
        occurred_at: "2026-10-28T00:00:00Z".into(),
    };
    w.core
        .with_conn(move |c| {
            events::insert(c, "fresh", &make("fresh"))?;
            events::insert(c, "almost", &make("almost"))?;
            events::insert(c, "old", &make("old"))?;
            c.execute(
                "UPDATE push_events SET created_at = datetime('now', '-23 hours', '-50 minutes') WHERE ref = 'almost'",
                [],
            )?;
            c.execute(
                "UPDATE push_events SET created_at = datetime('now', '-24 hours', '-1 minutes') WHERE ref = 'old'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let seen = w
        .core
        .with_conn(|c| {
            Ok((
                events::get(c, "fresh")?.is_some(),
                events::get(c, "almost")?.is_some(),
                events::get(c, "old")?.is_some(),
                events::get(c, "unknown")?.is_some(),
            ))
        })
        .await
        .unwrap();
    assert_eq!(seen, (true, true, false, false));
    // The next insert drops what has expired.
    let rows: i64 = w
        .core
        .with_conn(|c| {
            events::insert(
                c,
                "next",
                &events::Event {
                    title: "n".into(),
                    body: "b".into(),
                    kind: "test".into(),
                    level: "info".into(),
                    occurred_at: "2026-10-28T00:00:00Z".into(),
                },
            )?;
            c.query_row(
                "SELECT COUNT(*) FROM push_events WHERE ref = 'old'",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(rows, 0);
}

#[test]
fn refs_are_random_and_url_safe() {
    let a = events::new_ref();
    let b = events::new_ref();
    assert_ne!(a, b);
    assert_eq!(a.len(), 22);
    assert!(events::valid_ref(&a));
    for bad in ["", "a b", "a/b", "../x", &"x".repeat(65), "é"] {
        assert!(!events::valid_ref(bad), "{bad:?}");
    }
}

#[test]
fn handles_must_look_like_the_relays() {
    assert!(devices::valid_handle(&handle(7)));
    for bad in [
        "",
        "hrh_",
        "hrh_short",
        "hrk_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "hrh_AAAAAAAAAAAAAAAAAAAA AAAAAAAA",
        "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        &format!("hrh_{}", "A".repeat(101)),
    ] {
        assert!(!devices::valid_handle(bad), "{bad:?}");
    }
}

// ---------------------------------------------------------------------------
// The routes
// ---------------------------------------------------------------------------

struct Api {
    world: World,
    router: Router,
    admin_write: String,
    admin_read: String,
    viewer_read: String,
    admin_read_token_id: i64,
}

async fn ping(user: CurrentUser) -> impl IntoResponse {
    match user.require_editor() {
        Ok(()) => (StatusCode::OK, "pong").into_response(),
        Err(e) => e.into_response(),
    }
}

impl Api {
    async fn new() -> Api {
        let world = World::new().await;
        let core = world.core.clone();
        let viewer = core
            .with_conn(|c| users::insert_user(c, "vera", "x", Role::Viewer))
            .await
            .unwrap();
        let admin = world.user;
        let make = |user: i64, scope: TokenScope| {
            let token = tokens::generate();
            (
                user,
                scope,
                tokens::hash(&token),
                tokens::display_prefix(&token),
                token,
            )
        };
        let specs = [
            make(admin, TokenScope::Write),
            make(admin, TokenScope::Read),
            make(viewer, TokenScope::Read),
        ];
        let rows: Vec<(String, i64)> = core
            .with_conn({
                let specs: Vec<_> = specs
                    .iter()
                    .map(|(u, s, h, p, _)| (*u, *s, h.clone(), p.clone()))
                    .collect();
                move |c| {
                    specs
                        .iter()
                        .map(|(u, s, h, p)| {
                            api_tokens::insert(c, *u, "t", p, h, *s, None).map(|id| (h.clone(), id))
                        })
                        .collect()
                }
            })
            .await
            .unwrap();
        let router = Router::new()
            .merge(api::router::<Core>())
            .route("/api/v1/ping", post(ping))
            .route_layer(from_fn_with_state(core.clone(), guard::require_auth))
            .layer(axum::middleware::from_fn(crate::http::error::negotiate))
            .with_state(core);
        Api {
            router,
            admin_write: specs[0].4.clone(),
            admin_read: specs[1].4.clone(),
            viewer_read: specs[2].4.clone(),
            admin_read_token_id: rows[1].1,
            world,
        }
    }

    async fn call(
        &self,
        token: Option<&str>,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(token) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match body {
            Some(v) => {
                req = req.header(header::CONTENT_TYPE, "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = self
            .router
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = to_bytes(res.into_body(), usize::MAX).await.unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, headers, json)
    }

    async fn register(&self, token: &str, h: &str) -> (StatusCode, HeaderMap, Value) {
        self.call(
            Some(token),
            "POST",
            "/api/v1/push/devices",
            Some(json!({ "handle": h })),
        )
        .await
    }
}

#[tokio::test]
async fn registering_creates_201_then_200_with_the_same_id() {
    let api = Api::new().await;
    api.world.add_channel("hedgerow", true).await;
    let h = handle(1);

    let (status, headers, body) = api.register(&api.admin_write, &h).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_i64().unwrap();
    assert_eq!(
        headers[header::LOCATION],
        format!("/api/v1/push/devices/{id}")
    );

    let (status, _, body) = api.register(&api.admin_write, &h).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"].as_i64().unwrap(), id);
    // Still one device, and the same handle with another token is the same device.
    let (status, _, body) = api.register(&api.admin_read, &h).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"].as_i64().unwrap(), id);
    assert_eq!(api.world.device_count().await, 1);
}

#[tokio::test]
async fn registering_without_an_active_hedgerow_channel_is_409_push_disabled() {
    let api = Api::new().await;
    let h = handle(1);
    for (kind, active) in [
        (None, false),
        (Some("webhook"), true),
        (Some("hedgerow"), false),
    ] {
        if let Some(kind) = kind {
            api.world.add_channel(kind, active).await;
        }
        let (status, _, body) = api.register(&api.admin_write, &h).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "push_disabled");
    }
    assert_eq!(api.world.device_count().await, 0);
    // An active channel turns push on.
    api.world.add_channel("hedgerow", true).await;
    let (status, _, _) = api.register(&api.admin_write, &h).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn bad_handles_and_too_many_devices_are_refused() {
    let api = Api::new().await;
    api.world.add_channel("hedgerow", true).await;
    for bad in ["", "nope", "hrk_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"] {
        let (status, _, body) = api.register(&api.admin_write, bad).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {body}");
        assert_eq!(body["error"]["code"], "validation_failed");
    }
    let (status, _, body) = api
        .call(
            Some(&api.admin_write),
            "POST",
            "/api/v1/push/devices",
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    for n in 0..devices::MAX_PER_USER as usize {
        let (status, _, _) = api.register(&api.admin_write, &handle(n)).await;
        assert_eq!(status, StatusCode::CREATED, "{n}");
    }
    let (status, _, body) = api.register(&api.admin_write, &handle(99)).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "too_many_devices");
    // Another user is not affected.
    let (status, _, _) = api.register(&api.viewer_read, &handle(99)).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn devices_can_only_be_deleted_by_their_owner() {
    let api = Api::new().await;
    api.world.add_channel("hedgerow", true).await;
    let (_, _, body) = api.register(&api.admin_write, &handle(1)).await;
    let id = body["id"].as_i64().unwrap();
    let path = format!("/api/v1/push/devices/{id}");

    // Another user (even with a token of their own) gets 404 and changes nothing.
    let (status, _, _) = api
        .call(Some(&api.viewer_read), "DELETE", &path, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(api.world.device_count().await, 1);
    // Unknown and malformed ids are 404 as well.
    for p in ["/api/v1/push/devices/999999", "/api/v1/push/devices/abc"] {
        let (status, _, _) = api.call(Some(&api.admin_write), "DELETE", p, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{p}");
    }
    // The owner, with any token of theirs, deletes it.
    let (status, _, _) = api.call(Some(&api.admin_read), "DELETE", &path, None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(api.world.device_count().await, 0);
    let (status, _, _) = api
        .call(Some(&api.admin_write), "DELETE", &path, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn read_only_tokens_reach_the_three_push_routes_and_nothing_else() {
    let api = Api::new().await;
    api.world.add_channel("hedgerow", true).await;
    for token in [&api.admin_read, &api.viewer_read] {
        // The three routes work.
        let (status, _, body) = api.register(token, &handle(5)).await;
        assert!(
            status == StatusCode::CREATED || status == StatusCode::OK,
            "{body}"
        );
        let id = body["id"].as_i64().unwrap();
        let reference = events::new_ref();
        let r = reference.clone();
        api.world
            .core
            .with_conn(move |c| {
                events::insert(
                    c,
                    &r,
                    &events::Event {
                        title: "t".into(),
                        body: "b".into(),
                        kind: "problem".into(),
                        level: "critical".into(),
                        occurred_at: "2026-10-28T00:00:00Z".into(),
                    },
                )
            })
            .await
            .unwrap();
        let (status, _, _) = api
            .call(
                Some(token),
                "GET",
                &format!("/api/v1/push/events/{reference}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = api
            .call(
                Some(token),
                "DELETE",
                &format!("/api/v1/push/devices/{id}"),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Nothing else: a route that needs an editor still refuses them,
        // even for the account of an admin; there is no way to list devices
        // or to change them in other ways.
        let (status, _, _) = api.call(Some(token), "POST", "/api/v1/ping", None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "ping");
        for (method, path) in [
            ("GET", "/api/v1/push/devices"),
            ("PUT", "/api/v1/push/devices"),
            ("PATCH", "/api/v1/push/devices"),
            ("DELETE", "/api/v1/push/devices"),
            ("POST", "/api/v1/push/devices/1"),
            ("PUT", "/api/v1/push/devices/1"),
            ("POST", "/api/v1/push/events/abc"),
            ("DELETE", "/api/v1/push/events/abc"),
            ("GET", "/api/v1/push"),
            ("POST", "/api/v1/push/send"),
        ] {
            let (status, _, _) = api.call(Some(token), method, path, None).await;
            assert!(
                matches!(
                    status,
                    StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::FORBIDDEN
                ),
                "{method} {path}: {status}"
            );
        }
    }
    // The same token can still do what an editor token can: the exception is
    // the routes, not the token.
    let (status, _, _) = api
        .call(Some(&api.admin_write), "POST", "/api/v1/ping", None)
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn the_push_routes_need_a_signed_in_caller() {
    let api = Api::new().await;
    api.world.add_channel("hedgerow", true).await;
    for (method, path) in [
        ("POST", "/api/v1/push/devices"),
        ("DELETE", "/api/v1/push/devices/1"),
        ("GET", "/api/v1/push/events/abc"),
    ] {
        let (status, _, _) = api
            .call(None, method, path, Some(json!({"handle": handle(1)})))
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {path}");
        let (status, _, _) = api.call(Some("vv_notatoken"), method, path, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {path} with a bad token"
        );
    }
    assert_eq!(api.world.device_count().await, 0);
}

#[tokio::test]
async fn the_event_route_serves_the_text_until_it_expires() {
    let api = Api::new().await;
    let reference = events::new_ref();
    let r = reference.clone();
    api.world
        .core
        .with_conn(move |c| {
            events::insert(
                c,
                &r,
                &events::Event {
                    title: "🔴 api is down".into(),
                    body: "HTTP 503".into(),
                    kind: "problem".into(),
                    level: "critical".into(),
                    occurred_at: "2026-10-28T09:41:07Z".into(),
                },
            )
        })
        .await
        .unwrap();
    let path = format!("/api/v1/push/events/{reference}");
    let (status, headers, body) = api.call(Some(&api.viewer_read), "GET", &path, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "title": "🔴 api is down",
            "body": "HTTP 503",
            "kind": "problem",
            "level": "critical",
            "occurred_at": "2026-10-28T09:41:07Z",
        })
    );
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");

    let (status, _, body) = api
        .call(
            Some(&api.viewer_read),
            "GET",
            "/api/v1/push/events/unknownref",
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");

    let r = reference.clone();
    api.world
        .core
        .with_conn(move |c| {
            c.execute(
                "UPDATE push_events SET created_at = datetime('now', '-25 hours') WHERE ref = ?1",
                [r],
            )
        })
        .await
        .unwrap();
    let (status, _, _) = api.call(Some(&api.viewer_read), "GET", &path, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn deleting_the_token_or_the_user_removes_the_devices() {
    let api = Api::new().await;
    api.world.add_channel("hedgerow", true).await;
    api.register(&api.admin_read, &handle(1)).await;
    api.register(&api.admin_write, &handle(2)).await;
    let (status, _, _) = api.register(&api.viewer_read, &handle(3)).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(api.world.device_count().await, 3);

    // The token a device registered with goes: so does the device.
    let token_id = api.admin_read_token_id;
    api.world
        .core
        .with_conn(move |c| api_tokens::delete(c, token_id))
        .await
        .unwrap();
    assert_eq!(api.world.device_count().await, 2);

    // The user goes: so do their devices (and tokens).
    let viewer = api
        .world
        .core
        .with_conn(|c| Ok(users::get_user_by_username(c, "vera")?.unwrap().id))
        .await
        .unwrap();
    api.world
        .core
        .with_conn(move |c| c.execute("DELETE FROM users WHERE id = ?1", [viewer]))
        .await
        .unwrap();
    assert_eq!(api.world.device_count().await, 1);
}

#[test]
fn the_route_docs_match_the_router() {
    let docs = api::ROUTE_DOCS;
    let listed: Vec<(&str, &str)> = docs.iter().map(|r| (r.method, r.path)).collect();
    assert_eq!(
        listed,
        [
            ("POST", "/api/v1/push/devices"),
            ("DELETE", "/api/v1/push/devices/{id}"),
            ("GET", "/api/v1/push/events/{ref}"),
        ]
    );
    assert!(
        docs.iter()
            .all(|r| r.access == "viewer" && r.group == "Push")
    );
}
