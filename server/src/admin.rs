//! Pushleft's Admin page for Substrate (App admin protocol 1, Pushleft's
//! docs/app-admin-spec.md). Pushleft signs every request with ES256 for one
//! method, path and body; the keys and the audience come in
//! PUSHLEFT_ADMIN_JWKS and PUSHLEFT_ADMIN_AUDIENCE. Without them the path is a 404.
//!
//! One resource: `users`, with usage over 30 days, and `suspend`/`unsuspend`
//! (shown as Block and Unblock).

use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::app::{audit, App, AppInner};

const PREFIX: &str = "/_pushleft/admin";

fn err(code: u16, c: &str, message: &str) -> Response {
    (StatusCode::from_u16(code).unwrap(), axum::Json(json!({"error": {"code": c, "message": message}}))).into_response()
}

pub async fn handle(State(app): State<App>, method: Method, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Bytes) -> Response {
    let (Ok(jwks), Ok(aud)) = (std::env::var("PUSHLEFT_ADMIN_JWKS"), std::env::var("PUSHLEFT_ADMIN_AUDIENCE")) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // the raw request target, as signed
    let target = uri.path_and_query().map(|p| p.as_str()).unwrap_or("");
    let bearer = headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let Some(sub) = verify(bearer, &jwks, &aud, method.as_str(), target, &body, Utc::now().timestamp()) else {
        return err(401, "unauthorized", "The request is not signed by Pushleft.");
    };
    let path = uri.path().strip_prefix(PREFIX).unwrap_or("");
    let q: Vec<(String, String)> = url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes()).into_owned().collect();
    let input: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    let r = match (method.as_str(), path.trim_end_matches('/')) {
        ("GET", "/schema") => Ok(json!(schema())),
        ("GET", "/overview") => overview(&app).await,
        ("GET", "/users") => list(&app, &q).await,
        ("GET", p) if p.starts_with("/users/") && !p[7..].contains('/') => show(&app, &p[7..]).await,
        ("POST", p) if p.starts_with("/users/") => match p[7..].split_once("/actions/") {
            Some((id, action)) => act(&app, &sub, id, action, &input["input"]).await,
            None => Err(err(404, "not_found", "No such route.")),
        },
        _ => Err(err(404, "not_found", "No such route.")),
    };
    match r {
        Ok(v) => axum::Json(v).into_response(),
        Err(e) => e,
    }
}

/// The six checks of the spec. Returns the signed-in person's email.
pub fn verify(token: &str, jwks: &str, aud: &str, method: &str, target: &str, body: &[u8], now: i64) -> Option<String> {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let mut parts = token.split('.');
    let (h, c, s) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let header: Value = serde_json::from_slice(&b64.decode(h).ok()?).ok()?;
    if header["alg"] != "ES256" {
        return None;
    }
    let keys: Value = serde_json::from_str(jwks).ok()?;
    let key = keys["keys"].as_array()?.iter().find(|k| k["kid"] == header["kid"] && k["kty"] == "EC" && k["crv"] == "P-256")?;
    let mut point = vec![4u8];
    point.extend(b64.decode(key["x"].as_str()?).ok()?);
    point.extend(b64.decode(key["y"].as_str()?).ok()?);
    ring::signature::UnparsedPublicKey::new(&ring::signature::ECDSA_P256_SHA256_FIXED, &point)
        .verify(format!("{h}.{c}").as_bytes(), &b64.decode(s).ok()?).ok()?;
    let cl: Value = serde_json::from_slice(&b64.decode(c).ok()?).ok()?;
    let ok = cl["iss"] == "https://api.pushleft.dev"
        && matches!(cl["role"].as_str(), Some("owner" | "admin"))
        && cl["aud"] == aud
        && cl["exp"].as_i64()? + 30 >= now
        && cl["method"] == method
        && cl["path"] == target
        && cl["body"] == hex::encode(Sha256::digest(body));
    ok.then(|| cl["sub"].as_str().map(String::from)).flatten()
}

