//! Sessions (shown as such; the code calls them links): one URL per consumer, scoped to the panes (projects) and the
//! permissions the producer picked. The URL is itself an MCP server
//! (`/mcp/<secret>`), so a consumer pastes it into Claude or ChatGPT as a
//! custom connector, with no account and no sign-in.
//!
//! Each link has its own hidden account with tester membership in the chosen
//! projects, so approval, conversations, the status block and the audit log
//! all work as for a signed-in tester. `Perms` narrows what the MCP tools
//! return on top of that.

use std::collections::HashMap;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use chrono::{DateTime, Duration, Utc};
use maud::html;
use serde_json::Value;

use crate::app::{audit, hash, id, now, token, App, AppInner, Fail};
use crate::auth::{check_csrf, User};
use crate::domain::Project;
use crate::web::{csrf, ic, layout, picon};

/// What a link may do beyond sending messages.
#[derive(Clone, Copy, Debug)]
pub struct Perms {
    /// See the agent's replies and questions, and answer them.
    pub read: bool,
    /// See CI and deploy progress and the retest links.
    pub deploys: bool,
    /// Attach files, and download what the agent attached.
    pub files: bool,
}

/// The link behind a `/mcp/<secret>` URL: (account, label, perms), and a
/// last-used stamp. None when the secret is unknown, expired or revoked.
pub async fn resolve(app: &AppInner, secret: &str) -> Option<(String, String, Perms)> {
    let row: Option<(String, String, String, bool, bool, bool)> = sqlx::query_as(
        "SELECT l.id, l.account_id, l.label, l.can_read, l.can_deploys, l.can_files FROM tokens t
         JOIN access_links l ON l.id=t.ref
         WHERE t.hash=$1 AND t.kind='link' AND t.expires_at > now() AND NOT l.revoked AND l.expires_at > now()",
    ).bind(hash(secret)).fetch_optional(&app.db).await.ok().flatten();
    let (lid, account, label, read, deploys, files) = row?;
    let _ = sqlx::query("UPDATE access_links SET last_used_at=now() WHERE id=$1 AND (last_used_at IS NULL OR last_used_at < now() - interval '1 minute')")
        .bind(&lid).execute(&app.db).await;
    Some((account, label, Perms { read, deploys, files }))
}

/// Drop what a link may not see from a status block.
pub fn restrict(v: &mut Value, p: Perms) {
    if !p.read {
        if let Some(o) = v.as_object_mut() {
            o.remove("question");
        }
        if let Some(evs) = v.get_mut("new_events").and_then(|e| e.as_array_mut()) {
            evs.retain(|e| e.get("agent_text").is_none());
        }
    }
    if !p.deploys {
        if let Some(o) = v.as_object_mut() {
            o.insert("deployment_stage".into(), Value::Null);
        }
        if let Some(l) = v.get_mut("links").and_then(|l| l.as_object_mut()) {
            l.retain(|k, _| k == "conversation_url");
        }
        if let Some(evs) = v.get_mut("new_events").and_then(|e| e.as_array_mut()) {
            evs.retain(|e| e.get("kind").and_then(|k| k.as_str()) != Some("deploy"));
        }
    }
    if !p.files {
        if let Some(evs) = v.get_mut("new_events").and_then(|e| e.as_array_mut()) {
            for e in evs.iter_mut().filter_map(|e| e.as_object_mut()) {
                e.remove("attachments");
            }
        }
    }
    if let Some(latest) = v.get_mut("latest") {
        restrict(latest, p);
    }
}

