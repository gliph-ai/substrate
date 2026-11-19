//! The relay side: pairing a machine (RFC 8628), rotating its tokens, the
//! WebSocket it dials, agent reports, and the developer API the CLI uses.

use axum::body::Bytes;
use axum::extract::ws::{Message as WsMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Duration;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use substrate_proto::{self as proto, AckStage, Binding, CloudFrame, DaemonFrame, Pane, Report};
use tokio::sync::mpsc;

use crate::app::{audit, hash, id, now, token, App, AppInner, Fail};
use crate::domain::{self, add_event, Message, Project};
use crate::safety;

const ACCESS_TTL_MIN: i64 = 60;
const REFRESH_TTL_DAYS: i64 = 90;

// ---- pairing ----

#[derive(Deserialize)]
pub struct PairStart {
    machine_name: String,
    #[serde(default)]
    platform: String,
    #[serde(default)]
    version: String,
}

fn user_code() -> String {
    // No vowels or look-alikes: nothing offensive, nothing misread.
    const A: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";
    let mut r = rand::thread_rng();
    let s: String = (0..8).map(|_| A[rand::Rng::gen_range(&mut r, 0..A.len())] as char).collect();
    format!("{}-{}", &s[..4], &s[4..])
}

pub async fn pair_start(State(app): State<App>, headers: HeaderMap, Json(p): Json<PairStart>) -> Result<Json<Value>, Fail> {
    if !app.limiter.allow(&format!("pair:{}", crate::client_ip(&headers)), 0.05, 5.0) {
        return Err(Fail::new(429, "too many pairing attempts"));
    }
    let device = token();
    let code = user_code();
    let name: String = proto::sanitize(&p.machine_name, 64);
    sqlx::query("INSERT INTO pairings (device_hash, user_code, machine_name, platform, version, expires_at) VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(hash(&device)).bind(&code).bind(if name.is_empty() { "computer".to_string() } else { name })
        .bind(proto::sanitize(&p.platform, 32)).bind(proto::sanitize(&p.version, 32)).bind(now() + Duration::minutes(10))
        .execute(&app.db).await?;
    Ok(Json(json!({
        "device_code": device, "user_code": code,
        "verification_uri": format!("{}/pair?code={code}", app.cfg.public_url),
        "interval": 3, "expires_in": 600,
    })))
}

#[derive(Deserialize)]
pub struct PairToken {
    device_code: String,
}

pub async fn pair_token(State(app): State<App>, Json(p): Json<PairToken>) -> Result<Response, Fail> {
    let row: Option<(String, String, String, String, String)> = sqlx::query_as(
        "SELECT status, account_id, machine_name, platform, version FROM pairings WHERE device_hash=$1 AND expires_at > now()",
    ).bind(hash(&p.device_code)).fetch_optional(&app.db).await?;
    let Some((status, account, name, platform, version)) = row else {
        return Ok((axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": "expired_token"}))).into_response());
    };
    match status.as_str() {
        "pending" => return Ok((axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": "authorization_pending"}))).into_response()),
        "denied" => return Ok((axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": "access_denied"}))).into_response()),
        "approved" => {}
        _ => return Ok((axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": "expired_token"}))).into_response()),
    }
    // Issue exactly once.
    let n = sqlx::query("UPDATE pairings SET status='issued' WHERE device_hash=$1 AND status='approved'")
        .bind(hash(&p.device_code)).execute(&app.db).await?.rows_affected();
    if n != 1 {
        return Ok((axum::http::StatusCode::BAD_REQUEST, Json(json!({"error": "expired_token"}))).into_response());
    }
    let mid = id("mc");
    sqlx::query("INSERT INTO machines (id, account_id, name, platform, version) VALUES ($1,$2,$3,$4,$5)")
        .bind(&mid).bind(&account).bind(&name).bind(&platform).bind(&version).execute(&app.db).await?;
    audit(&app, &account, "", &name, "machine.paired", &mid, &platform).await;
    let (access, refresh) = issue_machine_tokens(&app, &account, &mid).await?;
    Ok(Json(json!({"machine_id": mid, "access_token": access, "refresh_token": refresh, "expires_in": ACCESS_TTL_MIN * 60})).into_response())
}

async fn issue_machine_tokens(app: &AppInner, account: &str, machine: &str) -> Result<(String, String), Fail> {
    let (a, r) = (token(), token());
    sqlx::query("INSERT INTO tokens (hash, kind, account_id, machine_id, expires_at) VALUES ($1,'machine_access',$2,$3,$4), ($5,'machine_refresh',$2,$3,$6)")
        .bind(hash(&a)).bind(account).bind(machine).bind(now() + Duration::minutes(ACCESS_TTL_MIN))
        .bind(hash(&r)).bind(now() + Duration::days(REFRESH_TTL_DAYS))
        .execute(&app.db).await?;
    Ok((a, r))
}

#[derive(Deserialize)]
pub struct Refresh {
    refresh_token: String,
}

/// Rotate. A refresh token works once; presenting a spent one again means it
/// was copied, so the whole machine is revoked.
pub async fn refresh(State(app): State<App>, Json(r): Json<Refresh>) -> Result<Json<Value>, Fail> {
    let row: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT t.account_id, t.machine_id, t.used FROM tokens t JOIN machines m ON m.id=t.machine_id
         WHERE t.hash=$1 AND t.kind='machine_refresh' AND t.expires_at > now() AND NOT m.revoked",
    ).bind(hash(&r.refresh_token)).fetch_optional(&app.db).await?;
    let Some((account, machine, used)) = row else { return Err(Fail::new(401, "invalid_grant")) };
    if used {
        revoke_machine(&app, &account, &machine, "refresh token reused").await?;
        return Err(Fail::new(401, "invalid_grant"));
    }
    let n = sqlx::query("UPDATE tokens SET used=true WHERE hash=$1 AND NOT used").bind(hash(&r.refresh_token)).execute(&app.db).await?.rows_affected();
    if n != 1 {
        return Err(Fail::new(401, "invalid_grant"));
    }
    let (access, refresh) = issue_machine_tokens(&app, &account, &machine).await?;
    Ok(Json(json!({"machine_id": machine, "access_token": access, "refresh_token": refresh, "expires_in": ACCESS_TTL_MIN * 60})))
}

/// Deliveries the old binding still held (accepted, submitted, working)
/// become interrupted: the developer decides whether to send them again, and
/// the project's queue moves on.
pub async fn interrupt_in_flight(app: &AppInner, pid: &str, why: &str) -> Result<(), Fail> {
    let stuck = sqlx::query_as::<_, Message>(
        "UPDATE messages SET stage='interrupted', updated_at=now() WHERE project_id=$1 AND stage IN ('accepted','submitted','working') RETURNING *",
    ).bind(pid).fetch_all(&app.db).await?;
    for m in stuck {
        add_event(app, &m.conversation_id, &m.id, "system", "Substrate", "stage", why, json!({"message_id": m.id, "stage": "interrupted"}), false).await?;
    }
    Ok(())
}

pub async fn revoke_machine(app: &AppInner, account: &str, machine: &str, why: &str) -> Result<(), Fail> {
    let pids: Vec<String> = sqlx::query_scalar("SELECT id FROM projects WHERE machine_id=$1").bind(machine).fetch_all(&app.db).await?;
    for pid in &pids {
        interrupt_in_flight(app, pid, "The developer's computer was signed out before the agent finished. The developer can deliver it again.").await?;
    }
    sqlx::query("UPDATE machines SET revoked=true WHERE id=$1 AND account_id=$2").bind(machine).bind(account).execute(&app.db).await?;
    sqlx::query("DELETE FROM tokens WHERE machine_id=$1").bind(machine).execute(&app.db).await?;
    if let Some(tx) = app.hub.lock().unwrap().remove(machine) {
        let _ = tx.send(CloudFrame::Error { message: "this machine was signed out".into() });
    }
    domain::machine_presence(app, machine, false).await?;
    audit(app, account, "", account, "machine.revoked", machine, why).await;
    Ok(())
}

/// The machine behind a relay request's bearer: (account, machine).
pub async fn machine_auth(app: &AppInner, headers: &HeaderMap) -> Result<(String, String), Fail> {
    let t = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT t.account_id, t.machine_id FROM tokens t JOIN machines m ON m.id=t.machine_id
         WHERE t.hash=$1 AND t.kind='machine_access' AND t.expires_at > now() AND NOT m.revoked",
    ).bind(hash(t.trim())).fetch_optional(&app.db).await?;
    row.ok_or_else(|| Fail::new(401, "unauthorized"))
}

// ---- the socket ----

pub async fn ws(State(app): State<App>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Result<Response, Fail> {
    let (account, machine) = machine_auth(&app, &headers).await?;
    Ok(upgrade.max_message_size(1 << 20).on_upgrade(move |s| async move {
        if let Err(e) = session(app.clone(), s, account, machine.clone()).await {
            tracing::warn!(machine, error = %e.1, "relay session ended");
        }
    }))
}

async fn bindings(app: &AppInner, machine: &str) -> Result<Vec<Binding>, Fail> {
    let ps = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE machine_id=$1 AND pane_id<>''").bind(machine).fetch_all(&app.db).await?;
    Ok(ps.into_iter().map(|p| Binding { project_id: p.id, handle: p.handle, pane_id: p.pane_id, agent_session: p.agent_session, cwd: p.cwd }).collect())
}

pub async fn push_bindings(app: &AppInner, machine: &str) -> Result<(), Fail> {
    let b = bindings(app, machine).await?;
    app.send_machine(machine, CloudFrame::Bindings { projects: b });
    Ok(())
}

async fn session(app: App, socket: WebSocket, account: String, machine: String) -> Result<(), Fail> {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<CloudFrame>();
    // A newer connection from the same machine replaces the old one.
    if let Some(old) = app.hub.lock().unwrap().insert(machine.clone(), tx.clone()) {
        let _ = old.send(CloudFrame::Error { message: "replaced by a newer connection".into() });
    }
    let writer = tokio::spawn(async move {
        let mut ping = tokio::time::interval(std::time::Duration::from_secs(25));
        loop {
            tokio::select! {
                f = rx.recv() => match f {
                    Some(f) => {
                        let close = matches!(f, CloudFrame::Error { .. });
                        if sink.send(WsMsg::Text(serde_json::to_string(&f).unwrap().into())).await.is_err() || close { break; }
                    }
                    None => break,
                },
                _ = ping.tick() => if sink.send(WsMsg::Ping(Default::default())).await.is_err() { break; },
            }
        }
        let _ = sink.close().await;
    });

    let result = async {
        while let Some(msg) = stream.next().await {
            let msg = msg.map_err(|e| Fail::new(400, e.to_string()))?;
            let text = match msg {
                WsMsg::Text(t) => t.to_string(),
                WsMsg::Close(_) => break,
                _ => continue,
            };
            let frame: DaemonFrame = match serde_json::from_str(&text) {
                Ok(f) => f,
                Err(e) => { tracing::warn!(error = %e, "bad relay frame"); continue; }
            };
            handle_frame(&app, &account, &machine, &tx, frame).await?;
        }
        Ok::<(), Fail>(())
    }.await;

    {
        let mut hub = app.hub.lock().unwrap();
        if hub.get(&machine).map(|t| t.same_channel(&tx)).unwrap_or(false) {
            hub.remove(&machine);
        }
    }
    if !app.machine_online(&machine) {
        domain::machine_presence(&app, &machine, false).await?;
    }
    writer.abort();
    result
}

async fn handle_frame(app: &AppInner, account: &str, machine: &str, tx: &mpsc::UnboundedSender<CloudFrame>, f: DaemonFrame) -> Result<(), Fail> {
    match f {
        DaemonFrame::Hello { version, protocol, panes } => {
            if protocol != proto::PROTOCOL {
                let _ = tx.send(CloudFrame::Error { message: format!("relay protocol {protocol} is not supported; update the substrate CLI") });
                return Ok(());
            }
            sqlx::query("UPDATE machines SET version=$2, panes=$3, last_seen=now() WHERE id=$1")
                .bind(machine).bind(proto::sanitize(&version, 32)).bind(sqlx::types::Json(&panes)).execute(&app.db).await?;
            domain::machine_presence(app, machine, true).await?;
            reconcile_panes(app, machine, &panes).await?;
            let _ = tx.send(CloudFrame::Welcome { machine_id: machine.to_string(), projects: bindings(app, machine).await? });
            // Anything sent but never acknowledged goes again with the same
            // delivery id; the relay drops a repeat it already holds.
            let unacked = sqlx::query_as::<_, Message>(
                "SELECT m.* FROM messages m JOIN projects p ON p.id=m.project_id WHERE p.machine_id=$1 AND m.stage='dispatched'",
            ).bind(machine).fetch_all(&app.db).await?;
            for m in unacked {
                sqlx::query("UPDATE messages SET stage='approved', delivery_id='' WHERE id=$1").bind(&m.id).execute(&app.db).await?;
            }
            for pid in project_ids(app, machine).await? {
                domain::pump(app, &pid).await?;
            }
        }
        DaemonFrame::Panes { panes } => {
            sqlx::query("UPDATE machines SET panes=$2, last_seen=now() WHERE id=$1").bind(machine).bind(sqlx::types::Json(&panes)).execute(&app.db).await?;
            reconcile_panes(app, machine, &panes).await?;
        }
        DaemonFrame::Status { project_id, state } => {
            let allowed = ["idle", "working", "blocked", "done", "unknown", "offline", "needs_rebind"];
            if !allowed.contains(&state.as_str()) {
                return Ok(());
            }
            let n = sqlx::query("UPDATE projects SET state=$3 WHERE id=$1 AND machine_id=$2").bind(&project_id).bind(machine).bind(&state)
                .execute(&app.db).await?.rows_affected();
            if n == 1 && matches!(state.as_str(), "idle" | "done") {
                domain::pump(app, &project_id).await?;
            }
        }
        DaemonFrame::Ack { delivery_id, stage, detail } => {
            let m = sqlx::query_as::<_, Message>(
                "SELECT m.* FROM messages m JOIN projects p ON p.id=m.project_id WHERE m.delivery_id=$1 AND p.machine_id=$2",
            ).bind(&delivery_id).bind(machine).fetch_optional(&app.db).await?;
            let Some(m) = m else { return Ok(()) };
            let p = domain::project(app, &m.project_id).await?;
            let detail = proto::sanitize(&detail, 300);
            let (stage_s, text, free) = match stage {
                AckStage::Accepted => ("accepted", "The relay on the developer's computer has it.".to_string(), false),
                AckStage::Submitted => ("submitted", format!("Submitted to the agent in herdr pane {}.", p.pane_id), false),
                AckStage::Working => ("working", "The agent is working on it.".to_string(), false),
                AckStage::AgentIdle => ("working", "The agent stopped working without sending a reply.".to_string(), true),
                AckStage::Uncertain => ("uncertain", "Unconfirmed: the relay restarted while submitting. It was not retried; the developer decides.".to_string(), true),
                AckStage::Failed => ("failed", format!("Delivery failed: {}", if detail.is_empty() { "unknown error" } else { &detail }), true),
            };
            // Never move a message backwards, or past a terminal stage.
            let order = ["dispatched", "accepted", "submitted", "working"];
            let cur = order.iter().position(|s| *s == m.stage);
            let new = order.iter().position(|s| *s == stage_s);
            let forward = match (cur, new) {
                (Some(c), Some(n)) => n > c || (n == c && stage == AckStage::AgentIdle && !m.agent_idle),
                (Some(_), None) => true,
                _ => false,
            };
            if !forward {
                return Ok(());
            }
            sqlx::query("UPDATE messages SET stage=$2, agent_idle=$3, updated_at=now() WHERE id=$1")
                .bind(&m.id).bind(stage_s).bind(stage == AckStage::AgentIdle).execute(&app.db).await?;
            add_event(app, &m.conversation_id, &m.id, "relay", &p.handle, "stage", &text, json!({"message_id": m.id, "stage": stage_s}), false).await?;
            if free {
                audit(app, account, &p.id, &p.handle, "delivery.ack", &m.id, stage_s).await;
                domain::pump(app, &p.id).await?;
            }
        }
        DaemonFrame::Excerpt { message_id, text } => {
            sqlx::query("UPDATE messages m SET excerpt=$3 FROM projects p WHERE m.id=$1 AND p.id=m.project_id AND p.machine_id=$2")
                .bind(&message_id).bind(machine).bind(proto::sanitize(&text, 8192)).execute(&app.db).await?;
        }
    }
    Ok(())
}

async fn project_ids(app: &AppInner, machine: &str) -> Result<Vec<String>, Fail> {
    Ok(sqlx::query_scalar("SELECT id FROM projects WHERE machine_id=$1").bind(machine).fetch_all(&app.db).await?)
}

/// A bound pane that is gone, or now hosts a different agent session, needs a
/// rebind: never deliver to whatever took its place.
async fn reconcile_panes(app: &AppInner, machine: &str, panes: &[Pane]) -> Result<(), Fail> {
    let ps = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE machine_id=$1 AND pane_id<>''").bind(machine).fetch_all(&app.db).await?;
    for p in ps {
        let live = panes.iter().find(|x| x.pane_id == p.pane_id);
        let state = match live {
            Some(x) if x.agent_session == p.agent_session || p.agent_session.is_empty() => match x.status.as_str() {
                "idle" | "done" | "working" | "blocked" => x.status.clone(),
                _ => "unknown".into(),
            },
            _ => "needs_rebind".into(),
        };
        if state != p.state {
            sqlx::query("UPDATE projects SET state=$2 WHERE id=$1").bind(&p.id).bind(&state).execute(&app.db).await?;
            if state == "needs_rebind" {
                audit(app, &p.owner_id, &p.id, &p.handle, "project.needs_rebind", &p.pane_id, "pane gone or session changed").await;
            }
            if matches!(state.as_str(), "idle" | "done") {
                domain::pump(app, &p.id).await?;
            }
        }
    }
    Ok(())
}

// ---- reports from the agent ----

pub async fn report(State(app): State<App>, headers: HeaderMap, Json(r): Json<Report>) -> Result<Json<Value>, Fail> {
    let (account, machine) = machine_auth(&app, &headers).await?;
    let m = authorize_report(&app, &machine, &r.message_id, &r.reply_token).await?;
    let p = domain::project(&app, &m.project_id).await?;
    if !app.limiter.allow(&format!("report:{}", m.id), 1.0, 30.0) {
        return Err(Fail::new(429, "too many reports"));
    }
    let text = proto::sanitize(&r.text, 16000);
    let hits = safety::secret_hits(&text);
    let held = !hits.is_empty();
    let agent_name = agent_label(&p.agent);
    let atts: Vec<String> = if r.attachments.is_empty() { vec![] } else {
        sqlx::query_scalar("SELECT id FROM attachments WHERE id = ANY($1) AND message_id=$2 AND origin='agent'")
            .bind(&r.attachments).bind(&m.id).fetch_all(&app.db).await?
    };
    match r.kind.as_str() {
        "progress" | "reply" => {
            add_event(&app, &m.conversation_id, &m.id, "agent", &agent_name, &r.kind, &text, json!({"message_id": m.id, "attachments": atts, "secret_hits": hits}), held).await?;
        }
        "ask" => {
            sqlx::query("UPDATE messages SET stage='awaiting_sender', question=$2, agent_idle=false, updated_at=now() WHERE id=$1").bind(&m.id).bind(&text).execute(&app.db).await?;
            add_event(&app, &m.conversation_id, &m.id, "agent", &agent_name, "question", &text, json!({"message_id": m.id, "secret_hits": hits}), held).await?;
            domain::pump(&app, &p.id).await?;
        }
        "final" => {
            sqlx::query("UPDATE messages SET stage='completed', agent_idle=false, updated_at=now() WHERE id=$1").bind(&m.id).execute(&app.db).await?;
            add_event(&app, &m.conversation_id, &m.id, "agent", &agent_name, "final", &text, json!({"message_id": m.id, "attachments": atts, "secret_hits": hits}), held).await?;
            domain::pump(&app, &p.id).await?;
        }
        "deploy" => {
            let d = r.deploy.ok_or_else(|| Fail::new(400, "deploy report without deploy details"))?;
            if !proto::DEPLOY_STAGES.contains(&d.stage.as_str()) {
                return Err(Fail::new(400, "unknown deploy stage"));
            }
            let clean = json!({
                "stage": d.stage, "commit": proto::sanitize(&d.commit, 64), "ci_url": proto::sanitize(&d.ci_url, 500),
                "env": proto::sanitize(&d.env, 40), "verify_url": proto::sanitize(&d.verify_url, 500),
                "run_id": proto::sanitize(&d.run_id, 64), "note": proto::sanitize(&d.note, 500),
            });
            sqlx::query("UPDATE messages SET deploy_stage=$2, deploy = deploy || $3, updated_at=now() WHERE id=$1")
                .bind(&m.id).bind(&d.stage).bind(&clean).execute(&app.db).await?;
            let line = deploy_line(&clean);
            add_event(&app, &m.conversation_id, &m.id, "deploy", "CI and deploy", "deploy", &line, json!({"message_id": m.id, "deploy": clean}), false).await?;
        }
        _ => return Err(Fail::new(400, "kind must be progress, ask, reply, final or deploy")),
    }
    if held {
        audit(&app, &account, &p.id, &agent_name, "reply.held", &m.id, &hits.join(",")).await;
    }
    Ok(Json(json!({"ok": true, "held": held, "secret_hits": hits})))
}

pub fn deploy_line(d: &Value) -> String {
    let s = |k: &str| d.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let stage = s("stage");
    let mut line = match stage.as_str() {
        "fix_committed" => format!("Fix committed{}.", if s("commit").is_empty() { String::new() } else { format!(" ({})", s("commit")) }),
        "ci_running" => "CI is running.".into(),
        "ci_passed" => "CI passed.".into(),
        "ci_failed" => "CI failed.".into(),
        "deploying" => format!("Deploying{}.", if s("env").is_empty() { String::new() } else { format!(" to {}", s("env")) }),
        "deployed" => format!("Deployed{}.", if s("env").is_empty() { String::new() } else { format!(" to {}", s("env")) }),
        "deploy_failed" => "The deploy failed.".into(),
        "verified" => "Smoke check passed on the live app.".into(),
        "ready_to_retest" => "Ready to retest.".into(),
        "not_deploying" => "Fixed in code; not being deployed now.".into(),
        "unknown" => "The pipeline stopped reporting; its state is unknown.".into(),
        other => other.to_string(),
    };
    if !s("note").is_empty() {
        line.push_str(&format!(" {}", s("note")));
    }
    line
}

pub fn agent_label(kind: &str) -> String {
    match kind {
        "claude" => "Claude Code".into(),
        "codex" => "Codex".into(),
        "gemini" => "Gemini CLI".into(),
        "cursor" => "Cursor".into(),
        "copilot" => "Copilot".into(),
        "" => "Agent".into(),
        k => k.to_string(),
    }
}

async fn authorize_report(app: &AppInner, machine: &str, mid: &str, reply_token: &str) -> Result<Message, Fail> {
    let m = domain::message(app, mid).await?;
    let p = domain::project(app, &m.project_id).await?;
    if p.machine_id != machine || m.reply_hash.is_empty() || !crate::auth::constant_eq(&hash(reply_token), &m.reply_hash) {
        return Err(Fail::new(403, "this reply token does not belong to that message"));
    }
    if matches!(m.stage.as_str(), "rejected" | "cancelled" | "expired") {
        return Err(Fail::new(409, "that message is closed"));
    }
    Ok(m)
}

#[derive(Deserialize)]
pub struct AttQ {
    message_id: String,
    reply_token: String,
}

pub async fn report_attachment(State(app): State<App>, headers: HeaderMap, Query(q): Query<AttQ>, body: Bytes) -> Result<Json<Value>, Fail> {
    let (_, machine) = machine_auth(&app, &headers).await?;
    let m = authorize_report(&app, &machine, &q.message_id, &q.reply_token).await?;
    let name = headers.get("x-filename").and_then(|v| v.to_str().ok()).unwrap_or("artifact");
    let a = domain::store_attachment(&app, &m.project_id, &machine, "agent", name, &body, &m.id, &m.conversation_id).await?;
    Ok(Json(json!({"attachment_id": a.id, "name": a.name, "mime": a.mime})))
}

#[derive(Deserialize)]
pub struct SignedQ {
    exp: i64,
    sig: String,
}

pub async fn attachment_download(State(app): State<App>, headers: HeaderMap, Path(aid): Path<String>, Query(q): Query<SignedQ>) -> Result<Response, Fail> {
    let (_, machine) = machine_auth(&app, &headers).await?;
    if q.exp < now().timestamp() || !app.verify(&format!("att:{aid}:{machine}:{}", q.exp), &q.sig) {
        return Err(Fail::new(403, "link expired"));
    }
    let a = sqlx::query_as::<_, domain::Attachment>("SELECT * FROM attachments WHERE id=$1").bind(&aid).fetch_optional(&app.db).await?
        .ok_or_else(|| Fail::new(404, "gone"))?;
    let bytes = app.files.get(&format!("attachments/{aid}")).await?.ok_or_else(|| Fail::new(404, "gone"))?;
    Ok((
        [(header::CONTENT_TYPE, "application/octet-stream".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", a.name)),
         (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string())],
        bytes,
    ).into_response())
}

// ---- developer API for the CLI (machine bearer, owner rights) ----

#[derive(Deserialize)]
pub struct AddProject {
    handle: String,
    #[serde(default)]
    name: String,
    pane_id: String,
    #[serde(default)]
    agent_session: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    cwd: String,
}

pub fn valid_handle(h: &str) -> bool {
    !h.is_empty() && h.len() <= 32 && h.chars().next().unwrap().is_ascii_lowercase()
        && h.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub async fn api_projects(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let (account, _) = machine_auth(&app, &headers).await?;
    let ps = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1 ORDER BY handle").bind(&account).fetch_all(&app.db).await?;
    let mut out = vec![];
    for p in ps {
        let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE project_id=$1 AND stage IN ('pending_approval','held')").bind(&p.id).fetch_one(&app.db).await?;
        out.push(json!({"id": p.id, "handle": p.handle, "name": p.name, "machine_id": p.machine_id, "pane_id": p.pane_id,
            "agent": p.agent, "state": p.state, "mode": if p.auto_active() { "auto" } else { "approve" }, "auto_until": p.auto_until, "pending": pending,
            "gliph_channel": p.gliph_channel}));
    }
    Ok(Json(json!({"projects": out})))
}

pub async fn api_add_project(State(app): State<App>, headers: HeaderMap, Json(a): Json<AddProject>) -> Result<Json<Value>, Fail> {
    let (account, machine) = machine_auth(&app, &headers).await?;
    if !valid_handle(&a.handle) {
        return Err(Fail::new(400, "a handle is lowercase letters, digits and -, starting with a letter, up to 32"));
    }
    let name = if a.name.trim().is_empty() { a.handle.clone() } else { proto::sanitize(&a.name, 64) };
    let pid = id("p");
    let r = sqlx::query("INSERT INTO projects (id, owner_id, handle, name, machine_id, pane_id, agent_session, agent, cwd, state) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'unknown')")
        .bind(&pid).bind(&account).bind(&a.handle).bind(&name).bind(&machine).bind(proto::sanitize(&a.pane_id, 64))
        .bind(proto::sanitize(&a.agent_session, 128)).bind(proto::sanitize(&a.agent, 32)).bind(proto::sanitize(&a.cwd, 512))
        .execute(&app.db).await;
    if let Err(sqlx::Error::Database(e)) = &r {
        if e.is_unique_violation() {
            return Err(Fail::new(409, "you already have a project with that handle"));
        }
    }
    r?;
    sqlx::query("INSERT INTO members (project_id, account_id, role) VALUES ($1,$2,'owner')").bind(&pid).bind(&account).execute(&app.db).await?;
    audit(&app, &account, &pid, &a.handle, "project.created", &pid, &a.pane_id).await;
    push_bindings(&app, &machine).await?;
    Ok(Json(json!({"id": pid, "handle": a.handle})))
}

async fn owned_by_handle(app: &AppInner, account: &str, handle: &str) -> Result<Project, Fail> {
    sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1 AND handle=$2").bind(account).bind(handle)
        .fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "no project with that handle"))
}

#[derive(Deserialize)]
pub struct Bind {
    pane_id: String,
    #[serde(default)]
    agent_session: String,
    #[serde(default)]
    agent: String,
    #[serde(default)]
    cwd: String,
}

/// Explicit developer action: binding (or moving) a project to a pane on this
/// machine. The only way a project ever changes where it delivers.
pub async fn api_bind(State(app): State<App>, headers: HeaderMap, Path(handle): Path<String>, Json(b): Json<Bind>) -> Result<Json<Value>, Fail> {
    let (account, machine) = machine_auth(&app, &headers).await?;
    let p = owned_by_handle(&app, &account, &handle).await?;
    let old_machine = p.machine_id.clone();
    sqlx::query("UPDATE projects SET machine_id=$2, pane_id=$3, agent_session=$4, agent=$5, cwd=$6, state='unknown' WHERE id=$1")
        .bind(&p.id).bind(&machine).bind(proto::sanitize(&b.pane_id, 64)).bind(proto::sanitize(&b.agent_session, 128))
        .bind(proto::sanitize(&b.agent, 32)).bind(proto::sanitize(&b.cwd, 512)).execute(&app.db).await?;
    // Sent but never acknowledged: it goes to the new binding. Anything the
    // old agent already had is interrupted, never resent by itself.
    sqlx::query("UPDATE messages SET stage='approved', delivery_id='' WHERE project_id=$1 AND stage='dispatched'").bind(&p.id).execute(&app.db).await?;
    interrupt_in_flight(&app, &p.id, "The project moved to another pane before the agent finished. The developer can deliver it again.").await?;
    audit(&app, &account, &p.id, &handle, "project.bound", &b.pane_id, &format!("machine {machine}")).await;
    push_bindings(&app, &machine).await?;
    if !old_machine.is_empty() && old_machine != machine {
        push_bindings(&app, &old_machine).await?;
    }
    Ok(Json(json!({"ok": true})))
}

pub async fn api_unbind(State(app): State<App>, headers: HeaderMap, Path(handle): Path<String>) -> Result<Json<Value>, Fail> {
    let (account, machine) = machine_auth(&app, &headers).await?;
    let p = owned_by_handle(&app, &account, &handle).await?;
    sqlx::query("UPDATE projects SET pane_id='', agent_session='', state='unbound' WHERE id=$1").bind(&p.id).execute(&app.db).await?;
    interrupt_in_flight(&app, &p.id, "The project was unbound before the agent finished. The developer can deliver it again.").await?;
    audit(&app, &account, &p.id, &handle, "project.unbound", &p.pane_id, "").await;
    push_bindings(&app, &machine).await?;
    Ok(Json(json!({"ok": true})))
}

#[derive(Deserialize)]
pub struct Mode {
    mode: String,
    #[serde(default)]
    minutes: Option<i64>,
}

pub async fn set_mode(app: &AppInner, account: &str, p: &Project, mode: &str, minutes: Option<i64>, via: &str) -> Result<(), Fail> {
    let until = match (mode, minutes) {
        ("auto", Some(m)) if m > 0 && m <= 60 * 24 * 30 => Some(now() + Duration::minutes(m)),
        ("auto", _) => None,
        ("approve", _) => None,
        _ => return Err(Fail::new(400, "mode is approve or auto")),
    };
    sqlx::query("UPDATE projects SET mode=$2, auto_until=$3 WHERE id=$1").bind(&p.id).bind(mode).bind(until).execute(&app.db).await?;
    let detail = match until { Some(u) => format!("until {} via {via}", u.format("%Y-%m-%d %H:%M UTC")), None => format!("via {via}") };
    audit(app, account, &p.id, &crate::domain::account_label(app, account).await, &format!("project.mode.{mode}"), &p.id, &detail).await;
    Ok(())
}

pub async fn api_mode(State(app): State<App>, headers: HeaderMap, Path(handle): Path<String>, Json(m): Json<Mode>) -> Result<Json<Value>, Fail> {
    let (account, _) = machine_auth(&app, &headers).await?;
    let p = owned_by_handle(&app, &account, &handle).await?;
    set_mode(&app, &account, &p, &m.mode, m.minutes, "cli").await?;
    Ok(Json(json!({"ok": true})))
}

pub async fn api_approvals(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let (account, _) = machine_auth(&app, &headers).await?;
    let ms = sqlx::query_as::<_, Message>(
        "SELECT m.* FROM messages m JOIN projects p ON p.id=m.project_id WHERE p.owner_id=$1 AND m.stage IN ('pending_approval','held') ORDER BY m.order_n",
    ).bind(&account).fetch_all(&app.db).await?;
    Ok(Json(json!({"approvals": ms.iter().map(msg_row).collect::<Vec<_>>()})))
}

pub async fn api_inbox(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let (account, _) = machine_auth(&app, &headers).await?;
    let ms = sqlx::query_as::<_, Message>(
        "SELECT m.* FROM messages m JOIN projects p ON p.id=m.project_id WHERE p.owner_id=$1 ORDER BY m.order_n DESC LIMIT 50",
    ).bind(&account).fetch_all(&app.db).await?;
    Ok(Json(json!({"messages": ms.iter().map(msg_row).collect::<Vec<_>>()})))
}

fn msg_row(m: &Message) -> Value {
    json!({"id": m.id, "project_id": m.project_id, "conversation_id": m.conversation_id, "from": m.sender_label, "client": m.client_name,
        "stage": m.stage, "deployment_stage": m.deploy_stage, "risk": m.risk, "text": m.text, "created_at": m.created_at})
}

pub async fn api_decide(State(app): State<App>, headers: HeaderMap, Path((mid, action)): Path<(String, String)>) -> Result<Json<Value>, Fail> {
    let (account, _) = machine_auth(&app, &headers).await?;
    let m = domain::decide(&app, &account, &mid, &action).await?;
    Ok(Json(json!({"id": m.id, "stage": m.stage})))
}

pub async fn api_whoami(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let (account, machine) = machine_auth(&app, &headers).await?;
    let (email, name): (String, String) = sqlx::query_as("SELECT email, name FROM accounts WHERE id=$1").bind(&account).fetch_one(&app.db).await?;
    let mname: String = sqlx::query_scalar("SELECT name FROM machines WHERE id=$1").bind(&machine).fetch_one(&app.db).await?;
    Ok(Json(json!({"account": email, "name": name, "machine_id": machine, "machine": mname, "online": app.machine_online(&machine)})))
}

pub async fn api_logout(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let (account, machine) = machine_auth(&app, &headers).await?;
    revoke_machine(&app, &account, &machine, "signed out from the CLI").await?;
    Ok(Json(json!({"ok": true})))
}
