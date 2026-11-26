//! The app API (`/api/v1`): what Sidra's Substrate tab uses. A developer signs in
//! with their email and a 6-digit code (no link to open on an iPad), gets a bearer
//! token, and reads requests, sessions and machines, approves and rejects, and
//! listens for changes, and changes a session's projects and permissions. Creating a
//! session stays on the web.

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Duration;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app::{audit, hash, now, token, App, AppInner, Fail};
use crate::domain::{self, Event, Message, Project};

const CODE_MINUTES: i64 = 10;
const TOKEN_DAYS: i64 = 180;

#[derive(Deserialize)]
pub struct CodeReq {
    email: String,
}

/// POST /api/v1/auth/code {email}: mail a 6-digit code. Always answers the same way,
/// so it does not tell anyone which emails have accounts.
pub async fn auth_code(State(app): State<App>, headers: HeaderMap, Json(r): Json<CodeReq>) -> Result<Json<Value>, Fail> {
    let email = r.email.trim().to_lowercase();
    if !crate::auth::valid_email(&email) {
        return Err(Fail::new(400, "That doesn't look like an email address."));
    }
    let ip = crate::client_ip(&headers);
    if !app.limiter.allow(&format!("code-ip:{ip}"), 0.1, 10.0) || !app.limiter.allow(&format!("code:{email}"), 1.0 / 60.0, 3.0) {
        return Err(Fail::new(429, "Too many codes. Wait a few minutes."));
    }
    let code = format!("{:06}", rand::random::<u32>() % 1_000_000);
    sqlx::query("INSERT INTO tokens (hash, kind, ref, expires_at) VALUES ($1,'email_code',$2,$3)")
        .bind(hash(&format!("{email}:{code}"))).bind(&email).bind(now() + Duration::minutes(CODE_MINUTES)).execute(&app.db).await?;
    let msg = crate::mail::Message {
        subject: "Your Substrate code",
        title: "Your sign-in code",
        intro: "Type this code where you asked for it. It works once and expires in 10 minutes.",
        button: "", url: "",
        note: "Didn't ask for it? Ignore this email.",
        code: &code,
    };
    if let Err(e) = app.mail.send(&email, &msg).await {
        tracing::error!(error = %e, "code mail");
        return Err(Fail::new(502, "We couldn't send the email. Try again in a minute."));
    }
    if app.cfg.dev {
        tracing::warn!(%code, %email, "dev sign-in code");
    }
    Ok(Json(json!({"ok": true, "expires_in": CODE_MINUTES * 60})))
}

#[derive(Deserialize)]
pub struct VerifyReq {
    email: String,
    code: String,
    #[serde(default)]
    device: String,
}

/// POST /api/v1/auth/verify {email, code, device}: the code for a bearer token.
pub async fn auth_verify(State(app): State<App>, Json(r): Json<VerifyReq>) -> Result<Json<Value>, Fail> {
    let email = r.email.trim().to_lowercase();
    // Five tries per email per ten minutes: a 6-digit code cannot be guessed.
    if !app.limiter.allow(&format!("verify:{email}"), 1.0 / 120.0, 5.0) {
        return Err(Fail::new(429, "Too many tries. Ask for a new code in a few minutes."));
    }
    let code: String = r.code.chars().filter(|c| c.is_ascii_digit()).collect();
    let n = sqlx::query("UPDATE tokens SET used=true WHERE hash=$1 AND kind='email_code' AND NOT used AND expires_at > now()")
        .bind(hash(&format!("{email}:{code}"))).execute(&app.db).await?.rows_affected();
    if n != 1 {
        return Err(Fail::new(401, "That code is wrong or has expired."));
    }
    let aid = crate::auth::upsert_account(&app, &email).await?;
    let t = token();
    let device = substrate_proto::sanitize(r.device.trim(), 64);
    sqlx::query("INSERT INTO tokens (hash, kind, account_id, extra, expires_at) VALUES ($1,'app',$2,$3,$4)")
        .bind(hash(&t)).bind(&aid).bind(&device).bind(now() + Duration::days(TOKEN_DAYS)).execute(&app.db).await?;
    audit(&app, &aid, "", &email, "app.signed_in", &device, "").await;
    Ok(Json(json!({"token": t, "expires_in": TOKEN_DAYS * 86400, "account": me_json(&app, &aid).await?})))
}

