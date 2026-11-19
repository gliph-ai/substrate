//! Who is asking. Browser sessions (magic link or Sign in with Gliph), with a
//! CSRF secret per session; bearer tokens for MCP clients and relays.

use axum::extract::{FromRequestParts, Query, State};
use axum::http::request::Parts;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect, Response};
use axum::Form;
use base64::Engine;
use chrono::Duration;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::app::{audit, hash, id, now, token, App, AppInner, Fail};
use crate::web;

pub const SESSION_COOKIE: &str = "sub_s";
const GLIPH_COOKIE: &str = "sub_gl";

#[derive(Clone, Debug)]
pub struct Account {
    pub id: String,
    pub email: String,
    pub name: String,
    pub csrf: String,
}

impl Account {
    pub fn label(&self) -> String {
        if !self.name.is_empty() { self.name.clone() } else { self.email.split('@').next().unwrap_or("").to_string() }
    }
}

fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

pub fn set_cookie(app: &AppInner, name: &str, value: &str, max_age: i64) -> String {
    let secure = if app.cfg.secure_cookies() { "; Secure" } else { "" };
    format!("{name}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}")
}

/// The signed-in browser account, if any.
pub async fn session(app: &AppInner, headers: &HeaderMap) -> Option<Account> {
    let t = cookie(headers, SESSION_COOKIE)?;
    let row: Option<(String, String, String, String)> = sqlx::query_as(
        "SELECT a.id, a.email, a.name, t.extra FROM tokens t JOIN accounts a ON a.id=t.account_id
         WHERE t.hash=$1 AND t.kind='session' AND t.expires_at > now()",
    ).bind(hash(&t)).fetch_optional(&app.db).await.ok().flatten();
    row.map(|(id, email, name, csrf)| Account { id, email, name, csrf })
}

/// Extractor: a signed-in browser. Redirects to /login otherwise.
pub struct User(pub Account);

impl FromRequestParts<App> for User {
    type Rejection = Response;
    async fn from_request_parts(parts: &mut Parts, app: &App) -> Result<Self, Self::Rejection> {
        match session(app, &parts.headers).await {
            Some(a) => Ok(User(a)),
            None => {
                let next = parts.uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
                Err(Redirect::to(&format!("/login?next={}", urlencoding::encode(next))).into_response())
            }
        }
    }
}

/// Every state-changing browser POST: the form's csrf must match the session's,
/// and an Origin header, when the browser sends one, must be ours.
pub fn check_csrf(app: &AppInner, headers: &HeaderMap, acct: &Account, form_csrf: &str) -> Result<(), Fail> {
    if let Some(o) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        if o != app.cfg.public_url {
            return Err(Fail::new(403, "cross-origin request refused"));
        }
    }
    if form_csrf.is_empty() || !constant_eq(form_csrf, &acct.csrf) {
        return Err(Fail::new(403, "this form expired; reload the page and try again"));
    }
    Ok(())
}

fn same_origin(app: &AppInner, headers: &HeaderMap) -> bool {
    headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).map(|o| o == app.cfg.public_url).unwrap_or(true)
}

pub fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Only a path on this site may follow sign-in.
pub fn safe_next(n: &str) -> String {
    if n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\") && !n.contains("..") && !n.chars().any(|c| c.is_control()) {
        n.to_string()
    } else {
        "/".into()
    }
}

pub async fn start_session(app: &AppInner, account: &str) -> Result<String, Fail> {
    let t = token();
    sqlx::query("INSERT INTO tokens (hash, kind, account_id, extra, expires_at) VALUES ($1,'session',$2,$3,$4)")
        .bind(hash(&t)).bind(account).bind(crate::app::token_n(16)).bind(now() + Duration::days(30))
        .execute(&app.db).await?;
    Ok(set_cookie(app, SESSION_COOKIE, &t, 30 * 86400))
}

pub async fn upsert_account(app: &AppInner, email: &str) -> Result<String, Fail> {
    let email = email.trim().to_lowercase();
    let aid: String = sqlx::query_scalar(
        "INSERT INTO accounts (id, email) VALUES ($1,$2) ON CONFLICT (email) DO UPDATE SET email=EXCLUDED.email RETURNING id",
    ).bind(id("u")).bind(&email).fetch_one(&app.db).await?;
    Ok(aid)
}