/// Make a link: its hidden account, tester membership in each project, and
/// the secret. Returns (link id, secret). Every project must be the owner's.
#[allow(clippy::too_many_arguments)]
pub async fn create(app: &AppInner, owner: &str, label: &str, projects: &[String], perms: Perms, auto: bool, deploy: (bool, bool), days: i64) -> Result<(String, String), Fail> {
    let label = substrate_proto::sanitize(label.trim(), 64);
    if label.is_empty() {
        return Err(Fail::new(400, "Give the link a name."));
    }
    if projects.is_empty() {
        return Err(Fail::new(400, "Pick at least one project."));
    }
    let mut tx = app.db.begin().await?;
    for pid in projects {
        let ok: Option<String> = sqlx::query_scalar("SELECT id FROM projects WHERE id=$1 AND owner_id=$2").bind(pid).bind(owner).fetch_optional(&mut *tx).await?;
        if ok.is_none() {
            return Err(Fail::new(404, "no such project"));
        }
    }
    let lid = id("l");
    let acc = id("a");
    let exp = now() + Duration::days(days.clamp(1, 365));
    sqlx::query("INSERT INTO accounts (id, email, name) VALUES ($1,$2,$3)")
        .bind(&acc).bind(format!("{lid}@links.substrate.invalid")).bind(&label).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO access_links (id, owner_id, account_id, label, can_read, can_deploys, can_files, auto, expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
        .bind(&lid).bind(owner).bind(&acc).bind(&label).bind(perms.read).bind(perms.deploys).bind(perms.files).bind(auto).bind(exp)
        .execute(&mut *tx).await?;
    for pid in projects {
        sqlx::query("INSERT INTO members (project_id, account_id, role, trusted_auto, invite_id, can_deploy_test, can_deploy_prod) VALUES ($1,$2,'tester',$3,$4,$5,$6)")
            .bind(pid).bind(&acc).bind(auto).bind(format!("link:{lid}")).bind(deploy.0).bind(deploy.1).execute(&mut *tx).await?;
    }
    let secret = token();
    sqlx::query("INSERT INTO tokens (hash, kind, account_id, ref, expires_at) VALUES ($1,'link',$2,$3,$4)")
        .bind(hash(&secret)).bind(&acc).bind(&lid).bind(exp).execute(&mut *tx).await?;
    tx.commit().await?;
    audit(app, owner, "", "", "link.created", &lid, &label).await;
    Ok((lid, secret))
}

/// Revoke a link: the secret stops working and its memberships go. Its
/// conversations stay for the producer's history.
pub async fn revoke(app: &AppInner, owner: &str, lid: &str) -> Result<(), Fail> {
    let acc: Option<String> = sqlx::query_scalar("UPDATE access_links SET revoked=true WHERE id=$1 AND owner_id=$2 RETURNING account_id")
        .bind(lid).bind(owner).fetch_optional(&app.db).await?;
    let Some(acc) = acc else { return Err(Fail::new(404, "no such link")) };
    sqlx::query("DELETE FROM tokens WHERE kind='link' AND ref=$1").bind(lid).execute(&app.db).await?;
    sqlx::query("DELETE FROM members WHERE account_id=$1").bind(&acc).execute(&app.db).await?;
    audit(app, owner, "", "", "link.revoked", lid, "").await;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct LinkRow {
    account_id: String,
    waiting: i64,
    deploy_test: bool,
    deploy_prod: bool,
    id: String,
    label: String,
    can_read: bool,
    can_deploys: bool,
    can_files: bool,
    auto: bool,
    expires_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    projects: Vec<String>,
}

pub async fn page(State(app): State<App>, User(a): User, axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>) -> Result<Response, Fail> {
    let links = sqlx::query_as::<_, LinkRow>(
        "SELECT l.id, l.account_id, l.label, l.can_read, l.can_deploys, l.can_files, l.auto, l.expires_at, l.last_used_at,
                (SELECT count(*) FROM messages mm WHERE mm.sender_id=l.account_id AND mm.stage IN ('pending_approval','held','reviewing')) AS waiting,
                COALESCE(bool_or(m.can_deploy_test), false) AS deploy_test, COALESCE(bool_or(m.can_deploy_prod), false) AS deploy_prod,
                COALESCE(array_agg(p.name ORDER BY p.name) FILTER (WHERE p.id IS NOT NULL), '{}') AS projects
         FROM access_links l LEFT JOIN members m ON m.account_id=l.account_id LEFT JOIN projects p ON p.id=m.project_id
         WHERE l.owner_id=$1 AND NOT l.revoked AND l.expires_at > now() GROUP BY l.id ORDER BY l.created_at DESC",
    ).bind(&a.id).fetch_all(&app.db).await?;
    Ok(Html(layout("Sessions", Some(&a), None, html! {
        div.stack style="padding:12px 0 48px" {
            @if let Some(msg) = q.get("flash") { (crate::web::notice("ok", html! { (msg) })) }
            div.between { h1 { span.grad { "Sessions" } } a.btn.primary href="/sessions/new" { (ic("link")) "New session" } }
            p.lead { "Give each person one link. They open it in a browser to chat, or add it to Claude or ChatGPT as a connector." }
            @if links.is_empty() {
                div.card { h3 { "No links yet" } p.muted { "Create a link for each tester, with the projects and permissions they get." } }
            } @else {
                div.list {
                    @for l in &links {
                        // The whole row opens the link's page: history, approvals, Revoke.
                        a.item.link-row href=(format!("/sessions/{}", l.id)) {
                            span.l.row style="align-items:flex-start;gap:12px" {
                                img.avatar-img src=(format!("/accounts/{}/avatar", l.account_id)) alt="" style="width:32px;height:32px";
                                span.stack style="gap:4px;align-items:flex-start" {
                                    span.row { strong style="font-weight:500" { (l.label) } @if l.waiting > 0 { span.tag.warn { (l.waiting) " waiting for you" } } }
                                    span.muted style="font-size:13px" { (l.projects.join(", ")) }
                                    span.row style="gap:6px;flex-wrap:wrap" {
                                        @if l.can_read { span.tag { "replies" } }
                                        @if l.can_deploys { span.tag { "deploys" } }
                                        @if l.can_files { span.tag { "files" } }
                                        @if l.deploy_test { span.tag.blue { "deploys to test" } }
                                        @if l.deploy_prod { span.tag.warn { "deploys to production" } }
                                        @if l.auto { span.tag.warn { "no approval" } }
                                    }
                                }
                            }
                            span.r.row {
                                span.faint style="font-size:13px" {
                                    @match l.last_used_at { Some(t) => { "used " (crate::web::ago_text(&t)) }, None => { "not used yet" } }
                                    ", expires " (l.expires_at.format("%b %d"))
                                }
                                span.chev { (ic("arrow-right")) }
                            }
                        }
                    }
                }
            }
        }
    }).into_string()).into_response())
}

/// The permission checkboxes, in the order read, deploys, files, auto, deploy to test, deploy to production.
const PERM_KEYS: [&str; 6] = ["read", "deploys", "files", "auto", "deploy_test", "deploy_prod"];

/// `mixed` marks a box whose value differs between the session's projects.
fn perm_fields(on: [bool; 6], mixed: [bool; 6]) -> maud::Markup {
    let rows = [
        ("read", "Read the agent's replies and questions"),
        ("deploys", "See CI and deploy progress and retest links"),
        ("files", "Attach files and download the agent's files"),
        ("auto", "Skip my approval (flagged messages still wait)"),
        ("deploy_test", "Ask the agent to deploy to test"),
        ("deploy_prod", "Ask the agent to deploy to production"),
    ];
    html! {
        div.field {
            label { "Permissions" }
            label.row style="font-weight:400" { input type="checkbox" checked disabled; "Send messages and answers" }
            @for (((name, text), on), mixed) in rows.iter().zip(on).zip(mixed) {
                @if mixed {
                    label.row style="font-weight:400;flex-wrap:wrap" { (text) select name=(format!("mix_{name}")) style="width:auto" { option value="" selected { "as each project has it" } option value="on" { "on in every project" } option value="off" { "off in every project" } } }
                } @else {
                    label.row style="font-weight:400" { input type="checkbox" name=(name) checked[on]; (text) }
                }
            }
        }
    }
}

pub async fn new_page(State(app): State<App>, User(a): User, axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>) -> Result<Response, Fail> {
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1 ORDER BY name").bind(&a.id).fetch_all(&app.db).await?;
    Ok(Html(layout("New session", Some(&a), None, html! {
        div.stack style="padding:12px 0 48px" {
            @if let Some(msg) = q.get("flash") { (crate::web::notice("ok", html! { (msg) })) }
            div.crumbs { a href="/sessions" style="text-decoration:none" { "Sessions" } span.sep { "/" } span.muted { "New session" } }
            h1 { "New session" }
            @if projects.is_empty() {
                div.card { h3 { "Add a project first" } p.muted { "A link gives access to projects. Install the relay: each agent in Herdr gets a project." } }
            } @else {
                form.card.stack method="post" action="/sessions" {
                    (csrf(&a))
                    div.field { label for="ll" { "Person or team" } input id="ll" type="text" name="label" required maxlength="64" placeholder="Sarah, or Design team"; }
                    div.field {
                        label { "Projects" }
                        @for p in &projects {
                            label.row style="font-weight:400" { input type="checkbox" name=(format!("p_{}", p.id)); (picon(&p.id, &p.name, &p.icon_etag, 22)) (p.name) span.muted.mono { (p.handle) } }
                        }
                    }
                    (perm_fields([true, true, true, false, false, false], [false; 6]))
                    div.field { label for="ld" { "Expires" } select id="ld" name="days" { option value="7" { "in 7 days" } option value="30" selected { "in 30 days" } option value="90" { "in 90 days" } option value="365" { "in a year" } } }
                    div.row { button.btn.primary type="submit" { (ic("link")) "Create link" } a.btn.tertiary href="/sessions" { "Cancel" } }
                }
            }
        }
    }).into_string()).into_response())
}

/// GET /sessions/{lid}: one session: what it reaches and may do, what waits for
/// approval, and everything its holder sent and got back.
pub async fn detail(State(app): State<App>, User(a): User, Path(lid): Path<String>, axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>) -> Result<Response, Fail> {
    let l = sqlx::query_as::<_, LinkRow>(
        "SELECT l.id, l.account_id, l.label, l.can_read, l.can_deploys, l.can_files, l.auto, l.expires_at, l.last_used_at,
                (SELECT count(*) FROM messages mm WHERE mm.sender_id=l.account_id AND mm.stage IN ('pending_approval','held','reviewing')) AS waiting,
                COALESCE(bool_or(m.can_deploy_test), false) AS deploy_test, COALESCE(bool_or(m.can_deploy_prod), false) AS deploy_prod,
                COALESCE(array_agg(p.name ORDER BY p.name) FILTER (WHERE p.id IS NOT NULL), '{}') AS projects
         FROM access_links l LEFT JOIN members m ON m.account_id=l.account_id LEFT JOIN projects p ON p.id=m.project_id
         WHERE l.id=$1 AND l.owner_id=$2 GROUP BY l.id",
    ).bind(&lid).bind(&a.id).fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "no such link"))?;
    let (acct, revoked): (String, bool) = sqlx::query_as("SELECT account_id, revoked FROM access_links WHERE id=$1").bind(&lid).fetch_one(&app.db).await?;
    let member_of: Vec<String> = sqlx::query_scalar("SELECT project_id FROM members WHERE account_id=$1").bind(&acct).fetch_all(&app.db).await?;
    let all: (bool, bool, bool) = sqlx::query_as("SELECT COALESCE(bool_and(trusted_auto), false), COALESCE(bool_and(can_deploy_test), false), COALESCE(bool_and(can_deploy_prod), false) FROM members WHERE account_id=$1")
        .bind(&acct).fetch_one(&app.db).await?;
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1").bind(&a.id).fetch_all(&app.db).await?;
    let waiting = sqlx::query_as::<_, crate::domain::Message>("SELECT * FROM messages WHERE sender_id=$1 AND stage IN ('pending_approval','held','reviewing') ORDER BY order_n")
        .bind(&acct).fetch_all(&app.db).await?;
    let convs: Vec<(String, String, String)> = sqlx::query_as("SELECT id, project_id, title FROM conversations WHERE sender_id=$1 ORDER BY updated_at DESC LIMIT 20")
        .bind(&acct).fetch_all(&app.db).await?;
    let mut history = vec![];
    for (cid, pid, title) in &convs {
        let Some(p) = projects.iter().find(|p| &p.id == pid) else { continue };
        let events = sqlx::query_as::<_, crate::domain::Event>("SELECT * FROM events WHERE conversation_id=$1 ORDER BY seq").bind(cid).fetch_all(&app.db).await?;
        let msgs = sqlx::query_as::<_, crate::domain::Message>("SELECT * FROM messages WHERE conversation_id=$1 ORDER BY order_n").bind(cid).fetch_all(&app.db).await?;
        let atts = sqlx::query_as::<_, crate::domain::Attachment>("SELECT * FROM attachments WHERE conversation_id=$1").bind(cid).fetch_all(&app.db).await?;
        history.push((cid.clone(), p, title.clone(), events, msgs, atts));
    }
    let host = app.cfg.host();
    let live = !revoked && l.expires_at > now();
    let (sent, active, done, shipped): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE kind <> 'answer'),
                count(*) FILTER (WHERE stage IN ('approved','dispatched','accepted','submitted','working','awaiting_sender')),
                count(*) FILTER (WHERE stage = 'completed'),
                count(*) FILTER (WHERE deploy_stage IN ('deployed','verified','ready_to_retest'))
         FROM messages WHERE sender_id=$1",
    ).bind(&acct).fetch_one(&app.db).await?;
    Ok(Html(layout("Sessions", Some(&a), Some("/live".into()), html! {
        div.stack style="padding:12px 0 48px" {
            @if let Some(msg) = q.get("flash") { (crate::web::notice("ok", html! { (msg) })) }
            div.crumbs { a href="/sessions" style="text-decoration:none" { "Sessions" } span.sep { "/" } span.muted { (l.label) } }
            div.between {
                div.row style="gap:14px" { img.avatar-img src=(format!("/accounts/{acct}/avatar")) alt="" style="width:44px;height:44px"; h1 style="margin:0" { (l.label) } }
                @if live {
                    form.inline method="post" action=(format!("/sessions/{}/revoke", l.id)) data-confirm="Revoke this link? It stops working at once." data-confirm-ok="Revoke" {
                        (csrf(&a)) button.btn.danger type="submit" { (ic("trash")) "Revoke" }
                    }
                } @else { span.tag.off { @if revoked { "revoked" } @else { "expired" } } }
            }
            div.card.stack style="gap:10px" {
                div.row style="flex-wrap:wrap" { @for name in &l.projects { @if let Some(p) = projects.iter().find(|p| &p.name == name) { span.row style="gap:6px" { (picon(&p.id, &p.name, &p.icon_etag, 20)) (p.name) } } } }
                div.row style="gap:6px;flex-wrap:wrap" {
                    @if l.can_read { span.tag { "replies" } }
                    @if l.can_deploys { span.tag { "deploys" } }
                    @if l.can_files { span.tag { "files" } }
                    @if l.deploy_test { span.tag.blue { "deploys to test" } }
                    @if l.deploy_prod { span.tag.warn { "deploys to production" } }
                    @if l.auto { span.tag.warn { "no approval" } }
                }
                span.faint style="font-size:13px" { @match l.last_used_at { Some(t) => { "Used " (crate::web::ago_text(&t)) }, None => { "Not used yet" } } ", expires " (l.expires_at.format("%b %d")) }
            }
            @if live {
                // A disclosure: app.js holds live refreshes while it is open, so unsaved ticks survive.
                details.edit.card id="permissions" open[q.contains_key("edit")] {
                    summary { "Change projects and permissions" }
                    form.stack method="post" action=(format!("/sessions/{}/permissions", l.id)) style="margin-top:12px" {
                        (csrf(&a))
                        div.field {
                            label { "Projects" }
                            @if let Some(msg) = q.get("error") { (crate::web::notice("warn", html! { (msg) })) }
                            @for p in &projects {
                                @let had = member_of.contains(&p.id);
                                label.row style="font-weight:400" { input type="checkbox" name=(format!("p_{}", p.id)) checked[had]; (picon(&p.id, &p.name, &p.icon_etag, 22)) (p.name) span.muted.mono { (p.handle) } }
                            }
                        }
                        @let was = [l.can_read, l.can_deploys, l.can_files, l.auto, l.deploy_test, l.deploy_prod];
                        @let mixed = [false, false, false, l.auto && !all.0, l.deploy_test && !all.1, l.deploy_prod && !all.2];
                        // A box that differs between projects is a three-way choice instead; its default keeps each project's value.
                        @for ((k, v), m) in PERM_KEYS.iter().zip(was).zip(mixed) { input type="hidden" name=(format!("was_{k}")) value=(if m { "mixed" } else if v { "1" } else { "0" }); }
                        (perm_fields(was, mixed))
                        span.faint style="font-size:13px" { "Only what you change is saved, in every project of this session. An added project gets the permissions above; one set to \"as each project has it\" starts off there. Trust and Test or Prod on a project page set one project." }
                        div.row { button.btn.primary type="submit" { (ic("check")) "Save" } }
                    }
                }
            }
            div.grid.tight {
                div.card.stat { span.k { (ic("message")) "Messages" } span.big { (sent) } }
                div.card.stat { span.k { (ic("clock")) "Waiting for you" } span.big style=(if l.waiting > 0 { "color:var(--amber)" } else { "" }) { (l.waiting) } }
                div.card.stat { span.k { (ic("zap")) "With the agent" } span.big { (active) } }
                div.card.stat { span.k { (ic("check")) "Completed" } span.big.grad { (done) } }
                div.card.stat { span.k { (ic("rocket")) "Deployed" } span.big { (shipped) } }
            }
            @if !waiting.is_empty() {
                div.card.glow.stack id="waiting" {
                    div.row style="font-size:19px;font-weight:500" { "Waiting for you " span.badge { (waiting.len()) } }
                    (crate::web::approvals_section(&a, &waiting, &projects, &format!("/sessions/{lid}#waiting"), false))
                }
            }
            h3 style="margin:12px 0 0" { "History" }
            @if history.is_empty() { p.muted { "Nothing sent yet." } }
            @for (cid, p, title, events, msgs, atts) in &history {
                section.stack {
                    div.row { (picon(&p.id, &p.name, &p.icon_etag, 20)) a href=(format!("/conversations/{cid}")) style="text-decoration:none;font-weight:500" { (title) } span.faint style="font-size:13px" { (p.name) } }
                    @if let Some(m) = msgs.iter().rev().find(|m| !m.deploy_stage.is_empty()) { (crate::web::deploy_track(&m.deploy_stage)) }
                    div.stack.timeline { @for e in events.iter().rev() { (crate::web::event_card(&app, &a, p, msgs, atts, e, true, &host)) } }
                }
            }
        }
    }).into_string()).into_response())
}