/// The account behind an app token.
pub(crate) async fn bearer(app: &AppInner, headers: &HeaderMap) -> Result<String, Fail> {
    let t = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").trim().to_string();
    let a = sqlx::query_scalar::<_, String>("SELECT t.account_id FROM tokens t JOIN accounts a ON a.id=t.account_id WHERE t.hash=$1 AND t.kind='app' AND t.expires_at > now() AND a.blocked_at IS NULL")
        .bind(hash(&t)).fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(401, "Sign in again."))?;
    crate::auth::seen(app, &a).await;
    Ok(a)
}

/// POST /api/v1/auth/logout: this token stops working.
pub async fn auth_logout(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let t = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").trim().to_string();
    sqlx::query("DELETE FROM tokens WHERE hash=$1 AND kind='app'").bind(hash(&t)).execute(&app.db).await?;
    Ok(Json(json!({"ok": true})))
}

async fn me_json(app: &AppInner, aid: &str) -> Result<Value, Fail> {
    let (email, name): (String, String) = sqlx::query_as("SELECT email, name FROM accounts WHERE id=$1").bind(aid).fetch_one(&app.db).await?;
    Ok(json!({"id": aid, "email": email, "name": domain::account_label(app, aid).await, "has_name": !name.is_empty(),
        "avatar_url": format!("{}/accounts/{aid}/avatar", app.cfg.public_url)}))
}

pub async fn me(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    Ok(Json(me_json(&app, &aid).await?))
}

pub(crate) fn project_json(app: &AppInner, p: &Project) -> Value {
    json!({"id": p.id, "name": p.name, "handle": p.handle, "agent": crate::relay::agent_label(&p.agent), "state": p.state,
        "icon_url": if p.icon_etag.is_empty() { Value::Null } else { json!(format!("{}/projects/{}/icon?v={}", app.cfg.public_url, p.id, p.icon_etag)) }})
}

fn message_json(app: &AppInner, m: &Message, projects: &[Project], with_prompt: bool) -> Value {
    let p = projects.iter().find(|p| p.id == m.project_id);
    let mut v = json!({
        "id": m.id, "conversation_id": m.conversation_id, "kind": m.kind, "text": m.text, "stage": m.stage,
        // The agent stopped without a final reply: the request is not in progress any more.
        "agent_stopped": m.agent_idle && matches!(m.stage.as_str(), "submitted" | "working"),
        "deploy_stage": if m.deploy_stage.is_empty() { Value::Null } else { json!(m.deploy_stage) },
        "deploy_allow": m.deploy_allow, "risk": m.risk, "client": m.client_name,
        "hold_why": if m.hold_why.is_empty() || m.stage != "held" { Value::Null } else { json!(m.hold_why) },
        "proposal": if m.proposal.is_empty() { Value::Null } else { json!({"text": m.proposal, "why": m.proposal_why}) },
        "sender": {"id": m.sender_id, "name": m.sender_label, "avatar_url": format!("{}/accounts/{}/avatar", app.cfg.public_url, m.sender_id)},
        "project": p.map(|p| project_json(app, p)),
        "created_at": m.created_at, "updated_at": m.updated_at,
        "web_url": format!("{}/conversations/{}", app.cfg.public_url, m.conversation_id),
    });
    if with_prompt {
        v["prompt"] = json!(m.envelope.replace(substrate_proto::ATTACH_DIR, "<attachment folder>"));
    }
    v
}