fn schema() -> Value {
    json!({
        "protocol": 1,
        "title": "Substrate",
        "overview": true,
        "resources": [{
            "name": "users", "label": "Users", "singular": "User", "kind": "user",
            "can": ["list", "show"],
            "search": true,
            "views": [
                {"name": "active_7d", "label": "Active this week"},
                {"name": "blocked", "label": "Blocked"}
            ],
            "fields": [
                {"name": "email", "type": "email", "is": "title", "required": true, "sort": true},
                {"name": "name", "type": "string", "is": "subtitle"},
                {"name": "status", "type": "enum", "is": "status", "list": true, "filter": true,
                 "options": [{"value": "active", "label": "Active", "tone": "good"}, {"value": "blocked", "label": "Blocked", "tone": "bad"}]},
                {"name": "last_seen", "type": "datetime", "label": "Last seen", "is": "last_seen", "list": true, "sort": true},
                {"name": "created_at", "type": "datetime", "label": "Signed up", "list": true, "sort": true},
                {"name": "machines", "type": "integer", "label": "Macs", "list": true, "sort": true, "group": "Usage",
                 "help": "Macs with the relay signed in"},
                {"name": "projects", "type": "integer", "list": true, "sort": true, "group": "Usage"},
                {"name": "testers", "type": "integer", "sort": true, "group": "Usage", "help": "People on the user's projects"},
                {"name": "messages_30d", "type": "integer", "label": "Messages, 30 days", "list": true, "sort": true, "group": "Usage"},
                {"name": "turns_30d", "type": "integer", "label": "Agent turns, 30 days", "list": true, "sort": true, "group": "Usage"},
                {"name": "agent_cost_30d", "type": "money", "label": "Agent spend, 30 days", "sort": true, "group": "Usage",
                 "help": "From the work log, at list prices"},
                {"name": "gliph_nick", "type": "string", "label": "Gliph name", "group": "Profile"},
                {"name": "blocked_at", "type": "datetime", "label": "Blocked", "group": "Block"},
                {"name": "blocked_reason", "type": "text", "label": "Reason", "group": "Block"}
            ],
            "actions": [
                {"name": "suspend", "label": "Block", "on": "item", "danger": true,
                 "confirm": "This signs the user out on every Mac, phone and browser, and stops their share links.",
                 "input": [{"name": "reason", "type": "text", "required": true}]},
                {"name": "unsuspend", "label": "Unblock", "on": "item",
                 "confirm": "The user can sign in again."}
            ]
        }]
    })
}

const ROW: &str = "SELECT a.id, a.email, a.name, a.gliph_nick, a.created_at, a.blocked_at, a.blocked_reason,
    GREATEST(a.last_seen_at, (SELECT max(m.last_seen) FROM machines m WHERE m.account_id=a.id)) AS last_seen,
    (SELECT count(*) FROM machines m WHERE m.account_id=a.id AND NOT m.revoked) AS machines,
    (SELECT count(*) FROM projects p WHERE p.owner_id=a.id) AS projects,
    (SELECT count(DISTINCT mb.account_id) FROM members mb JOIN projects p ON p.id=mb.project_id WHERE p.owner_id=a.id AND mb.account_id<>a.id
       AND NOT EXISTS (SELECT 1 FROM access_links l WHERE l.account_id=mb.account_id)) AS testers,
    (SELECT count(*) FROM messages g JOIN projects p ON p.id=g.project_id WHERE p.owner_id=a.id AND g.created_at > now() - interval '30 days') AS messages_30d,
    (SELECT count(*) FROM log_turns t WHERE t.owner_id=a.id AND t.started_at > now() - interval '30 days') AS turns_30d,
    (SELECT coalesce(sum(t.cost), 0) FROM log_turns t WHERE t.owner_id=a.id AND t.started_at > now() - interval '30 days') AS agent_cost_30d
    FROM accounts a WHERE NOT EXISTS (SELECT 1 FROM access_links l WHERE l.account_id=a.id)";

type Row = (String, String, String, String, DateTime<Utc>, Option<DateTime<Utc>>, String, Option<DateTime<Utc>>, i64, i64, i64, i64, i64, f64);