/// Shown once, right after the link is made; the secret never goes in a URL.
fn created(app: &AppInner, a: &crate::auth::Account, label: &str, secret: &str) -> Response {
    let u = format!("{}/mcp/{secret}", app.cfg.public_url);
    Html(layout("Session created", Some(a), None, html! {
        div.stack style="padding:12px 0 48px" {
            div.crumbs { a href="/sessions" style="text-decoration:none" { "Sessions" } span.sep { "/" } span.muted { (label) } }
            div.card.glow {
                h3 { "Send this link to " (label) }
                div.secret { (u) }
                div.row { button.btn.primary type="button" data-copy=(u) { (ic("copy")) "Copy link" } span.faint style="font-size:13px" { "You see it only once." } }
                div.stack style="gap:8px;font-size:14px" {
                    div.row.muted { (ic("link")) span { strong style="color:var(--text)" { "Browser" } ": Open the link for a chat with your agents." } }
                    div.row.muted { img.logo src="/static/logos/claude.svg" alt="Claude" width="18" height="18"; span { strong style="color:var(--text)" { "Claude" } ": Settings › Connectors › Add custom connector. Paste the link, leave authentication off." } }
                    div.row.muted { img.logo src="/static/logos/openai.svg" alt="ChatGPT" width="18" height="18"; span { strong style="color:var(--text)" { "ChatGPT" } ": Settings › Apps › Advanced settings › Developer mode. Create an app with the link and no authentication." } }
                }
            }
            div.row { a.btn href="/sessions" { (ic("arrow-left")) "All sessions" } }
        }
    }).into_string()).into_response()
}

