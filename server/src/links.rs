//! Access links: one URL per consumer, scoped to the panes (projects) and the
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
    if let Some(latest) = v.get_mut("latest") {
        restrict(latest, p);
    }
}

/// Make a link: its hidden account, tester membership in each project, and
/// the secret. Returns (link id, secret). Every project must be the owner's.
#[allow(clippy::too_many_arguments)]
pub async fn create(app: &AppInner, owner: &str, label: &str, projects: &[String], perms: Perms, auto: bool, days: i64) -> Result<(String, String), Fail> {
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
        sqlx::query("INSERT INTO members (project_id, account_id, role, trusted_auto, invite_id) VALUES ($1,$2,'tester',$3,$4)")
            .bind(pid).bind(&acc).bind(auto).bind(format!("link:{lid}")).execute(&mut *tx).await?;
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
    render(&app, &a, q.get("flash").map(String::as_str), None).await
}

/// The page; `new` is (label, secret) right after a link was made, shown once
/// and never put in a URL.
async fn render(app: &AppInner, a: &crate::auth::Account, flash: Option<&str>, new: Option<(&str, &str)>) -> Result<Response, Fail> {
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1 ORDER BY name").bind(&a.id).fetch_all(&app.db).await?;
    let links = sqlx::query_as::<_, LinkRow>(
        "SELECT l.id, l.label, l.can_read, l.can_deploys, l.can_files, l.auto, l.expires_at, l.last_used_at,
                COALESCE(array_agg(p.name ORDER BY p.name) FILTER (WHERE p.id IS NOT NULL), '{}') AS projects
         FROM access_links l LEFT JOIN members m ON m.account_id=l.account_id LEFT JOIN projects p ON p.id=m.project_id
         WHERE l.owner_id=$1 AND NOT l.revoked AND l.expires_at > now() GROUP BY l.id ORDER BY l.created_at DESC",
    ).bind(&a.id).fetch_all(&app.db).await?;
    let new_url = new.map(|(_, s)| format!("{}/mcp/{s}", app.cfg.public_url));
    Ok(Html(layout("Access links", Some(a), None, html! {
        div.narrow.stack style="padding:16px 0 48px" {
            @if let Some(msg) = flash { div.flash role="status" { (msg) } }
            h1 { "Access " span.grad { "links" } }
            p.lead { "Give each person one link. They open it in a browser to chat, or add it to Claude or ChatGPT as a connector." }
            @if let Some(u) = &new_url {
                div.card.glow {
                    h3 { "Send this link to " (new.map(|(l, _)| l).unwrap_or("your tester")) }
                    div.secret { (u) }
                    div.row { button.btn.primary type="button" data-copy=(u) { (ic("copy")) "Copy link" } span.faint style="font-size:13px" { "You see it only once." } }
                    p.muted style="margin:0;font-size:14px" { "Opened in a browser, the link is a chat with your agents." }
                    p.muted style="margin:0;font-size:14px" {
                        "In Claude: Settings › Connectors › Add custom connector, paste the link, leave authentication off. "
                        "In ChatGPT: Settings › Apps › Advanced settings › Developer mode, then create an app with the link and no authentication."
                    }
                }
            }
            @if projects.is_empty() {
                div.card { h3 { "Add a project first" } p.muted { "A link gives access to projects. Bind a herdr pane with the relay, then come back." } }
            } @else {
                form.card.stack method="post" action="/links" {
                    (csrf(a))
                    h3 { "New link" }
                    div.field { label for="ll" { "Person or team" } input id="ll" type="text" name="label" required maxlength="64" placeholder="Sarah, or Design team"; }
                    div.field {
                        label { "Panes" }
                        @for p in &projects {
                            label.row style="font-weight:400" { input type="checkbox" name=(format!("p_{}", p.id)); (picon(&p.id, &p.name, &p.icon_etag, 22)) (p.name) span.muted.mono { (p.handle) } }
                        }
                    }
                    div.field {
                        label { "Permissions" }
                        label.row style="font-weight:400" { input type="checkbox" checked disabled; "Send messages and answers" }
                        label.row style="font-weight:400" { input type="checkbox" name="read" checked; "Read the agent's replies and questions" }
                        label.row style="font-weight:400" { input type="checkbox" name="deploys" checked; "See CI and deploy progress and retest links" }
                        label.row style="font-weight:400" { input type="checkbox" name="files" checked; "Attach files and download the agent's files" }
                        label.row style="font-weight:400" { input type="checkbox" name="auto"; "Skip my approval (flagged messages still wait)" }
                    }
                    div.field { label for="ld" { "Expires" } select id="ld" name="days" { option value="7" { "in 7 days" } option value="30" selected { "in 30 days" } option value="90" { "in 90 days" } option value="365" { "in a year" } } }
                    div.row { button.btn.primary type="submit" { (ic("link")) "Create link" } }
                }
            }
            @if !links.is_empty() {
                div.list {
                    div.label { "Active links" }
                    @for l in &links {
                        div.item {
                            span.l.stack style="gap:4px;align-items:flex-start" {
                                strong style="font-weight:500" { (l.label) }
                                span.muted style="font-size:13px" { (l.projects.join(", ")) }
                                span.row style="gap:6px" {
                                    @if l.can_read { span.tag { "replies" } }
                                    @if l.can_deploys { span.tag { "deploys" } }
                                    @if l.can_files { span.tag { "files" } }
                                    @if l.auto { span.tag.warn { "no approval" } }
                                }
                            }
                            span.r.row {
                                span.faint style="font-size:13px" {
                                    @match l.last_used_at { Some(t) => { "used " (crate::web::ago(&t)) }, None => { "not used yet" } }
                                    " · expires " (l.expires_at.format("%b %d"))
                                }
                                form.inline method="post" action=(format!("/links/{}/revoke", l.id)) data-confirm="Revoke this link? It stops working at once." {
                                    (csrf(a)) button.btn.small.danger type="submit" { (ic("trash")) "Revoke" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }).into_string()).into_response())
}

pub async fn create_post(State(app): State<App>, headers: HeaderMap, User(a): User, Form(f): Form<HashMap<String, String>>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, f.get("csrf").map(String::as_str).unwrap_or(""))?;
    let projects: Vec<String> = f.keys().filter_map(|k| k.strip_prefix("p_")).map(str::to_string).collect();
    let on = |k: &str| f.contains_key(k);
    let days = f.get("days").and_then(|d| d.parse().ok()).unwrap_or(30);
    let label = f.get("label").cloned().unwrap_or_default();
    match create(&app, &a.id, &label, &projects, Perms { read: on("read"), deploys: on("deploys"), files: on("files") }, on("auto"), days).await {
        Ok((_, secret)) => render(&app, &a, None, Some((label.trim(), &secret))).await,
        Err(Fail(_, msg)) => Ok(Redirect::to(&format!("/links?flash={}", urlencoding::encode(&msg))).into_response()),
    }
}

pub async fn revoke_post(State(app): State<App>, headers: HeaderMap, User(a): User, Path(lid): Path<String>, Form(f): Form<HashMap<String, String>>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, f.get("csrf").map(String::as_str).unwrap_or(""))?;
    revoke(&app, &a.id, &lid).await?;
    Ok(Redirect::to("/links?flash=Link+revoked.").into_response())
}
