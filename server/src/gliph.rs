//! Substrate inside Gliph: a plugin connection to gliph.sh (docs/protocol.md,
//! "Plugins and agents") that registers `/substrate` at runtime, mirrors every
//! linked project's conversation into its channel, and turns `@handle …` in a
//! linked channel into a message to that project's agent: agent to agent.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use crate::app::{App, GliphOut};
use crate::domain::{self, NewMessage, Project};
use crate::relay::set_mode;

const DISPLAY: &str = "substrate";

pub fn spawn(app: App) {
    if app.cfg.gliph_plugin_token.is_empty() {
        tracing::info!("gliph plugin: no GLIPH_PLUGIN_TOKEN, not connecting");
        return;
    }
    let (tx, rx) = mpsc::unbounded_channel();
    *app.gliph_out.lock().unwrap() = Some(tx);
    tokio::spawn(run(app, rx));
}

async fn run(app: App, mut rx: mpsc::UnboundedReceiver<GliphOut>) {
    let mut backoff = 1u64;
    loop {
        match session(&app, &mut rx).await {
            Ok(()) => backoff = 1,
            Err(e) => tracing::warn!(error = %e, "gliph plugin disconnected"),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(60);
    }
}

async fn register_frame(app: &App) -> anyhow::Result<String> {
    let channels: Vec<String> = sqlx::query_scalar("SELECT DISTINCT gliph_channel FROM projects WHERE gliph_channel<>''").fetch_all(&app.db).await?;
    Ok(json!({"t": "register", "d": {
        "id": DISPLAY, "display": DISPLAY, "kind": "agent",
        "commands": [{
            "name": "substrate",
            "usage": "/substrate link|unlink|status|approve|reject|send|mode …",
            "desc": "Substrate: message your coding agents from this channel",
            "args": [{"name": "command", "type": "text", "required": false}],
            "scope": "channel", "min_role": "member"
        }],
        "subscribe": channels
    }}).to_string())
}

async fn session(app: &App, rx: &mut mpsc::UnboundedReceiver<GliphOut>) -> anyhow::Result<()> {
    let base = app.cfg.gliph_url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    let url = format!("{base}/plugin?token={}", app.cfg.gliph_plugin_token);
    let (ws, _) = tokio_tungstenite::connect_async(url).await?;
    let (mut sink, mut stream) = ws.split();
    sink.send(WsMsg::Text(register_frame(app).await?.into())).await?;
    tracing::info!("gliph plugin connected");
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    loop {
        tokio::select! {
            out = rx.recv() => {
                let Some(out) = out else { return Ok(()) };
                let frame = match out {
                    GliphOut::Say { channel, body, agent } => json!({"t": "say", "d": {"channel": channel, "body": body, "agent": agent.unwrap_or("")}}).to_string(),
                    GliphOut::Resubscribe => register_frame(app).await?,
                };
                sink.send(WsMsg::Text(frame.into())).await?;
            }
            msg = stream.next() => {
                let Some(msg) = msg else { return Ok(()) };
                let WsMsg::Text(t) = msg? else { continue };
                let Ok(f) = serde_json::from_str::<Value>(&t) else { continue };
                let d = f.get("d").cloned().unwrap_or(json!({}));
                match f.get("t").and_then(|v| v.as_str()) {
                    Some("invoke") => {
                        let call = d["call"].as_str().unwrap_or("").to_string();
                        let reply = match invoke(app, &d).await {
                            Ok(say) => json!({"t": "result", "d": {"call": call, "say": say}}),
                            Err(msg) => json!({"t": "fail", "d": {"call": call, "message": msg}}),
                        };
                        sink.send(WsMsg::Text(reply.to_string().into())).await?;
                    }
                    Some("event") => { if let Err(e) = on_event(app, &d).await { tracing::debug!(error = %e, "gliph event"); } }
                    Some("error") => tracing::warn!(frame = %t, "gliph plugin error"),
                    _ => {}
                }
            }
            _ = ping.tick() => { sink.send(WsMsg::Ping(Default::default())).await?; }
        }
    }
}

async fn account_for_nick(app: &App, nick: &str) -> Option<String> {
    sqlx::query_scalar("SELECT id FROM accounts WHERE gliph_nick=$1").bind(nick).fetch_optional(&app.db).await.ok().flatten()
}

fn sign_in_hint(app: &App) -> String {
    format!("Sign in to Substrate with Gliph once, then try again: {}/auth/gliph", app.cfg.public_url)
}

/// The project a command means: by handle among the caller's projects, or the
/// one linked to this channel.
async fn resolve(app: &App, account: &str, handle: Option<&str>, channel: &str) -> Result<Project, String> {
    let rows: Vec<Project> = match handle {
        Some(h) => sqlx::query_as("SELECT p.* FROM projects p JOIN members m ON m.project_id=p.id AND m.account_id=$1 WHERE p.handle=$2")
            .bind(account).bind(h).fetch_all(&app.db).await.map_err(|_| "database error".to_string())?,
        None => sqlx::query_as("SELECT p.* FROM projects p JOIN members m ON m.project_id=p.id AND m.account_id=$1 WHERE p.gliph_channel=$2")
            .bind(account).bind(channel).fetch_all(&app.db).await.map_err(|_| "database error".to_string())?,
    };
    match rows.len() {
        1 => Ok(rows.into_iter().next().unwrap()),
        0 => Err(match handle { Some(h) => format!("no project '{h}' that you can use"), None => "name a project: no project is linked to this channel".into() }),
        _ => Err("more than one project matches; use its handle".into()),
    }
}

async fn invoke(app: &App, d: &Value) -> Result<String, String> {
    let channel = d["channel"].as_str().unwrap_or("");
    let actor = d["actor"].as_str().unwrap_or("");
    let args = d["args"].as_str().unwrap_or("").trim().to_string();
    let mut words = args.splitn(3, char::is_whitespace);
    let sub = words.next().unwrap_or("").to_lowercase();
    let a1 = words.next().map(str::to_string);
    let rest = words.next().unwrap_or("").to_string();
    if sub.is_empty() || sub == "help" {
        return Ok("Substrate: /invite substrate adds it to a channel. Then /substrate link <project> · unlink · status [project] · approve <message> · reject <message> · send <project> <text> · mode <project> approve|auto [1h|8h]. In a linked channel, type '@<project> <text>' to message its agent.".into());
    }
    let account = account_for_nick(app, actor).await.ok_or_else(|| sign_in_hint(app))?;
    let fail = |e: crate::app::Fail| e.1;
    match sub.as_str() {
        "link" => {
            let h = a1.ok_or("usage: /substrate link <project>")?;
            let p = resolve(app, &account, Some(&h), channel).await?;
            if p.owner_id != account {
                return Err("only the project's developer can link it".into());
            }
            sqlx::query("UPDATE projects SET gliph_channel=$2 WHERE id=$1").bind(&p.id).bind(channel).execute(&app.db).await.map_err(|_| "database error")?;
            crate::app::audit(app, &account, &p.id, actor, "gliph.linked", channel, "").await;
            app.gliph(GliphOut::Resubscribe);
            Ok(format!("Linked {}. Its conversations appear here. Type '@{} <text>' to message its agent.", p.name, p.handle))
        }
        "unlink" => {
            let p = resolve(app, &account, a1.as_deref(), channel).await?;
            if p.owner_id != account {
                return Err("only the project's developer can unlink it".into());
            }
            sqlx::query("UPDATE projects SET gliph_channel='' WHERE id=$1").bind(&p.id).execute(&app.db).await.map_err(|_| "database error")?;
            crate::app::audit(app, &account, &p.id, actor, "gliph.unlinked", channel, "").await;
            app.gliph(GliphOut::Resubscribe);
            Ok(format!("Unlinked {}.", p.name))
        }
        "status" => {
            let p = resolve(app, &account, a1.as_deref(), channel).await?;
            let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE project_id=$1 AND stage IN ('pending_approval','held','reviewing')").bind(&p.id).fetch_one(&app.db).await.unwrap_or(0);
            Ok(format!("{}: agent {} ({}), relay {}, {} waiting for approval, {}.", p.name, crate::relay::agent_label(&p.agent), p.state.replace('_', " "),
                if app.machine_online(&p.machine_id) { "online" } else { "offline" }, waiting, if p.auto_active() { "auto relay on" } else { "approval required" }))
        }
        "approve" | "reject" => {
            let mid = a1.ok_or("usage: /substrate approve <message id>")?;
            let m = domain::decide(app, &account, &mid, &sub, None, None).await.map_err(fail)?;
            Ok(format!("{} {} ({}).", if sub == "approve" { "Approved" } else { "Rejected" }, m.id, m.stage.replace('_', " ")))
        }
        "send" => {
            let h = a1.ok_or("usage: /substrate send <project> <text>")?;
            let p = resolve(app, &account, Some(&h), channel).await?;
            let m = domain::create_message(app, NewMessage {
                project_id: &p.id, sender_id: &account, sender_label: "", client_name: "Gliph", conversation_id: None,
                kind: "task", text: &rest, idem_key: "", attachment_ids: &[], deploy: None,
            }).await.map_err(fail)?;
            Ok(format!("Sent to {} ({}, {}).", p.name, m.id, m.stage.replace('_', " ")))
        }
        "mode" => {
            let h = a1.ok_or("usage: /substrate mode <project> approve|auto [1h|8h]")?;
            let p = resolve(app, &account, Some(&h), channel).await?;
            if p.owner_id != account {
                return Err("only the project's developer can change the relay mode".into());
            }
            let mut it = rest.split_whitespace();
            let mode = it.next().unwrap_or("");
            let minutes = match it.next() { Some("1h") => Some(60), Some("8h") => Some(480), Some(_) => return Err("duration is 1h or 8h (or nothing: until turned off)".into()), None => None };
            set_mode(app, &account, &p, mode, minutes, "gliph").await.map_err(fail)?;
            Ok(if mode == "auto" { format!("Auto relay is on for {}. Messages reach its agent without approval. Flagged ones still wait.", p.name) } else { format!("{} is back to approval required.", p.name) })
        }
        _ => Err("unknown subcommand; try /substrate help".into()),
    }
}

/// `@handle text` in a linked channel, from a member or an agent plugin: a
/// message to that project's agent, sent as the developer who linked the
/// channel and labelled with who said it. It still goes through approval
/// unless auto relay is on, because channel members are not all trusted.
async fn on_event(app: &App, e: &Value) -> anyhow::Result<()> {
    if e["enc"].as_bool().unwrap_or(false) || !matches!(e["kind"].as_str(), Some("message") | Some("agent")) {
        return Ok(());
    }
    let actor = e["actor"].as_str().unwrap_or("");
    if actor == DISPLAY {
        return Ok(());
    }
    let body = e["body"].as_str().unwrap_or("").trim();
    let Some(rest) = body.strip_prefix('@') else { return Ok(()) };
    let (handle, text) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let channel = e["channel"].as_str().unwrap_or("");
    let p: Option<Project> = sqlx::query_as("SELECT * FROM projects WHERE gliph_channel=$1 AND handle=$2").bind(channel).bind(handle).fetch_optional(&app.db).await?;
    let Some(p) = p else { return Ok(()) };
    if text.trim().is_empty() {
        return Ok(());
    }
    let label = format!("{actor} in Gliph");
    // The linked developer's own nick sends as the developer (auto-approved, as
    // their own messages are); anyone else is a guest the developer approves.
    let own = account_for_nick(app, actor).await.as_deref() == Some(p.owner_id.as_str());
    let sender = p.owner_id.clone();
    let res = if own {
        domain::create_message(app, NewMessage { project_id: &p.id, sender_id: &sender, sender_label: "", client_name: "Gliph", conversation_id: None, kind: "task", text, idem_key: "", attachment_ids: &[], deploy: None }).await
    } else {
        guest_message(app, &p, &label, text).await
    };
    if let Err(err) = res {
        app.gliph(GliphOut::Say { channel: channel.into(), body: format!("[{}] not sent: {}", p.handle, err.1), agent: None });
    }
    Ok(())
}

/// A message from a channel guest: recorded under the developer's account (the
/// channel is theirs) but never auto-approved by that, and labelled honestly.
async fn guest_message(app: &App, p: &Project, label: &str, text: &str) -> Result<domain::Message, crate::app::Fail> {
    let m = domain::create_message(app, NewMessage {
        project_id: &p.id, sender_id: &p.owner_id, sender_label: label, client_name: "Gliph channel",
        conversation_id: None, kind: "task", text, idem_key: "", attachment_ids: &[], deploy: None,
    }).await?;
    // create_message auto-approves the owner's own messages; a guest's is not
    // the owner's, so put it back in front of the developer unless auto relay is on.
    if m.stage == "approved" && !p.auto_active() {
        sqlx::query("UPDATE messages SET stage='pending_approval' WHERE id=$1 AND stage='approved'").bind(&m.id).execute(&app.db).await?;
        domain::add_event(app, &m.conversation_id, &m.id, "system", "Substrate", "stage",
            "From a Gliph channel member: waiting for the developer's approval.", json!({"message_id": m.id, "stage": "pending_approval"}), false).await?;
        crate::push::approval_waiting(app.db.clone(), p.owner_id.clone(), m.id.clone(), p.name.clone(), label.to_string(), text.to_string(), false);
        return domain::message(app, &m.id).await;
    }
    Ok(m)
}