fn item(r: Row) -> Value {
    let (id, email, name, nick, created, blocked_at, reason, last_seen, machines, projects, testers, messages, turns, cost) = r;
    let blocked = blocked_at.is_some();
    json!({
        "id": id,
        "values": {
            "email": email, "name": name, "gliph_nick": nick,
            "status": if blocked { "blocked" } else { "active" },
            "last_seen": last_seen.map(|t| t.to_rfc3339()), "created_at": created.to_rfc3339(),
            "machines": machines, "projects": projects, "testers": testers, "messages_30d": messages, "turns_30d": turns,
            "agent_cost_30d": {"amount": format!("{cost:.2}"), "currency": "USD"},
            "blocked_at": blocked_at.map(|t| t.to_rfc3339()), "blocked_reason": reason
        },
        "actions": [if blocked { "unsuspend" } else { "suspend" }]
    })
}

fn db(e: sqlx::Error) -> Response {
    tracing::error!(error = %e, "admin");
    err(500, "internal", "The database refused the query.")
}

async fn list(app: &AppInner, q: &[(String, String)]) -> Result<Value, Response> {
    let get = |k: &str| q.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let limit = get("limit").and_then(|v| v.parse::<i64>().ok()).unwrap_or(50).clamp(1, 200);
    // ponytail: offset cursor; keyset paging when the users list grows past a few thousand
    let offset = get("after").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0).max(0);
    let search = get("q").map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| format!("%{}%", s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")));
    let status: Vec<&str> = get("filter[status]").map(|v| v.split(',').collect()).unwrap_or_default();
    let view = get("view").unwrap_or("");
    let (col, desc) = match get("sort").unwrap_or("-last_seen") {
        s if s.starts_with('-') => (&s[1..], true),
        s => (s, false),
    };
    let col = match col {
        "email" | "created_at" | "last_seen" | "machines" | "projects" | "testers" | "messages_30d" | "turns_30d" | "agent_cost_30d" => col,
        _ => return Err(err(400, "bad_sort", "That column cannot be sorted.")),
    };
    let filter = "WHERE ($1::text IS NULL OR u.email ILIKE $1 OR u.name ILIKE $1 OR u.gliph_nick ILIKE $1)
        AND (cardinality($2::text[]) = 0 OR (CASE WHEN u.blocked_at IS NULL THEN 'active' ELSE 'blocked' END) = ANY($2))
        AND ($3 <> 'blocked' OR u.blocked_at IS NOT NULL)
        AND ($3 <> 'active_7d' OR u.last_seen > now() - interval '7 days')";
    let sql = format!(
        "SELECT * FROM ({ROW}) u {filter} ORDER BY u.{col} {} NULLS LAST, u.id LIMIT $4 OFFSET $5",
        if desc { "DESC" } else { "ASC" }
    );
    let rows: Vec<Row> = sqlx::query_as(&sql).bind(&search).bind(&status).bind(view).bind(limit).bind(offset)
        .fetch_all(&app.db).await.map_err(db)?;
    let total: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM ({ROW}) u {filter}"))
        .bind(&search).bind(&status).bind(view).fetch_one(&app.db).await.map_err(db)?;
    let n = rows.len() as i64;
    Ok(json!({
        "items": rows.into_iter().map(item).collect::<Vec<_>>(),
        "next": (offset + n < total && n == limit).then(|| (offset + n).to_string()),
        "total": total
    }))
}

async fn one(app: &AppInner, id: &str) -> Result<Value, Response> {
    let r: Option<Row> = sqlx::query_as(&format!("{ROW} AND a.id=$1")).bind(id).fetch_optional(&app.db).await.map_err(db)?;
    r.map(item).ok_or_else(|| err(404, "not_found", "No such user."))
}

async fn show(app: &AppInner, id: &str) -> Result<Value, Response> {
    one(app, id).await
}