/// With the agent, waiting on a tester's answer, or still on its way to production. An agent that
/// stopped without a final reply is not working on it any more: that message goes to history.
pub(crate) const ACTIVE: &str = "m.stage IN ('approved','dispatched','accepted','awaiting_sender')
    OR (m.stage IN ('submitted','working') AND NOT m.agent_idle)
    OR m.deploy_stage IN ('committing','running_checks','pushing','fix_committed','ci_running','ci_passed','deploying','deployed')";

pub(crate) const HISTORY: &str = "m.stage IN ('completed','rejected','cancelled','expired','failed','interrupted','uncertain','answered')
    OR (m.stage IN ('submitted','working') AND m.agent_idle)";

/// GET /api/v1/dashboard: what waits for you, what is with an agent, what finished.
pub async fn dashboard(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1 ORDER BY name").bind(&aid).fetch_all(&app.db).await?;
    let q = |w: &str, lim: i64| format!("SELECT m.* FROM messages m JOIN projects p ON p.id=m.project_id WHERE p.owner_id=$1 AND ({w}) ORDER BY m.updated_at DESC LIMIT {lim}");
    let waiting = sqlx::query_as::<_, Message>(&q("m.stage IN ('pending_approval','held','reviewing')", 100)).bind(&aid).fetch_all(&app.db).await?;
    let active = sqlx::query_as::<_, Message>(&q(ACTIVE, 30)).bind(&aid).fetch_all(&app.db).await?;
    let history = sqlx::query_as::<_, Message>(&q(&format!("({HISTORY}) AND NOT ({ACTIVE})"), 25))
        .bind(&aid).fetch_all(&app.db).await?;
    let ms: Vec<(String, Value)> = sqlx::query_as("SELECT id, panes FROM machines WHERE account_id=$1 AND NOT revoked").bind(&aid).fetch_all(&app.db).await?;
    let agents: Vec<&Value> = ms.iter().filter(|m| app.machine_online(&m.0)).flat_map(|m| m.1.as_array().into_iter().flatten()).filter(|p| !p["agent"].as_str().unwrap_or("").is_empty()).collect();
    Ok(Json(json!({
        "counts": {"waiting": waiting.len(), "active": active.len(), "agents": agents.len(), "working": agents.iter().filter(|p| p["status"] == "working").count()},
        "waiting": waiting.iter().map(|m| message_json(&app, m, &projects, true)).collect::<Vec<_>>(),
        "active": active.iter().map(|m| message_json(&app, m, &projects, false)).collect::<Vec<_>>(),
        "history": history.iter().map(|m| message_json(&app, m, &projects, false)).collect::<Vec<_>>(),
        "new_session_url": format!("{}/sessions/new", app.cfg.public_url),
    })))
}

/// POST /api/v1/messages/{id}/{approve|reject|review}. An approve may carry
/// `{"text": "...", "why": "..."}`: the developer's edit, which the agent gets instead,
/// and the line the tester gets on why. Review sends the request to the agent to go over first.
pub async fn decide(State(app): State<App>, headers: HeaderMap, Path((mid, action)): Path<(String, String)>, body: axum::body::Bytes) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    if !matches!(action.as_str(), "approve" | "reject" | "review") {
        return Err(Fail::new(404, "approve, reject or review"));
    }
    let b: Value = if body.is_empty() { Value::Null } else { serde_json::from_slice(&body).map_err(|_| Fail::new(400, "the body is not JSON"))? };
    let m = domain::decide(&app, &aid, &mid, &action, b["text"].as_str(), b["why"].as_str()).await?;
    Ok(Json(json!({"id": m.id, "stage": m.stage})))
}