pub async fn create_post(State(app): State<App>, headers: HeaderMap, User(a): User, Form(f): Form<HashMap<String, String>>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, f.get("csrf").map(String::as_str).unwrap_or(""))?;
    let projects: Vec<String> = f.keys().filter_map(|k| k.strip_prefix("p_")).map(str::to_string).collect();
    let on = |k: &str| f.contains_key(k);
    let days = f.get("days").and_then(|d| d.parse().ok()).unwrap_or(30);
    let label = f.get("label").cloned().unwrap_or_default();
    match create(&app, &a.id, &label, &projects, Perms { read: on("read"), deploys: on("deploys"), files: on("files") }, on("auto"), (on("deploy_test"), on("deploy_prod")), days).await {
        Ok((_, secret)) => Ok(created(&app, &a, label.trim(), &secret)),
        Err(Fail(_, msg)) => Ok(Redirect::to(&format!("/sessions/new?flash={}", urlencoding::encode(&msg))).into_response()),
    }
}

/// Change a live link: each (permission, on) in `changed` in every project, and,
/// with `projects`, the projects it reaches. A permission not in `changed` keeps
/// each project's value; an added project gets it where all projects agree, off
/// where they differ. Shared by the web form and the app API. Ok(false): nothing changed.
pub async fn update(app: &AppInner, owner: &str, lid: &str, changed: &[(&str, bool)], projects: Option<&[String]>) -> Result<bool, Fail> {
    let mut tx = app.db.begin().await?;
    let acc: Option<String> = sqlx::query_scalar("SELECT account_id FROM access_links WHERE id=$1 AND owner_id=$2 AND NOT revoked AND expires_at > now() FOR UPDATE").bind(lid).bind(owner).fetch_optional(&mut *tx).await?;
    let Some(acc) = acc else { return Err(Fail::new(404, "no such session")) };
    let current: Vec<String> = sqlx::query_scalar("SELECT project_id FROM members WHERE account_id=$1").bind(&acc).fetch_all(&mut *tx).await?;
    let (added, removed): (Vec<&String>, Vec<&String>) = match projects {
        Some([]) => return Err(Fail::new(400, "Keep at least one project, or revoke the session.")),
        Some(t) => (t.iter().filter(|p| !current.contains(p)).collect(), current.iter().filter(|p| !t.contains(p)).collect()),
        None => (vec![], vec![]),
    };
    if changed.is_empty() && added.is_empty() && removed.is_empty() {
        return Ok(false);
    }
    let agreed: (bool, bool, bool) = sqlx::query_as("SELECT COALESCE(bool_and(trusted_auto), false), COALESCE(bool_and(can_deploy_test), false), COALESCE(bool_and(can_deploy_prod), false) FROM members WHERE account_id=$1")
        .bind(&acc).fetch_one(&mut *tx).await?;
    let pick = |k: &str, dflt: bool| changed.iter().find(|(c, _)| *c == k).map(|(_, on)| *on).unwrap_or(dflt);
    for pid in &added {
        let ok: Option<String> = sqlx::query_scalar("SELECT id FROM projects WHERE id=$1 AND owner_id=$2").bind(pid).bind(owner).fetch_optional(&mut *tx).await?;
        if ok.is_none() {
            return Err(Fail::new(404, "no such project"));
        }
        sqlx::query("INSERT INTO members (project_id, account_id, role, trusted_auto, invite_id, can_deploy_test, can_deploy_prod) VALUES ($1,$2,'tester',$3,$4,$5,$6) ON CONFLICT DO NOTHING")
            .bind(pid).bind(&acc).bind(pick("auto", agreed.0)).bind(format!("link:{lid}")).bind(pick("deploy_test", agreed.1)).bind(pick("deploy_prod", agreed.2)).execute(&mut *tx).await?;
    }
    for pid in &removed {
        sqlx::query("DELETE FROM members WHERE project_id=$1 AND account_id=$2 AND role='tester'").bind(pid).bind(&acc).execute(&mut *tx).await?;
    }
    for (k, on) in changed {
        // Column names come from PERM_KEYS, never from the caller.
        let (link_col, member_col) = match *k { "read" => (Some("can_read"), None), "deploys" => (Some("can_deploys"), None), "files" => (Some("can_files"), None),
            "auto" => (Some("auto"), Some("trusted_auto")), "deploy_test" => (None, Some("can_deploy_test")), "deploy_prod" => (None, Some("can_deploy_prod")), _ => continue };
        if let Some(c) = link_col { sqlx::query(&format!("UPDATE access_links SET {c}=$2 WHERE id=$1")).bind(lid).bind(on).execute(&mut *tx).await?; }
        if let Some(c) = member_col { sqlx::query(&format!("UPDATE members SET {c}=$2 WHERE account_id=$1 AND role='tester'")).bind(&acc).bind(on).execute(&mut *tx).await?; }
    }
    // "No approval" on the link mirrors its projects, as after Trust on a project page.
    sqlx::query("UPDATE access_links SET auto=COALESCE((SELECT bool_or(trusted_auto) FROM members WHERE account_id=$1), false) WHERE account_id=$1").bind(&acc).execute(&mut *tx).await?;
    tx.commit().await?;
    let mut what: Vec<String> = changed.iter().map(|(k, on)| format!("{k}={on}")).collect();
    what.extend(added.iter().map(|p| format!("+{p}")).chain(removed.iter().map(|p| format!("-{p}"))));
    audit(app, owner, "", "", "link.updated", lid, &what.join(",")).await;
    app.changed(owner);
    Ok(true)
}

