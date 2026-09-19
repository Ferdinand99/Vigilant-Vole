use askama::Template;
use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::cookie::{Cookie, SignedCookieJar};
use serde::Deserialize;

use crate::auth::{self, SESSION_COOKIE};
use crate::db::users as db_users;

use super::{AppState, HtmlTemplate};

#[derive(Template)]
#[template(path = "setup.html")]
struct SetupTemplate {
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct SetupForm {
    username: String,
    password: String,
}

#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

async fn has_admin(state: &AppState) -> bool {
    let conn = state.db.get().await.expect("failed to get db connection");
    let count = conn
        .interact(|conn| db_users::count_users(conn))
        .await
        .expect("db task panicked")
        .expect("count query failed");
    count > 0
}

pub async fn setup_form(State(state): State<AppState>) -> impl IntoResponse {
    if has_admin(&state).await {
        return Redirect::to("/login").into_response();
    }
    HtmlTemplate(SetupTemplate { error: None }).into_response()
}

pub async fn setup_submit(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::Form(form): axum::Form<SetupForm>,
) -> impl IntoResponse {
    if has_admin(&state).await {
        return Redirect::to("/login").into_response();
    }
    if form.username.trim().is_empty() || form.password.len() < 8 {
        return HtmlTemplate(SetupTemplate {
            error: Some("Username is required and password must be at least 8 characters.".into()),
        })
        .into_response();
    }

    let username = form.username.trim().to_string();
    let password_hash = auth::hash_password(&form.password);
    let conn = state.db.get().await.expect("failed to get db connection");
    let user_id = conn
        .interact(move |conn| db_users::insert_user(conn, &username, &password_hash))
        .await
        .expect("db task panicked")
        .expect("failed to create admin user");

    let jar = jar.add(session_cookie(user_id));
    (jar, Redirect::to("/")).into_response()
}

pub async fn login_form(State(state): State<AppState>) -> impl IntoResponse {
    if !has_admin(&state).await {
        return Redirect::to("/setup").into_response();
    }
    HtmlTemplate(LoginTemplate { error: None }).into_response()
}

pub async fn login_submit(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    axum::Form(form): axum::Form<LoginForm>,
) -> impl IntoResponse {
    let conn = state.db.get().await.expect("failed to get db connection");
    let username = form.username.trim().to_string();
    let user = conn
        .interact(move |conn| db_users::get_user_by_username(conn, &username))
        .await
        .expect("db task panicked")
        .expect("query failed");

    match user {
        Some(user) if auth::verify_password(&user.password_hash, &form.password) => {
            let jar = jar.add(session_cookie(user.id));
            (jar, Redirect::to("/")).into_response()
        }
        _ => HtmlTemplate(LoginTemplate {
            error: Some("Invalid username or password.".into()),
        })
        .into_response(),
    }
}

pub async fn logout(jar: SignedCookieJar) -> impl IntoResponse {
    let jar = jar.remove(Cookie::from(SESSION_COOKIE));
    (jar, Redirect::to("/login"))
}

fn session_cookie(user_id: i64) -> Cookie<'static> {
    let mut cookie = Cookie::new(SESSION_COOKIE, user_id.to_string());
    cookie.set_path("/");
    cookie.set_http_only(true);
    cookie
}

/// Gate for every protected route: requires a validly-signed session cookie,
/// and bootstraps to /setup on a brand new install with no admin user yet.
pub async fn require_auth(
    State(state): State<AppState>,
    jar: SignedCookieJar,
    request: Request,
    next: Next,
) -> Response {
    if !has_admin(&state).await {
        return Redirect::to("/setup").into_response();
    }
    if jar.get(SESSION_COOKIE).is_some() {
        next.run(request).await
    } else {
        Redirect::to("/login").into_response()
    }
}