fn events_json(app: &AppInner, owner: &str, events: &[Event], messages: &[Message]) -> Vec<Value> {
    events.iter().rev().map(|e| {
        let m = messages.iter().find(|m| m.id == e.message_id);
        let avatar = match e.actor_kind.as_str() {
            "human" => m.map(|m| format!("{}/accounts/{}/avatar", app.cfg.public_url, m.sender_id)),
            "developer" => Some(format!("{}/accounts/{owner}/avatar", app.cfg.public_url)),
            "system" if !e.data["by"].as_str().unwrap_or("").is_empty() => Some(format!("{}/accounts/{owner}/avatar", app.cfg.public_url)),
            _ => None,
        };
        json!({"seq": e.seq, "time": e.created_at, "actor": e.actor_kind, "name": e.actor_name, "kind": e.kind, "text": e.text,
            "stage": e.data["stage"], "held": e.held, "avatar_url": avatar})
    }).collect()
}

/// GET /api/v1/conversations/{id}: its messages and events, newest first.
pub async fn conversation(State(app): State<App>, headers: HeaderMap, Path(cid): Path<String>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    let pid: String = sqlx::query_scalar("SELECT project_id FROM conversations WHERE id=$1").bind(&cid).fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "no such conversation"))?;
    let p = domain::require_owner(&app, &pid, &aid).await?;
    let msgs = sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE conversation_id=$1 ORDER BY order_n").bind(&cid).fetch_all(&app.db).await?;
    let events = sqlx::query_as::<_, Event>("SELECT * FROM events WHERE conversation_id=$1 ORDER BY seq").bind(&cid).fetch_all(&app.db).await?;
    let projects = [p];
    Ok(Json(json!({"id": cid, "project": project_json(&app, &projects[0]),
        "deploy_stage": msgs.iter().rev().find(|m| !m.deploy_stage.is_empty()).map(|m| m.deploy_stage.clone()),
        "messages": msgs.iter().map(|m| message_json(&app, m, &projects, true)).collect::<Vec<_>>(),
        "events": events_json(&app, &aid, &events, &msgs)})))
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    id: String,
    account_id: String,
    label: String,
    can_read: bool,
    can_deploys: bool,
    can_files: bool,
    auto: bool,
    revoked: bool,
    expires_at: chrono::DateTime<chrono::Utc>,
    last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

async fn session_json(app: &AppInner, s: &SessionRow, projects: &[Project]) -> Result<Value, Fail> {
    let rows: Vec<(String, bool, bool, bool)> = sqlx::query_as("SELECT project_id, can_deploy_test, can_deploy_prod, trusted_auto FROM members WHERE account_id=$1").bind(&s.account_id).fetch_all(&app.db).await?;
    let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE sender_id=$1 AND stage IN ('pending_approval','held','reviewing')").bind(&s.account_id).fetch_one(&app.db).await?;
    Ok(json!({
        "id": s.id, "name": s.label, "avatar_url": format!("{}/accounts/{}/avatar", app.cfg.public_url, s.account_id),
        // Each project carries its own values: a session permission set on a project page can differ per project.
        "projects": rows.iter().filter_map(|r| projects.iter().find(|p| p.id == r.0).map(|p| (r, p))).map(|(r, p)| {
            let mut v = project_json(app, p);
            v["permissions"] = json!({"no_approval": r.3, "deploy_test": r.1, "deploy_production": r.2});
            v
        }).collect::<Vec<_>>(),
        "permissions": {"replies": s.can_read, "deploys": s.can_deploys, "files": s.can_files, "no_approval": s.auto,
            "deploy_test": rows.iter().any(|r| r.1), "deploy_production": rows.iter().any(|r| r.2)},
        "waiting": waiting, "last_used_at": s.last_used_at, "expires_at": s.expires_at,
        "active": !s.revoked && s.expires_at > now(),
        "web_url": format!("{}/sessions/{}", app.cfg.public_url, s.id),
    }))
}

const SESSION_COLS: &str = "id, account_id, label, can_read, can_deploys, can_files, auto, revoked, expires_at, last_used_at";