/// POST /sessions/{lid}/permissions: change what a live link may do, in all its projects.
pub async fn permissions_post(State(app): State<App>, headers: HeaderMap, User(a): User, Path(lid): Path<String>, Form(f): Form<HashMap<String, String>>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, f.get("csrf").map(String::as_str).unwrap_or(""))?;
    // Only the boxes the owner changed: an unchanged box keeps each project's own Trust, Test and Prod.
    let changed: Vec<(&str, bool)> = PERM_KEYS.iter().filter_map(|k| {
        let on = f.contains_key(*k);
        match f.get(&format!("was_{k}")).map(String::as_str) {
            Some("mixed") => match f.get(&format!("mix_{k}")).map(String::as_str) { Some("on") => Some((*k, true)), Some("off") => Some((*k, false)), _ => None },
            Some(w) if (w == "1") == on => None,
            _ => Some((*k, on)),
        }
    }).collect();
    let target: Vec<String> = f.keys().filter_map(|k| k.strip_prefix("p_")).map(str::to_string).collect();
    match update(&app, &a.id, &lid, &changed, Some(&target)).await {
        Ok(false) => return Ok(Redirect::to(&format!("/sessions/{lid}?flash=Nothing+changed.")).into_response()),
        Err(Fail(c, msg)) if c == axum::http::StatusCode::BAD_REQUEST => return Ok(Redirect::to(&format!("/sessions/{lid}?edit=1&error={}#permissions", urlencoding::encode(&msg))).into_response()),
        Err(e) => return Err(e),
        Ok(true) => {}
    }
    Ok(Redirect::to(&format!("/sessions/{lid}?flash=Saved.")).into_response())
}

pub async fn revoke_post(State(app): State<App>, headers: HeaderMap, User(a): User, Path(lid): Path<String>, Form(f): Form<HashMap<String, String>>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, f.get("csrf").map(String::as_str).unwrap_or(""))?;
    revoke(&app, &a.id, &lid).await?;
    Ok(Redirect::to("/sessions?flash=Link+revoked.").into_response())
}
