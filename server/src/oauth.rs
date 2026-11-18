//! OAuth 2.1 for MCP clients (Claude custom connectors, ChatGPT developer
//! mode, Claude Code, any client): protected-resource and authorization-server
//! metadata, dynamic client registration, authorization code + PKCE (S256
//! only), rotating refresh tokens. Public clients only; no client secrets.

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Redirect, Response};
use axum::{Form, Json};
use base64::Engine;
use chrono::Duration;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::app::{audit, hash, id, now, token, App, AppInner, Fail};
use crate::auth::{check_csrf, constant_eq, User};
use crate::web;

pub async fn protected_resource(State(app): State<App>) -> Json<Value> {
    Json(json!({
        "resource": format!("{}/mcp", app.cfg.public_url),
        "authorization_servers": [app.cfg.public_url],
        "scopes_supported": ["substrate"],
        "bearer_methods_supported": ["header"],
        "resource_name": "Substrate",
    }))
}

pub async fn auth_server(State(app): State<App>) -> Json<Value> {
    let u = &app.cfg.public_url;
    Json(json!({
        "issuer": u,
        "authorization_endpoint": format!("{u}/oauth/authorize"),
        "token_endpoint": format!("{u}/oauth/token"),
        "registration_endpoint": format!("{u}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": ["substrate"],
    }))
}

fn redirect_ok(u: &str) -> bool {
    let Ok(p) = url::Url::parse(u) else { return false };
    if p.fragment().is_some() {
        return false;
    }
    match p.scheme() {
        "https" => true,
        "http" => matches!(p.host_str(), Some("localhost") | Some("127.0.0.1") | Some("[::1]")),
        // Native apps register private-use schemes (RFC 8252); never javascript:/data:.
        s => s.contains('.') && !matches!(s, "javascript" | "data" | "file" | "vbscript"),
    }
}

#[derive(Deserialize)]
pub struct Register {
    #[serde(default)]
    client_name: String,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

pub async fn register(State(app): State<App>, headers: HeaderMap, Json(r): Json<Register>) -> Result<Response, Fail> {
    if !app.limiter.allow(&format!("dcr:{}", crate::client_ip(&headers)), 0.05, 20.0) {
        return Err(Fail::new(429, "too many registrations"));
    }
    if r.redirect_uris.is_empty() || r.redirect_uris.len() > 10 || !r.redirect_uris.iter().all(|u| u.len() < 2000 && redirect_ok(u)) {
        return Ok((axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": "invalid_redirect_uri"}))).into_response());
    }
    let name = substrate_proto::sanitize(&r.client_name, 64);
    let name = if name.is_empty() { "MCP client".to_string() } else { name };
    let cid = id("cl");
    sqlx::query("INSERT INTO clients (id, name, redirect_uris) VALUES ($1,$2,$3)").bind(&cid).bind(&name).bind(json!(r.redirect_uris)).execute(&app.db).await?;
    Ok((axum::http::StatusCode::CREATED, Json(json!({
        "client_id": cid, "client_name": name, "redirect_uris": r.redirect_uris,
        "grant_types": ["authorization_code", "refresh_token"], "response_types": ["code"],
        "token_endpoint_auth_method": "none", "client_id_issued_at": now().timestamp(),
    }))).into_response())
}

#[derive(Deserialize, Clone)]
pub struct AuthorizeQ {
    #[serde(default)]
    pub response_type: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub redirect_uri: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub code_challenge: String,
    #[serde(default)]
    pub code_challenge_method: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub resource: String,
}

async fn client(app: &AppInner, cid: &str) -> Result<Option<(String, Vec<String>)>, Fail> {
    let r: Option<(String, Value)> = sqlx::query_as("SELECT name, redirect_uris FROM clients WHERE id=$1").bind(cid).fetch_optional(&app.db).await?;
    Ok(r.map(|(n, v)| (n, serde_json::from_value(v).unwrap_or_default())))
}

/// Validate before showing anything: a bad client or redirect is an error
/// page, never a redirect (no open redirector).
async fn validate(app: &AppInner, q: &AuthorizeQ) -> Result<String, String> {
    let Some((name, uris)) = client(app, &q.client_id).await.map_err(|_| "internal error".to_string())? else {
        return Err("This app is not registered with Substrate. Remove the connector and add it again.".into());
    };
    if !uris.iter().any(|u| u == &q.redirect_uri) {
        return Err("This app asked to send you somewhere it did not register. Nothing was shared.".into());
    }
    if q.response_type != "code" || q.code_challenge_method != "S256" || q.code_challenge.len() < 43 {
        return Err("This app's sign-in request is incomplete (it needs PKCE with S256).".into());
    }
    Ok(name)
}

pub async fn authorize(State(app): State<App>, headers: HeaderMap, Query(q): Query<AuthorizeQ>) -> Result<Response, Fail> {
    let name = match validate(&app, &q).await {
        Ok(n) => n,
        Err(e) => return Ok(web::error_page("Can't connect this app", &e).into_response()),
    };
    let Some(acct) = crate::auth::session(&app, &headers).await else {
        let here = format!("/oauth/authorize?{}", serde_urlencoded_q(&q));
        return Ok(Redirect::to(&format!("/login?next={}", urlencoding::encode(&here))).into_response());
    };
    let projects: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT p.name, p.handle, m.role FROM members m JOIN projects p ON p.id=m.project_id WHERE m.account_id=$1 ORDER BY p.name",
    ).bind(&acct.id).fetch_all(&app.db).await?;
    Ok(web::consent_page(&acct, &name, &q, &projects).into_response())
}

pub fn serde_urlencoded_q(q: &AuthorizeQ) -> String {
    [("response_type", &q.response_type), ("client_id", &q.client_id), ("redirect_uri", &q.redirect_uri), ("state", &q.state),
     ("code_challenge", &q.code_challenge), ("code_challenge_method", &q.code_challenge_method), ("scope", &q.scope), ("resource", &q.resource)]
        .iter().map(|(k, v)| format!("{k}={}", urlencoding::encode(v))).collect::<Vec<_>>().join("&")
}

#[derive(Deserialize)]
pub struct Decision {
    csrf: String,
    decision: String,
    #[serde(flatten)]
    q: AuthorizeQ,
}

pub async fn authorize_post(State(app): State<App>, headers: HeaderMap, User(acct): User, Form(d): Form<Decision>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &acct, &d.csrf)?;
    let name = match validate(&app, &d.q).await {
        Ok(n) => n,
        Err(e) => return Ok(web::error_page("Can't connect this app", &e).into_response()),
    };
    let mut target = url::Url::parse(&d.q.redirect_uri).map_err(|_| Fail::new(400, "bad redirect"))?;
    if d.decision != "allow" {
        target.query_pairs_mut().append_pair("error", "access_denied").append_pair("state", &d.q.state);
        return Ok(Redirect::to(target.as_str()).into_response());
    }
    let code = token();
    let extra = json!({"challenge": d.q.code_challenge, "redirect_uri": d.q.redirect_uri}).to_string();
    sqlx::query("INSERT INTO tokens (hash, kind, account_id, client_id, extra, expires_at) VALUES ($1,'oauth_code',$2,$3,$4,$5)")
        .bind(hash(&code)).bind(&acct.id).bind(&d.q.client_id).bind(extra).bind(now() + Duration::minutes(10)).execute(&app.db).await?;
    audit(&app, &acct.id, "", &acct.label(), "client.authorized", &d.q.client_id, &name).await;
    target.query_pairs_mut().append_pair("code", &code).append_pair("state", &d.q.state);
    Ok(Redirect::to(target.as_str()).into_response())
}

#[derive(Deserialize)]
pub struct TokenReq {
    grant_type: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    code_verifier: String,
    #[serde(default)]
    redirect_uri: String,
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    refresh_token: String,
}

fn token_err(e: &str) -> Response {
    (axum::http::StatusCode::BAD_REQUEST, [(header::CACHE_CONTROL, "no-store")], Json(json!({"error": e}))).into_response()
}

async fn issue(app: &AppInner, account: &str, client: &str) -> Result<Response, Fail> {
    let (a, r) = (token(), token());
    sqlx::query("INSERT INTO tokens (hash, kind, account_id, client_id, expires_at) VALUES ($1,'oauth_access',$2,$3,$4), ($5,'oauth_refresh',$2,$3,$6)")
        .bind(hash(&a)).bind(account).bind(client).bind(now() + Duration::hours(1))
        .bind(hash(&r)).bind(now() + Duration::days(60)).execute(&app.db).await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({
        "access_token": a, "token_type": "Bearer", "expires_in": 3600, "refresh_token": r, "scope": "substrate",
    }))).into_response())
}