/// GET /api/v1/sessions
pub async fn sessions(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1").bind(&aid).fetch_all(&app.db).await?;
    let rows = sqlx::query_as::<_, SessionRow>(&format!("SELECT {SESSION_COLS} FROM access_links WHERE owner_id=$1 AND NOT revoked AND expires_at > now() ORDER BY created_at DESC"))
        .bind(&aid).fetch_all(&app.db).await?;
    let mut out = vec![];
    for s in &rows {
        out.push(session_json(&app, s, &projects).await?);
    }
    Ok(Json(json!({"sessions": out, "new_session_url": format!("{}/sessions/new", app.cfg.public_url)})))
}

/// GET /api/v1/sessions/{id}: the session, its counts and its last 20 conversations.
pub async fn session(State(app): State<App>, headers: HeaderMap, Path(sid): Path<String>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1").bind(&aid).fetch_all(&app.db).await?;
    let s = sqlx::query_as::<_, SessionRow>(&format!("SELECT {SESSION_COLS} FROM access_links WHERE id=$1 AND owner_id=$2"))
        .bind(&sid).bind(&aid).fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "no such session"))?;
    let mut v = session_json(&app, &s, &projects).await?;
    let (sent, active, done, shipped): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE kind <> 'answer'), count(*) FILTER (WHERE stage IN ('approved','dispatched','accepted','awaiting_sender') OR (stage IN ('submitted','working') AND NOT agent_idle)),
                count(*) FILTER (WHERE stage = 'completed'), count(*) FILTER (WHERE deploy_stage IN ('deployed','verified','ready_to_retest'))
         FROM messages WHERE sender_id=$1",
    ).bind(&s.account_id).fetch_one(&app.db).await?;
    v["counts"] = json!({"messages": sent, "waiting": v["waiting"], "active": active, "completed": done, "deployed": shipped});
    let convs: Vec<(String, String, String)> = sqlx::query_as("SELECT id, project_id, title FROM conversations WHERE sender_id=$1 ORDER BY updated_at DESC LIMIT 20")
        .bind(&s.account_id).fetch_all(&app.db).await?;
    let mut history = vec![];
    for (cid, pid, title) in convs {
        let Some(p) = projects.iter().find(|p| p.id == pid) else { continue };
        let msgs = sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE conversation_id=$1 ORDER BY order_n").bind(&cid).fetch_all(&app.db).await?;
        let events = sqlx::query_as::<_, Event>("SELECT * FROM events WHERE conversation_id=$1 ORDER BY seq").bind(&cid).fetch_all(&app.db).await?;
        history.push(json!({"id": cid, "title": title, "project": project_json(&app, p),
            "deploy_stage": msgs.iter().rev().find(|m| !m.deploy_stage.is_empty()).map(|m| m.deploy_stage.clone()),
            "events": events_json(&app, &aid, &events, &msgs)}));
    }
    v["history"] = json!(history);
    v["available_projects"] = json!(projects.iter().map(|p| project_json(&app, p)).collect::<Vec<_>>());
    Ok(Json(v))
}

/// PATCH /api/v1/sessions/{id} `{"permissions": {name: bool}, "projects": [project id]}`:
/// each permission given is set in every project of the session; one left out keeps each
/// project's value. `projects`, when given, is the full list the session reaches.
pub async fn session_update(State(app): State<App>, headers: HeaderMap, Path(sid): Path<String>, Json(b): Json<Value>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    let mut changed = vec![];
    if let Some(perms) = b.get("permissions").and_then(Value::as_object) {
        for (name, v) in perms {
            let key = match name.as_str() { "replies" => "read", "deploys" => "deploys", "files" => "files", "no_approval" => "auto",
                "deploy_test" => "deploy_test", "deploy_production" => "deploy_prod", _ => return Err(Fail::new(400, "unknown permission")) };
            changed.push((key, v.as_bool().ok_or_else(|| Fail::new(400, "a permission is true or false"))?));
        }
    }
    let projects: Option<Vec<String>> = match b.get("projects") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value(v.clone()).map_err(|_| Fail::new(400, "projects is a list of project ids"))?),
    };
    crate::links::update(&app, &aid, &sid, &changed, projects.as_deref()).await?;
    session(State(app), headers, Path(sid)).await
}