pub fn valid_email(e: &str) -> bool {
    let e = e.trim();
    e.len() <= 254 && e.split_once('@').map(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.') && !e.contains(char::is_whitespace)).unwrap_or(false)
}

// ---- magic link ----

#[derive(Deserialize)]
pub struct LoginForm {
    email: String,
    #[serde(default)]
    next: String,
}

pub async fn login_post(State(app): State<App>, headers: HeaderMap, Form(f): Form<LoginForm>) -> Result<Response, Fail> {
    if !same_origin(&app, &headers) {
        return Err(Fail::new(403, "cross-origin request refused"));
    }
    let email = f.email.trim().to_lowercase();
    if !valid_email(&email) {
        return Ok(web::login_page(&app, &f.next, Some("That doesn't look like an email address.")).into_response());
    }
    let ip = crate::client_ip(&headers);
    if !app.limiter.allow(&format!("login-ip:{ip}"), 0.1, 10.0) || !app.limiter.allow(&format!("login:{email}"), 1.0 / 60.0, 3.0) {
        return Ok(web::login_page(&app, &f.next, Some("Too many sign-in links. Wait a few minutes.")).into_response());
    }
    let t = token();
    sqlx::query("INSERT INTO tokens (hash, kind, ref, extra, expires_at) VALUES ($1,'magic',$2,$3,$4)")
        .bind(hash(&t)).bind(&email).bind(safe_next(&f.next)).bind(now() + Duration::minutes(15))
        .execute(&app.db).await?;
    let link = format!("{}/auth/magic?t={t}", app.cfg.public_url);
    let msg = crate::mail::Message {
        subject: "Your Substrate sign-in link",
        title: "Sign in to Substrate",
        intro: "The button works once and expires in 15 minutes.",
        button: "Sign in",
        url: &link,
        note: "Didn't ask for it? Ignore this email.",
        code: "",
    };
    if let Err(e) = app.mail.send(&email, &msg).await {
        tracing::error!(error = %e, "magic link mail");
        return Ok(web::login_page(&app, &f.next, Some("We couldn't send the email. Try again in a minute.")).into_response());
    }
    if app.cfg.dev {
        tracing::warn!(%link, "dev sign-in link");
    }
    Ok(web::check_email_page(&email).into_response())
}

#[derive(Deserialize)]
pub struct MagicQ {
    t: String,
}

/// GET shows a button; only the POST spends the link. Mail scanners that
/// prefetch links then cannot sign anyone in or burn the link.
pub async fn magic_get(Query(q): Query<MagicQ>) -> Response {
    web::magic_confirm_page(&q.t).into_response()
}

pub async fn magic_post(State(app): State<App>, headers: HeaderMap, Form(q): Form<MagicQ>) -> Result<Response, Fail> {
    if !same_origin(&app, &headers) {
        return Err(Fail::new(403, "cross-origin request refused"));
    }
    let row: Option<(String, String)> = sqlx::query_as(
        "UPDATE tokens SET used=true WHERE hash=$1 AND kind='magic' AND NOT used AND expires_at > now() RETURNING ref, extra",
    ).bind(hash(&q.t)).fetch_optional(&app.db).await?;
    let Some((email, next)) = row else {
        return Ok(web::login_page(&app, "/", Some("That sign-in link has expired or was already used. Ask for a new one.")).into_response());
    };
    let aid = upsert_account(&app, &email).await?;
    let c = start_session(&app, &aid).await?;
    audit(&app, &aid, "", &email, "account.signin", "", "magic link").await;
    Ok(([(header::SET_COOKIE, c)], Redirect::to(&safe_next(&next))).into_response())
}

pub async fn logout(State(app): State<App>, headers: HeaderMap) -> Response {
    if same_origin(&app, &headers) {
        if let Some(t) = cookie(&headers, SESSION_COOKIE) {
            let _ = sqlx::query("DELETE FROM tokens WHERE hash=$1 AND kind='session'").bind(hash(&t)).execute(&app.db).await;
        }
    }
    ([(header::SET_COOKIE, set_cookie(&app, SESSION_COOKIE, "", 0))], Redirect::to("/")).into_response()
}

// ---- Sign in with Gliph ----

#[derive(Deserialize)]
pub struct GliphStart {
    #[serde(default)]
    next: String,
}

pub fn gliph_callback(app: &AppInner) -> String {
    format!("{}/auth/gliph/callback", app.cfg.public_url)
}

/// Send the browser to gliph.sh's own approval page (the device-link flow,
/// platform=substrate). PKCE verifier and state ride in a short-lived cookie.
pub async fn gliph_start(State(app): State<App>, Query(q): Query<GliphStart>) -> Response {
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(crate::app::token_n(48));
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = crate::app::token_n(16);
    let payload = format!("{state}.{verifier}.{}", urlencoding::encode(&safe_next(&q.next)));
    let signed = format!("{payload}.{}", app.sign(&format!("gl:{payload}")));
    let url = format!(
        "{}/link.html?device_name=Substrate&platform=substrate&redirect_uri={}&state={state}&code_challenge={challenge}&code_challenge_method=S256",
        app.cfg.gliph_url,
        urlencoding::encode(&gliph_callback(&app)),
    );
    ([(header::SET_COOKIE, set_cookie(&app, GLIPH_COOKIE, &signed, 600))], Redirect::to(&url)).into_response()
}

#[derive(Deserialize)]
pub struct GliphCb {
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
}

#[derive(Deserialize)]
struct GliphUser {
    #[serde(default)]
    id: String,
    #[serde(default)]
    nick: String,
    #[serde(default)]
    email: String,
}

pub async fn gliph_cb(State(app): State<App>, headers: HeaderMap, Query(q): Query<GliphCb>) -> Result<Response, Fail> {
    let bad = |m: &str| Ok(web::login_page(&app, "/", Some(m)).into_response());
    let Some(c) = cookie(&headers, GLIPH_COOKIE) else { return bad("Sign in with Gliph took too long. Try again.") };
    let parts: Vec<&str> = c.split('.').collect();
    if parts.len() != 4 || !app.verify(&format!("gl:{}.{}.{}", parts[0], parts[1], parts[2]), parts[3]) || !constant_eq(parts[0], &q.state) || q.code.is_empty() {
        return bad("Sign in with Gliph could not be verified. Try again.");
    }
    let (verifier, next) = (parts[1], urlencoding::decode(parts[2]).map(|s| s.into_owned()).unwrap_or_else(|_| "/".into()));
    let http = reqwest::Client::new();
    let r = http.post(format!("{}/api/auth/device-link/token", app.cfg.gliph_url))
        .json(&serde_json::json!({"code": q.code, "code_verifier": verifier, "redirect_uri": gliph_callback(&app)}))
        .send().await.map_err(|e| anyhow::anyhow!(e))?;
    if !r.status().is_success() {
        return bad("Gliph did not confirm the sign-in. Try again.");
    }
    #[derive(Deserialize)]
    struct Tok { token: String, user: GliphUser, #[serde(default)] email: String }
    let t: Tok = r.json().await.map_err(|e| anyhow::anyhow!(e))?;
    // We wanted the identity, not a Gliph session: sign that session out.
    let _ = http.post(format!("{}/api/logout", app.cfg.gliph_url)).bearer_auth(&t.token).send().await;
    // gliph keeps email off its public user object; the substrate redemption
    // carries it at the top level.
    let email = if t.email.is_empty() { t.user.email.clone() } else { t.email.clone() };
    if !valid_email(&email) {
        return bad("Your Gliph account has no email address to sign in with.");
    }
    let aid = upsert_account(&app, &email).await?;
    sqlx::query("UPDATE accounts SET gliph_user_id=$2, gliph_nick=$3, name=CASE WHEN name='' THEN $3 ELSE name END WHERE id=$1")
        .bind(&aid).bind(&t.user.id).bind(&t.user.nick).execute(&app.db).await?;
    // A nick belongs to one Substrate account at a time.
    sqlx::query("UPDATE accounts SET gliph_nick='' WHERE gliph_nick=$1 AND id<>$2").bind(&t.user.nick).bind(&aid).execute(&app.db).await?;
    let session = start_session(&app, &aid).await?;
    audit(&app, &aid, "", &t.user.nick, "account.signin", "", "gliph").await;
    let clear = set_cookie(&app, GLIPH_COOKIE, "", 0);
    let mut resp = Redirect::to(&safe_next(&next)).into_response();
    resp.headers_mut().append(header::SET_COOKIE, session.parse().unwrap());
    resp.headers_mut().append(header::SET_COOKIE, clear.parse().unwrap());
    Ok(resp)
}

// ---- bearer tokens (MCP clients, personal tokens) ----

/// The account behind an MCP request's bearer, and the client's name.
pub async fn mcp_bearer(app: &AppInner, headers: &HeaderMap) -> Option<(String, String)> {
    let t = headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")?.trim().to_string();
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT t.account_id, t.kind, COALESCE(c.name, '') FROM tokens t LEFT JOIN clients c ON c.id=t.client_id
         WHERE t.hash=$1 AND t.kind IN ('oauth_access','pat') AND t.expires_at > now()",
    ).bind(hash(&t)).fetch_optional(&app.db).await.ok().flatten();
    row.map(|(a, kind, client)| (a, if kind == "pat" { "personal token".into() } else { client }))
}

pub fn unauthorized_mcp(app: &AppInner) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\"", app.cfg.public_url))],
        axum::Json(serde_json::json!({"error": "unauthorized"})),
    ).into_response()
}