pub async fn token_endpoint(State(app): State<App>, Form(t): Form<TokenReq>) -> Result<Response, Fail> {
    match t.grant_type.as_str() {
        "authorization_code" => {
            let row: Option<(String, String, String)> = sqlx::query_as(
                "UPDATE tokens SET used=true WHERE hash=$1 AND kind='oauth_code' AND NOT used AND expires_at > now() RETURNING account_id, client_id, extra",
            ).bind(hash(&t.code)).fetch_optional(&app.db).await?;
            let Some((account, client, extra)) = row else { return Ok(token_err("invalid_grant")) };
            let x: Value = serde_json::from_str(&extra).unwrap_or_default();
            let challenge = x["challenge"].as_str().unwrap_or("");
            let got = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(t.code_verifier.as_bytes()));
            if client != t.client_id || x["redirect_uri"].as_str() != Some(t.redirect_uri.as_str()) || !constant_eq(&got, challenge) {
                return Ok(token_err("invalid_grant"));
            }
            issue(&app, &account, &client).await
        }
        "refresh_token" => {
            let row: Option<(String, String)> = sqlx::query_as(
                "UPDATE tokens SET used=true WHERE hash=$1 AND kind='oauth_refresh' AND NOT used AND expires_at > now() RETURNING account_id, client_id",
            ).bind(hash(&t.refresh_token)).fetch_optional(&app.db).await?;
            let Some((account, client)) = row else { return Ok(token_err("invalid_grant")) };
            if !t.client_id.is_empty() && t.client_id != client {
                return Ok(token_err("invalid_grant"));
            }
            issue(&app, &account, &client).await
        }
        _ => Ok(token_err("unsupported_grant_type")),
    }
}

/// A personal token for clients that cannot do OAuth. Shown once.
pub async fn create_pat(app: &AppInner, account: &str, label: &str) -> Result<String, Fail> {
    let t = format!("sk_sub_{}", token());
    sqlx::query("INSERT INTO tokens (hash, kind, account_id, ref, expires_at) VALUES ($1,'pat',$2,$3,$4)")
        .bind(hash(&t)).bind(account).bind(substrate_proto::sanitize(label, 64)).bind(now() + Duration::days(90)).execute(&app.db).await?;
    audit(app, account, "", account, "pat.created", "", label).await;
    Ok(t)
}