/// GET /api/v1/machines: each computer and its agents (read-only).
pub async fn machines(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1").bind(&aid).fetch_all(&app.db).await?;
    let ms: Vec<(String, String, String, String, Option<chrono::DateTime<chrono::Utc>>, Value)> = sqlx::query_as(
        "SELECT id, name, platform, version, last_seen, panes FROM machines WHERE account_id=$1 AND NOT revoked ORDER BY created_at",
    ).bind(&aid).fetch_all(&app.db).await?;
    Ok(Json(json!({"machines": ms.iter().map(|(id, name, platform, version, seen, panes)| json!({
        "id": id, "name": name, "platform": platform, "relay_version": version, "online": app.machine_online(id), "last_seen": seen,
        "agents": panes.as_array().into_iter().flatten().filter(|p| !p["agent"].as_str().unwrap_or("").is_empty()).map(|pn| {
            let pid = pn["pane_id"].as_str().unwrap_or("");
            json!({"pane_id": pid, "workspace": pn["workspace"], "agent": pn["agent"], "status": pn["status"], "title": pn["title"],
                "project": projects.iter().find(|p| &p.machine_id == id && p.pane_id == pid).map(|p| project_json(&app, p))})
        }).collect::<Vec<_>>(),
    })).collect::<Vec<_>>()})))
}

/// POST /api/v1/devices {token, bundle, sandbox, client_ref}: Sidra's APNs token, for a push per
/// request that waits for approval. `client_ref` comes back in each push (Sidra's id for this account).
pub async fn device_add(State(app): State<App>, headers: HeaderMap, Json(b): Json<Value>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    let token = b["token"].as_str().unwrap_or("").trim().to_lowercase();
    let bundle = b["bundle"].as_str().unwrap_or("").trim().to_string();
    if token.len() < 32 || token.len() > 200 || !token.chars().all(|c| c.is_ascii_hexdigit()) || bundle.is_empty() || bundle.len() > 200 {
        return Err(Fail::new(400, "a device token is hex, and a bundle id is needed"));
    }
    let client_ref: String = b["client_ref"].as_str().unwrap_or("").chars().take(64).collect();
    sqlx::query("INSERT INTO devices (token, account_id, bundle, sandbox, client_ref) VALUES ($1,$2,$3,$4,$5)
                 ON CONFLICT (token) DO UPDATE SET account_id=$2, bundle=$3, sandbox=$4, client_ref=$5")
        .bind(&token).bind(&aid).bind(&bundle).bind(b["sandbox"].as_bool().unwrap_or(b["sandbox"] == "true")).bind(&client_ref).execute(&app.db).await?;
    Ok(Json(json!({"ok": true})))
}

/// DELETE /api/v1/devices/{token}: no more pushes to this device (Sidra signs out).
pub async fn device_remove(State(app): State<App>, headers: HeaderMap, axum::extract::Path(token): axum::extract::Path<String>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    sqlx::query("DELETE FROM devices WHERE token=$1 AND account_id=$2").bind(token.to_lowercase()).bind(&aid).execute(&app.db).await?;
    Ok(Json(json!({"ok": true})))
}

/// GET /api/v1/live: server-sent "changed" when anything of this account changes
/// (a new request, an agent's state, a deploy step). Refetch on each.
pub async fn live(State(app): State<App>, headers: HeaderMap) -> Result<Response, Fail> {
    let aid = bearer(&app, &headers).await?;
    Ok(crate::web::account_live(&app, std::slice::from_ref(&aid)).into_response())
}
