//! Messages, their lifecycle, the event log, the delivery pump and the status
//! block a calling assistant reads (docs/substrate.md: "Message lifecycle").

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::FromRow;
use substrate_proto::{self as proto, AttachmentRef, CloudFrame};

use crate::app::{audit, hash, id, now, token, AppInner, Fail, GliphOut};
use crate::safety;

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Project {
    pub id: String,
    pub owner_id: String,
    pub handle: String,
    pub name: String,
    pub description: String,
    pub machine_id: String,
    pub pane_id: String,
    pub agent_session: String,
    pub agent: String,
    pub cwd: String,
    pub state: String,
    pub mode: String,
    pub auto_until: Option<DateTime<Utc>>,
    pub retest_hosts: String,
    pub ci_public: bool,
    pub gliph_channel: String,
    pub created_at: DateTime<Utc>,
    /// Empty until the relay found the project's own logo (`/projects/{id}/icon`).
    pub icon_etag: String,
}

impl Project {
    pub fn auto_active(&self) -> bool {
        self.mode == "auto" && self.auto_until.map(|u| u > now()).unwrap_or(true)
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Message {
    pub id: String,
    pub conversation_id: String,
    pub project_id: String,
    pub sender_id: String,
    pub sender_label: String,
    pub client_name: String,
    pub kind: String,
    pub text: String,
    pub envelope: String,
    pub idem_key: String,
    pub stage: String,
    pub deploy_stage: String,
    pub deploy: Value,
    pub risk: String,
    pub reply_hash: String,
    pub delivery_id: String,
    pub agent_idle: bool,
    pub question: String,
    pub excerpt: String,
    pub order_n: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub deploy_allow: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Event {
    pub conversation_id: String,
    pub seq: i64,
    pub message_id: String,
    pub actor_kind: String,
    pub actor_name: String,
    pub kind: String,
    pub text: String,
    pub data: Value,
    pub held: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct Attachment {
    pub id: String,
    pub project_id: String,
    pub conversation_id: String,
    pub message_id: String,
    pub uploader_id: String,
    pub origin: String,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub sha256: String,
    pub created_at: DateTime<Utc>,
}

/// Stages that mean "this message holds the agent". One per project at a time.
pub const IN_FLIGHT: &[&str] = &["dispatched", "accepted", "submitted", "working"];

pub async fn project(app: &AppInner, pid: &str) -> Result<Project, Fail> {
    sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE id=$1")
        .bind(pid).fetch_optional(&app.db).await?
        .ok_or_else(|| Fail::new(404, "no such project"))
}

pub async fn message(app: &AppInner, mid: &str) -> Result<Message, Fail> {
    sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE id=$1")
        .bind(mid).fetch_optional(&app.db).await?
        .ok_or_else(|| Fail::new(404, "no such message"))
}

/// The caller's role in a project: "owner", "tester", or None.
pub async fn role(app: &AppInner, pid: &str, account: &str) -> Result<Option<String>, Fail> {
    Ok(sqlx::query_scalar::<_, String>("SELECT role FROM members WHERE project_id=$1 AND account_id=$2")
        .bind(pid).bind(account).fetch_optional(&app.db).await?)
}

/// The deploy targets this account may ask for on a project: the owner any, a tester what
/// the developer allowed.
pub async fn deploy_targets(app: &AppInner, pid: &str, account: &str) -> Result<Vec<&'static str>, Fail> {
    let row: Option<(String, bool, bool)> = sqlx::query_as("SELECT role, can_deploy_test, can_deploy_prod FROM members WHERE project_id=$1 AND account_id=$2")
        .bind(pid).bind(account).fetch_optional(&app.db).await?;
    Ok(match row {
        Some((r, _, _)) if r == "owner" => vec!["test", "production"],
        Some((_, t, p)) => [(t, "test"), (p, "production")].into_iter().filter(|x| x.0).map(|x| x.1).collect(),
        None => vec![],
    })
}

pub async fn require_owner(app: &AppInner, pid: &str, account: &str) -> Result<Project, Fail> {
    let p = project(app, pid).await?;
    if p.owner_id != account {
        return Err(Fail::new(403, "only the project's developer can do that"));
    }
    Ok(p)
}

pub async fn account_label(app: &AppInner, account: &str) -> String {
    let r: Option<(String, String)> = sqlx::query_as("SELECT name, email FROM accounts WHERE id=$1")
        .bind(account).fetch_optional(&app.db).await.ok().flatten();
    match r {
        Some((n, _)) if !n.is_empty() => n,
        Some((_, e)) => e.split('@').next().unwrap_or("someone").to_string(),
        None => "someone".into(),
    }
}

/// Append an event to a conversation and tell everyone who is waiting on it:
/// long-polls, live pages and the linked Gliph channel (unless held).
pub async fn add_event(
    app: &AppInner, conv: &str, msg: &str, actor_kind: &str, actor_name: &str, kind: &str, text: &str, data: Value, held: bool,
) -> Result<i64, Fail> {
    let mut tx = app.db.begin().await?;
    let seq: i64 = sqlx::query_scalar("UPDATE conversations SET last_seq=last_seq+1, updated_at=now() WHERE id=$1 RETURNING last_seq")
        .bind(conv).fetch_one(&mut *tx).await?;
    sqlx::query("INSERT INTO events (conversation_id, seq, message_id, actor_kind, actor_name, kind, text, data, held) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(conv).bind(seq).bind(msg).bind(actor_kind).bind(actor_name).bind(kind).bind(text).bind(&data).bind(held)
        .execute(&mut *tx).await?;
    tx.commit().await?;
    app.wake(conv, seq);
    // The owner's pages and apps (Sidra's badge) refresh on every event.
    if let Ok(Some(owner)) = sqlx::query_scalar::<_, String>("SELECT p.owner_id FROM conversations c JOIN projects p ON p.id=c.project_id WHERE c.id=$1").bind(conv).fetch_optional(&app.db).await {
        app.changed(&owner);
    }
    if !held {
        mirror(app, conv, actor_kind, actor_name, kind, text, &data).await;
    }
    Ok(seq)
}

/// One line in the project's Gliph channel, when it has one.
async fn mirror(app: &AppInner, conv: &str, actor_kind: &str, actor_name: &str, kind: &str, text: &str, data: &Value) {
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT p.gliph_channel, p.handle, p.agent FROM conversations c JOIN projects p ON p.id=c.project_id WHERE c.id=$1",
    ).bind(conv).fetch_optional(&app.db).await.ok().flatten();
    let Some((channel, handle, agent_kind)) = row else { return };
    if channel.is_empty() {
        return;
    }
    let mid = data.get("message_id").and_then(|v| v.as_str()).unwrap_or("");
    let short = |s: &str| -> String { let s = s.replace('\n', " "); if s.chars().count() > 400 { format!("{}…", s.chars().take(400).collect::<String>()) } else { s } };
    let body = match actor_kind {
        "human" => format!("[{handle}] {actor_name}: {} · {mid}", short(text)),
        "agent" => format!("[{handle}] {actor_name} ({kind}): {}", short(text)),
        "deploy" => format!("[{handle}] deploy: {}", short(text)),
        _ => format!("[{handle}] {}", short(text)),
    };
    // The real mark of whoever the line is from: the tester's assistant, the
    // project's coding agent, or the CI service.
    let agent = match actor_kind {
        "human" => brand(data.get("client").and_then(|v| v.as_str()).unwrap_or("")),
        "agent" => brand(&agent_kind),
        "deploy" => Some(if data["deploy"]["ci_url"].as_str().unwrap_or("").contains("gitlab") { "gitlab" } else { "github" }),
        _ => None,
    };
    app.gliph(GliphOut::Say { channel, body, agent });
}

/// The Gliph brand name for an agent kind or an assistant's client name.
pub fn brand(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    if n.contains("claude") || n.contains("anthropic") {
        Some("claude")
    } else if n.contains("codex") || n.contains("openai") || n.contains("chatgpt") {
        Some("openai")
    } else if n.contains("gemini") {
        Some("gemini")
    } else if n.contains("cursor") {
        Some("cursor")
    } else if n.contains("copilot") {
        Some("copilot")
    } else {
        None
    }
}

pub async fn set_stage(app: &AppInner, mid: &str, stage: &str) -> Result<(), Fail> {
    sqlx::query("UPDATE messages SET stage=$2, updated_at=now() WHERE id=$1").bind(mid).bind(stage).execute(&app.db).await?;
    Ok(())
}

pub struct NewMessage<'a> {
    pub project_id: &'a str,
    pub sender_id: &'a str,
    /// Shown as the sender; empty means the account's own name.
    pub sender_label: &'a str,
    pub client_name: &'a str,
    pub conversation_id: Option<&'a str>,
    pub kind: &'a str,
    pub text: &'a str,
    pub idem_key: &'a str,
    /// Already-validated attachment ids (uploaded by this sender, unattached).
    pub attachment_ids: &'a [String],
    /// The deploy asked for: "production", "test" or "none" (substrate_proto::EnvelopeInput::deploy).
    pub deploy: Option<&'a str>,
}

pub const MESSAGE_TTL_DAYS: i64 = 7;

pub async fn create_message(app: &AppInner, m: NewMessage<'_>) -> Result<Message, Fail> {
    let p = project(app, m.project_id).await?;
    let role = role(app, &p.id, m.sender_id).await?.ok_or_else(|| Fail::new(403, "you have no access to this project"))?;

    if !m.idem_key.is_empty() {
        if let Some(existing) = sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE sender_id=$1 AND idem_key=$2")
            .bind(m.sender_id).bind(m.idem_key).fetch_optional(&app.db).await?
        {
            return Ok(existing);
        }
    }
    if !app.limiter.allow(&format!("send:{}", m.sender_id), 10.0 / 60.0, 10.0) {
        return Err(Fail::new(429, "too many messages; wait a minute and try again"));
    }
    let today: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE project_id=$1 AND sender_id=$2 AND created_at > now() - interval '1 day'")
        .bind(&p.id).bind(m.sender_id).fetch_one(&app.db).await?;
    if today >= 200 {
        return Err(Fail::new(429, "daily message limit for this project reached"));
    }

    let text = proto::sanitize(m.text, 8000);
    if text.is_empty() {
        return Err(Fail::new(400, "the message is empty"));
    }
    if proto::forges_envelope(&text) {
        audit(app, m.sender_id, &p.id, m.sender_id, "message.refused", "", "envelope forgery").await;
        return Err(Fail::new(400, "the message contains text that imitates Substrate's own framing; rephrase it"));
    }

    let conv = match m.conversation_id.filter(|c| !c.is_empty()) {
        Some(cid) => {
            let ok: Option<(String, String)> = sqlx::query_as("SELECT project_id, sender_id FROM conversations WHERE id=$1")
                .bind(cid).fetch_optional(&app.db).await?;
            match ok {
                Some((pid, sid)) if pid == p.id && (sid == m.sender_id || role == "owner") => cid.to_string(),
                _ => return Err(Fail::new(404, "no such conversation")),
            }
        }
        None => {
            let cid = id("c");
            let title: String = text.lines().next().unwrap_or("").chars().take(72).collect();
            sqlx::query("INSERT INTO conversations (id, project_id, sender_id, title) VALUES ($1,$2,$3,$4)")
                .bind(&cid).bind(&p.id).bind(m.sender_id).bind(&title).execute(&app.db).await?;
            cid
        }
    };

    let label = if m.sender_label.is_empty() { account_label(app, m.sender_id).await } else { m.sender_label.to_string() };
    let mid = id("m");
    let names: Vec<String> = if m.attachment_ids.is_empty() {
        vec![]
    } else {
        sqlx::query_scalar("SELECT name FROM attachments WHERE id = ANY($1) AND uploader_id=$2 AND project_id=$3 AND message_id=''")
            .bind(m.attachment_ids).bind(m.sender_id).bind(&p.id).fetch_all(&app.db).await?
    };
    if names.len() != m.attachment_ids.len() {
        return Err(Fail::new(400, "an attachment is unknown, already used, or belongs to another project"));
    }
    if let Some(t) = m.deploy.filter(|t| *t != "none") {
        if !deploy_targets(app, m.project_id, m.sender_id).await?.contains(&t) {
            return Err(Fail::new(403, if t == "production" { "The developer has not allowed you to deploy to production." } else { "The developer has not allowed you to deploy to test." }));
        }
    }
    let envelope = proto::envelope(&proto::EnvelopeInput {
        message_id: &mid, project_handle: &p.handle, project_path: &p.cwd, sender: &label,
        client: m.client_name, text: &text, attachments: &names, nonce: &crate::app::token_n(12), deploy: m.deploy,
    });

    let flags = safety::risk_flags(&text);
    let trusted: bool = sqlx::query_scalar("SELECT trusted_auto FROM members WHERE project_id=$1 AND account_id=$2")
        .bind(&p.id).bind(m.sender_id).fetch_optional(&app.db).await?.unwrap_or(false);
    let auto = role == "owner" || p.auto_active() || trusted;
    let stage = if !flags.is_empty() { "held" } else if auto { "approved" } else { "pending_approval" };
    // What the agent may deploy while it works on this; the relay's guard enforces it.
    // Checked above against the sender's permissions. An answer keeps its message's.
    let deploy: String = match m.deploy {
        Some(d) => d.to_string(),
        None => match m.conversation_id {
            Some(c) if m.kind == "answer" => sqlx::query_scalar("SELECT deploy_allow FROM messages WHERE conversation_id=$1 ORDER BY order_n DESC LIMIT 1")
                .bind(c).fetch_optional(&app.db).await?.unwrap_or_else(|| "none".into()),
            _ if role == "owner" => "production".into(),
            _ => "none".into(),
        },
    };

    let inserted = sqlx::query_as::<_, Message>(
        "INSERT INTO messages (id, conversation_id, project_id, sender_id, sender_label, client_name, kind, text, envelope, idem_key, stage, risk, expires_at, deploy_allow)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
         ON CONFLICT (sender_id, idem_key) WHERE idem_key <> '' DO NOTHING RETURNING *",
    )
    .bind(&mid).bind(&conv).bind(&p.id).bind(m.sender_id).bind(&label).bind(m.client_name).bind(m.kind).bind(&text)
    .bind(&envelope).bind(m.idem_key).bind(stage).bind(flags.join(",")).bind(now() + Duration::days(MESSAGE_TTL_DAYS)).bind(&deploy)
    .fetch_optional(&app.db).await?;
    let Some(_inserted) = inserted else {
        // A concurrent call with the same idempotency key won the race.
        return Ok(sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE sender_id=$1 AND idem_key=$2")
            .bind(m.sender_id).bind(m.idem_key).fetch_one(&app.db).await?);
    };
    if !m.attachment_ids.is_empty() {
        sqlx::query("UPDATE attachments SET message_id=$1, conversation_id=$2 WHERE id = ANY($3)")
            .bind(&mid).bind(&conv).bind(m.attachment_ids).execute(&app.db).await?;
    }

    let via = if m.client_name.is_empty() { String::new() } else { format!(" via {}", m.client_name) };
    add_event(app, &conv, &mid, "human", &label, m.kind, &text,
        json!({"message_id": mid, "client": m.client_name, "attachments": names}), false).await?;

    let (sys, detail) = match stage {
        "held" => (format!("Held for the developer. It matched {}. The agent has not received it.", flags.join(", ")), "held"),
        "approved" if role == "owner" => ("Sent by the developer. Queued for the agent.".to_string(), "auto"),
        "approved" => ("Auto relay is on. Queued for the agent without approval.".to_string(), "auto"),
        _ => ("Waiting for the developer's approval. The agent has not received it yet.".to_string(), "pending"),
    };
    add_event(app, &conv, &mid, "system", "Substrate", "stage", &sys, json!({"message_id": mid, "stage": stage}), false).await?;
    audit(app, m.sender_id, &p.id, &format!("{label}{via}"), "message.received", &mid, detail).await;
    if stage == "pending_approval" || stage == "held" {
        crate::push::approval_waiting(app.db.clone(), p.owner_id.clone(), mid.clone(), p.name.clone(), label.clone(), text.clone(), stage == "held");
    }
    if stage == "approved" {
        pump(app, &p.id).await?;
    }
    message(app, &mid).await
}

/// Owner decisions on a message.
pub async fn decide(app: &AppInner, owner: &str, mid: &str, action: &str) -> Result<Message, Fail> {
    let m = message(app, mid).await?;
    let p = require_owner(app, &m.project_id, owner).await?;
    let who = account_label(app, owner).await;
    let (from, to, text): (&[&str], &str, String) = match action {
        "approve" => (&["pending_approval", "held"], "approved", "Approved by the developer.".into()),
        "reject" => (&["pending_approval", "held", "approved"], "rejected", "The developer declined this message. The agent did not get it.".into()),
        "cancel" => (&["approved", "dispatched", "accepted"], "cancelled", "Cancelled by the developer before the agent received it.".into()),
        "retry" => (&["uncertain", "failed", "interrupted", "expired"], "approved", "The developer sent it again.".into()),
        _ => return Err(Fail::new(400, "unknown action")),
    };
    if !from.contains(&m.stage.as_str()) {
        return Err(Fail::new(409, format!("the message is {} and cannot be {}d now", m.stage.replace('_', " "), action)));
    }
    if action == "cancel" && !m.delivery_id.is_empty() {
        app.send_machine(&p.machine_id, CloudFrame::Cancel { delivery_id: m.delivery_id.clone() });
    }
    let expires = if action == "retry" { now() + Duration::days(MESSAGE_TTL_DAYS) } else { m.expires_at };
    sqlx::query("UPDATE messages SET stage=$2, expires_at=$3, delivery_id=CASE WHEN $2='approved' THEN '' ELSE delivery_id END, agent_idle=false, updated_at=now() WHERE id=$1")
        .bind(mid).bind(to).bind(expires).execute(&app.db).await?;
    add_event(app, &m.conversation_id, mid, "system", "Substrate", "stage", &text, json!({"message_id": mid, "stage": to, "by": who}), false).await?;
    audit(app, owner, &p.id, &who, &format!("message.{action}"), mid, &m.stage).await;
    if to == "approved" {
        pump(app, &p.id).await?;
    }
    message(app, mid).await
}

/// Deliver the next approved message for a project if the agent is free and
/// its relay is online. Ordered per project; projects never wait on each other.
pub async fn pump(app: &AppInner, pid: &str) -> Result<(), Fail> {
    let p = project(app, pid).await?;
    if p.machine_id.is_empty() || p.pane_id.is_empty() || !app.machine_online(&p.machine_id) {
        return Ok(());
    }
    if matches!(p.state.as_str(), "needs_rebind" | "offline" | "unbound") {
        return Ok(());
    }
    let busy: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE project_id=$1 AND stage = ANY($2) AND NOT agent_idle")
        .bind(pid).bind(IN_FLIGHT).fetch_one(&app.db).await?;
    if busy > 0 {
        return Ok(());
    }
    let next = sqlx::query_as::<_, Message>(
        "SELECT * FROM messages WHERE project_id=$1 AND stage='approved' ORDER BY order_n LIMIT 1",
    ).bind(pid).fetch_optional(&app.db).await?;
    let Some(m) = next else { return Ok(()) };

    // Revalidate at the last moment: approval may be days old.
    if m.expires_at < now() {
        set_stage(app, &m.id, "expired").await?;
        add_event(app, &m.conversation_id, &m.id, "system", "Substrate", "stage", "Expired before it could be delivered.", json!({"message_id": m.id, "stage": "expired"}), false).await?;
        return Box::pin(pump(app, pid)).await;
    }
    if role(app, pid, &m.sender_id).await?.is_none() {
        set_stage(app, &m.id, "cancelled").await?;
        add_event(app, &m.conversation_id, &m.id, "system", "Substrate", "stage", "Not delivered. The sender lost access to this project.", json!({"message_id": m.id, "stage": "cancelled"}), false).await?;
        return Box::pin(pump(app, pid)).await;
    }

    let reply_token = token();
    let delivery = id("d");
    sqlx::query("UPDATE messages SET stage='dispatched', delivery_id=$2, reply_hash=$3, agent_idle=false, updated_at=now() WHERE id=$1")
        .bind(&m.id).bind(&delivery).bind(hash(&reply_token)).execute(&app.db).await?;
    let atts = sqlx::query_as::<_, Attachment>("SELECT * FROM attachments WHERE message_id=$1 AND origin='tester' ORDER BY created_at")
        .bind(&m.id).fetch_all(&app.db).await?;
    let attachments = atts.iter().map(|a| AttachmentRef {
        name: a.name.clone(),
        url: signed_attachment_url(app, &a.id, &p.machine_id),
        size: a.size as u64,
        sha256: a.sha256.clone(),
    }).collect();
    let sent = app.send_machine(&p.machine_id, CloudFrame::Deliver {
        delivery_id: delivery.clone(), message_id: m.id.clone(), project_id: pid.to_string(),
        envelope: m.envelope.clone(), reply_token, attachments, deploy: m.deploy_allow.clone(),
    });
    if !sent {
        // The socket closed between the check and the send: back to the queue.
        sqlx::query("UPDATE messages SET stage='approved', delivery_id='' WHERE id=$1").bind(&m.id).execute(&app.db).await?;
        return Ok(());
    }
    add_event(app, &m.conversation_id, &m.id, "relay", &p.handle, "stage", "Sent to the developer's relay.", json!({"message_id": m.id, "stage": "dispatched"}), false).await?;
    Ok(())
}

/// A download URL only the given machine can use, for 30 minutes.
pub fn signed_attachment_url(app: &AppInner, att: &str, machine: &str) -> String {
    let exp = (now() + Duration::minutes(30)).timestamp();
    let sig = app.sign(&format!("att:{att}:{machine}:{exp}"));
    format!("{}/relay/attachments/{att}?exp={exp}&sig={sig}", app.cfg.public_url)
}

/// The block every message result carries for a calling assistant
/// (docs/substrate.md §8a). `tester` hides held events and raw CI URLs.
pub async fn status_block(app: &AppInner, m: &Message, since: i64, tester: bool) -> Result<Value, Fail> {
    let p = project(app, &m.project_id).await?;
    let online = app.machine_online(&p.machine_id);
    let seq: i64 = sqlx::query_scalar("SELECT last_seq FROM conversations WHERE id=$1").bind(&m.conversation_id).fetch_one(&app.db).await?;
    let events = sqlx::query_as::<_, Event>("SELECT * FROM events WHERE conversation_id=$1 AND seq > $2 ORDER BY seq LIMIT 100")
        .bind(&m.conversation_id).bind(since).fetch_all(&app.db).await?;
    let own_host = app.cfg.host();
    let held_reply = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM events WHERE message_id=$1 AND held")
        .bind(&m.id).fetch_one(&app.db).await? > 0;

    let atts = sqlx::query_as::<_, Attachment>("SELECT * FROM attachments WHERE conversation_id=$1 ORDER BY created_at").bind(&m.conversation_id).fetch_all(&app.db).await?;
    let new_events: Vec<Value> = events.iter().filter(|e| !(tester && e.held)).map(|e| {
        let mut v = json!({"seq": e.seq, "time": e.created_at, "actor": e.actor_kind, "name": e.actor_name, "kind": e.kind});
        // The agent's event lists its files by id; a person's message owns the
        // files uploaded with it. The MCP layer adds a signed URL per viewer.
        let ids: Vec<&str> = e.data.get("attachments").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        let mine: Vec<Value> = atts.iter()
            .filter(|x| if e.actor_kind == "agent" { ids.contains(&x.id.as_str()) } else { e.actor_kind == "human" && x.message_id == e.message_id && x.origin == "tester" })
            .map(|x| json!({"id": x.id, "name": x.name, "type": x.mime, "size": x.size})).collect();
        if !mine.is_empty() {
            v["attachments"] = json!(mine);
        }
        if e.actor_kind == "agent" || e.actor_kind == "deploy" {
            v["agent_text"] = json!(safety::defang_links(&e.text, &own_host, &p.retest_hosts));
        } else {
            v["text"] = json!(e.text);
        }
        v
    }).collect();

    let d = &m.deploy;
    let verify = d.get("verify_url").and_then(|v| v.as_str()).unwrap_or("");
    let ci = d.get("ci_url").and_then(|v| v.as_str()).unwrap_or("");
    let mut links = json!({ "conversation_url": format!("{}/conversations/{}", app.cfg.public_url, m.conversation_id) });
    if !m.deploy_stage.is_empty() {
        links["ci_summary_url"] = json!(format!("{}/conversations/{}#deploy", app.cfg.public_url, m.conversation_id));
    }
    if !verify.is_empty() && safety::host_allowed(verify, &own_host, &p.retest_hosts) {
        links["deployment_url"] = json!(verify);
    }
    if !ci.is_empty() && (p.ci_public || !tester) {
        links["ci_url"] = json!(ci);
    }

    let (summary, next, check, terminal) = summarize(m, online, &p, held_reply);
    let mut v = json!({
        "message_id": m.id, "conversation_id": m.conversation_id, "project": p.handle,
        "seq": seq, "stage": m.stage, "deployment_stage": if m.deploy_stage.is_empty() { Value::Null } else { json!(m.deploy_stage) },
        "terminal": terminal, "summary": summary, "next_action": next,
        "check_after_seconds": check, "agent_online": online, "new_events": new_events, "links": links,
    });
    if m.stage == "awaiting_sender" && !m.question.is_empty() && !held_reply {
        v["question"] = json!({"agent_text": safety::defang_links(&m.question, &own_host, &p.retest_hosts)});
    }
    Ok(v)
}

/// The plain sentence and the next step for the human, from observed state
/// only. Never claims more than the stage proves.
pub fn summarize(m: &Message, online: bool, p: &Project, held_reply: bool) -> (String, &'static str, Option<i64>, bool) {
    if held_reply {
        return ("The agent replied. The developer reviews the reply before you see it.".into(), "check_later", Some(600), false);
    }
    let dep = m.deploy_stage.as_str();
    match m.stage.as_str() {
        "pending_approval" => ("Delivered to Substrate. Waiting for the developer to approve it.".into(), "check_later", Some(300), false),
        "held" => ("Delivered to Substrate. The developer reviews it first.".into(), "check_later", Some(600), false),
        "approved" if !online || p.machine_id.is_empty() => ("Approved. The developer's agent is offline. It gets the message when it reconnects.".into(), "check_later", Some(600), false),
        "approved" if p.state == "needs_rebind" => ("Approved. The developer must reconnect the project to its agent first.".into(), "check_later", Some(600), false),
        "approved" => ("Approved and queued for the developer's agent.".into(), "wait", Some(60), false),
        "dispatched" | "accepted" => ("On the developer's computer. Waiting for the agent to be free.".into(), "wait", Some(60), false),
        "submitted" | "working" if m.agent_idle => ("The developer's agent stopped without a reply.".into(), "check_later", Some(300), false),
        "submitted" | "working" => ("The developer's agent is working on it.".into(), "wait", Some(60), false),
        "awaiting_sender" => ("The developer's agent has a question for you.".into(), "answer_question", None, false),
        "answered" => ("You answered the agent's question. Your answer is a new message in this conversation.".into(), "nothing", None, true),
        "rejected" => ("The developer declined this message. The agent did not get it.".into(), "nothing", None, true),
        "expired" => ("It expired before the developer's agent received it.".into(), "contact_developer", None, true),
        "cancelled" => ("It was cancelled before the developer's agent received it.".into(), "nothing", None, true),
        "uncertain" => ("Delivery is unconfirmed. The developer's computer restarted during delivery.".into(), "contact_developer", None, false),
        "failed" => ("Delivery failed. The developer can see the error.".into(), "contact_developer", None, false),
        "interrupted" => ("The agent stopped before finishing. The developer can resume it.".into(), "contact_developer", None, false),
        "completed" => match dep {
            "ready_to_retest" => ("The fix is live. Try it again.".into(), "retest", None, true),
            "verified" => ("The fix is deployed and verified. Try it again.".into(), "retest", None, true),
            "deployed" => ("The fix is deployed. Try it again.".into(), "retest", None, true),
            "fix_committed" | "ci_running" | "ci_passed" | "deploying" => ("Fixed in code. The fix is on its way to the live app.".into(), "wait", Some(120), false),
            "ci_failed" => ("Fixed in code. The build failed, so the fix is not live.".into(), "check_later", Some(900), false),
            "deploy_failed" => ("Fixed in code. The deploy failed, so the fix is not live.".into(), "check_later", Some(900), false),
            "not_deploying" => ("Fixed in code and not deployed yet. Wait before you retest.".into(), "nothing", None, true),
            "unknown" => ("Fixed in code. The deploy state is unknown.".into(), "contact_developer", None, true),
            _ => ("The developer's agent replied.".into(), "nothing", None, true),
        },
        _ => ("In progress.".into(), "check_later", Some(300), false),
    }
}

/// Mark every project on a machine offline (socket closed) or back.
pub async fn machine_presence(app: &AppInner, machine: &str, online: bool) -> Result<(), Fail> {
    if online {
        sqlx::query("UPDATE machines SET last_seen=now() WHERE id=$1").bind(machine).execute(&app.db).await?;
        sqlx::query("UPDATE projects SET state='unknown' WHERE machine_id=$1 AND state='offline'").bind(machine).execute(&app.db).await?;
    } else {
        sqlx::query("UPDATE machines SET last_seen=now() WHERE id=$1").bind(machine).execute(&app.db).await?;
        sqlx::query("UPDATE projects SET state='offline' WHERE machine_id=$1 AND state <> 'unbound'").bind(machine).execute(&app.db).await?;
    }
    if let Ok(Some(acct)) = sqlx::query_scalar::<_, String>("SELECT account_id FROM machines WHERE id=$1").bind(machine).fetch_optional(&app.db).await {
        app.changed(&acct);
    }
    Ok(())
}

/// Every minute: expire what waited too long.
pub async fn housekeeping(app: &AppInner) -> Result<(), Fail> {
    let expired = sqlx::query_as::<_, Message>(
        "UPDATE messages SET stage='expired', updated_at=now() WHERE stage IN ('pending_approval','held','approved') AND expires_at < now() RETURNING *",
    ).fetch_all(&app.db).await?;
    for m in expired {
        add_event(app, &m.conversation_id, &m.id, "system", "Substrate", "stage", "Expired before it could be delivered.", json!({"message_id": m.id, "stage": "expired"}), false).await?;
    }
    // Projects are 1-1 with Herdr panes: one with no pane goes now, one whose
    // agent left goes after 10 minutes (a restart in its folder keeps it), queue and all.
    let gone = sqlx::query_as::<_, Project>(
        "SELECT * FROM projects WHERE pane_id='' OR state='unbound' OR (state='needs_rebind' AND gone_at < now() - interval '10 minutes')",
    ).fetch_all(&app.db).await?;
    for p in gone {
        crate::app::audit(app, &p.owner_id, "", "Substrate", "project.cleaned_up", &p.handle, "agent gone for 10 minutes").await;
        delete_project(app, &p).await?;
        app.changed(&p.owner_id);
    }
    sqlx::query("DELETE FROM tokens WHERE expires_at < now() - interval '1 day'").execute(&app.db).await?;
    // The audit log keeps 180 days.
    sqlx::query("DELETE FROM audit WHERE created_at < now() - interval '180 days'").execute(&app.db).await?;
    sqlx::query("DELETE FROM pairings WHERE expires_at < now() - interval '1 day'").execute(&app.db).await?;
    Ok(())
}

/// Delete a project with its conversations and files, and tell its relay.
pub async fn delete_project(app: &AppInner, p: &Project) -> Result<(), Fail> {
    let files: Vec<String> = sqlx::query_scalar("SELECT id FROM attachments WHERE project_id=$1").bind(&p.id).fetch_all(&app.db).await?;
    sqlx::query("DELETE FROM projects WHERE id=$1").bind(&p.id).execute(&app.db).await?;
    for f in files {
        let _ = app.files.delete(&format!("attachments/{f}")).await;
    }
    if !p.machine_id.is_empty() {
        crate::relay::push_bindings(app, &p.machine_id).await?;
    }
    Ok(())
}

/// Store bytes as an attachment after sniffing what they really are.
pub async fn store_attachment(
    app: &AppInner, project: &str, uploader: &str, origin: &str, name: &str, bytes: &[u8], message: &str, conversation: &str,
) -> Result<Attachment, Fail> {
    if bytes.is_empty() || bytes.len() > crate::files::MAX_ATTACHMENT {
        return Err(Fail::new(413, "attachments must be between 1 byte and 10 MB"));
    }
    let name = proto::safe_filename(name);
    let mime = crate::files::sniff(bytes, &name).ok_or_else(|| Fail::new(415, "only PNG, JPEG, WebP, PDF and plain text files are accepted"))?;
    let aid = id("a");
    let sha = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(bytes));
    app.files.put(&format!("attachments/{aid}"), bytes).await?;
    Ok(sqlx::query_as::<_, Attachment>(
        "INSERT INTO attachments (id, project_id, conversation_id, message_id, uploader_id, origin, name, mime, size, sha256) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *",
    ).bind(&aid).bind(project).bind(conversation).bind(message).bind(uploader).bind(origin).bind(&name).bind(mime)
     .bind(bytes.len() as i64).bind(&sha).fetch_one(&app.db).await?)
}

#[cfg(test)]
mod tests {
    use super::brand;

    #[test]
    fn brands_for_gliph_logos() {
        assert_eq!(brand("claude"), Some("claude"));
        assert_eq!(brand("Claude"), Some("claude"));
        assert_eq!(brand("codex"), Some("openai"));
        assert_eq!(brand("ChatGPT"), Some("openai"));
        assert_eq!(brand("gemini"), Some("gemini"));
        assert_eq!(brand("personal token"), None);
        assert_eq!(brand("Gliph"), None);
    }
}