async fn overview(app: &AppInner) -> Result<Value, Response> {
    let (users, active, blocked, turns, cost): (i64, i64, i64, i64, f64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM accounts a WHERE NOT EXISTS (SELECT 1 FROM access_links l WHERE l.account_id=a.id)),
                (SELECT count(*) FROM accounts a WHERE GREATEST(a.last_seen_at, (SELECT max(m.last_seen) FROM machines m WHERE m.account_id=a.id)) > now() - interval '7 days'),
                (SELECT count(*) FROM accounts WHERE blocked_at IS NOT NULL),
                (SELECT count(*) FROM log_turns WHERE started_at > now() - interval '30 days'),
                (SELECT coalesce(sum(cost), 0) FROM log_turns WHERE started_at > now() - interval '30 days')",
    ).fetch_one(&app.db).await.map_err(db)?;
    Ok(json!({"tiles": [
        {"label": "Users", "value": users, "resource": "users"},
        {"label": "Active this week", "value": active, "resource": "users", "view": "active_7d"},
        {"label": "Blocked", "value": blocked, "resource": "users", "view": "blocked"},
        {"label": "Agent turns, 30 days", "value": turns},
        {"label": "Agent spend, 30 days", "value": format!("{cost:.2}"), "unit": "USD"}
    ]}))
}

async fn act(app: &AppInner, by: &str, id: &str, action: &str, input: &Value) -> Result<Value, Response> {
    one(app, id).await?;
    let actor = format!("pushleft:{by}");
    match action {
        "suspend" => {
            let reason = input["reason"].as_str().map(str::trim).unwrap_or("");
            if reason.is_empty() {
                return Err((StatusCode::UNPROCESSABLE_ENTITY, axum::Json(json!({"error": {"code": "invalid", "message": "Give a reason.", "fields": {"reason": "Give a reason."}}}))).into_response());
            }
            block(app, id, reason).await.map_err(|e| e.into_response())?;
            audit(app, id, "", &actor, "account.blocked", id, reason).await;
            Ok(json!({"item": one(app, id).await?, "message": "Blocked. The user is signed out everywhere."}))
        }
        "unsuspend" => {
            // anything issued in a race with the block stays dead: the user signs in afresh
            sweep(app, id).await.map_err(|e| e.into_response())?;
            sqlx::query("UPDATE accounts SET blocked_at=NULL, blocked_reason='' WHERE id=$1").bind(id).execute(&app.db).await.map_err(db)?;
            audit(app, id, "", &actor, "account.unblocked", id, "").await;
            Ok(json!({"item": one(app, id).await?, "message": "Unblocked. The user can sign in again."}))
        }
        _ => Err(err(404, "not_found", "No such action.")),
    }
}

/// Block, then end every way in: pairings not yet redeemed, each Mac's relay,
/// browser and app sessions, personal and MCP tokens, and share links. A new
/// sign-in is refused in `auth::upsert_account`. Every credential check also
/// refuses a blocked account, so a token that a failed or racing cleanup leaves
/// behind does not work.
pub async fn block(app: &AppInner, id: &str, reason: &str) -> Result<(), crate::app::Fail> {
    sqlx::query("UPDATE accounts SET blocked_at=coalesce(blocked_at, now()), blocked_reason=$2 WHERE id=$1").bind(id).bind(reason).execute(&app.db).await?;
    sweep(app, id).await
}

/// Every credential and device of the account goes. Run by Block, and again by
/// Unblock, so a token minted while the block was being written never revives.
async fn sweep(app: &AppInner, id: &str) -> Result<(), crate::app::Fail> {
    sqlx::query("DELETE FROM pairings WHERE account_id=$1").bind(id).execute(&app.db).await?;
    let links: Vec<String> = sqlx::query_scalar("SELECT id FROM access_links WHERE owner_id=$1 AND NOT revoked").bind(id).fetch_all(&app.db).await?;
    for l in links {
        crate::links::revoke(app, id, &l).await?;
    }
    let machines: Vec<String> = sqlx::query_scalar("SELECT id FROM machines WHERE account_id=$1 AND NOT revoked").bind(id).fetch_all(&app.db).await?;
    for m in machines {
        crate::relay::revoke_machine(app, id, &m, "account blocked").await?;
    }
    sqlx::query("DELETE FROM tokens WHERE account_id=$1").bind(id).execute(&app.db).await?;
    Ok(())
}
