//! The website: landing, sign-in, dashboard, projects, conversations,
//! invites, pairing, consent, uploads and the audit log. Server-rendered with
//! maud (every interpolation is escaped); app.js only refreshes and copies.

use std::convert::Infallible;
use std::time::Duration as StdDuration;

use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use chrono::{DateTime, Duration, Utc};
use futures_util::stream::Stream;
use maud::{html, Markup, DOCTYPE};
use serde::Deserialize;
use serde_json::json;

use crate::app::{audit, hash, id, now, token, App, AppInner, Fail};
use crate::auth::{check_csrf, session, Account, User};
use crate::domain::{self, Attachment, Event, Message, Project};
use crate::oauth::AuthorizeQ;
use crate::relay::{agent_label, set_mode};

// ---- pieces ----

pub fn agent_logo(kind: &str) -> Option<&'static str> {
    match kind.to_ascii_lowercase().as_str() {
        "claude" | "claude code" => Some("claude"),
        "codex" | "openai" => Some("openai"),
        "gemini" | "gemini cli" => Some("gemini"),
        "cursor" => Some("cursor"),
        "copilot" => Some("copilot"),
        _ => None,
    }
}

/// The logo of the assistant a tester used, from its OAuth client name.
pub fn client_logo(client: &str) -> Option<&'static str> {
    let c = client.to_ascii_lowercase();
    if c.contains("claude") || c.contains("anthropic") {
        Some("claude")
    } else if c.contains("chatgpt") || c.contains("openai") || c.contains("codex") {
        Some("openai")
    } else if c.contains("gemini") || c.contains("google") {
        Some("gemini")
    } else if c.contains("cursor") {
        Some("cursor")
    } else if c.contains("copilot") || c.contains("vscode") || c.contains("visual studio") {
        Some("copilot")
    } else {
        None
    }
}

fn logo(name: Option<&str>, alt: &str) -> Markup {
    html! { @if let Some(n) = name { img.logo src=(format!("/static/logos/{n}.svg")) alt=(alt) width="18" height="18"; } }
}

/// A project's own logo (found by the relay, like Sidra's tab icons), or a
/// lettered tile in its place, so every project is recognisable at a glance.
pub fn picon(id: &str, name: &str, etag: &str, size: u32) -> Markup {
    let style = format!("width:{size}px;height:{size}px");
    html! {
        @if etag.is_empty() {
            span.picon.tile style=(format!("{style};font-size:{}px", size * 9 / 20)) aria-hidden="true" { (initials(name).chars().next().unwrap_or('?')) }
        } @else {
            img.picon src=(format!("/projects/{id}/icon?v={etag}")) alt="" width=(size) height=(size) style=(style);
        }
    }
}

pub async fn project_icon_file(State(app): State<App>, Path(pid): Path<String>) -> Result<Response, Fail> {
    let row: Option<(Option<Vec<u8>>, String)> = sqlx::query_as("SELECT icon, icon_mime FROM projects WHERE id=$1").bind(&pid).fetch_optional(&app.db).await?;
    match row {
        Some((Some(bytes), mime)) if !mime.is_empty() => Ok((
            [(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, "public, max-age=86400".to_string())],
            bytes,
        ).into_response()),
        _ => Err(Fail::new(404, "no icon")),
    }
}

/// Gliph's unicorn, wherever Gliph is named.
fn gliph_mark(size: u32) -> Markup {
    html! { img.logo src="/static/logos/gliph.png" alt="" width=(size) height=(size) style=(format!("width:{size}px;height:{size}px;border-radius:{}px", size / 4)); }
}

/// Shell commands with a copy button. The `$` prompt is drawn by CSS, so
/// neither the button nor a hand selection copies it.
pub(crate) fn cmd(lines: &[&str]) -> Markup {
    html! {
        div.code.copyable {
            @for l in lines { span.ln { (l) } }
            button.btn.small.copy type="button" data-copy=(lines.join("\n")) { (ic("copy")) "Copy" }
        }
    }
}

/// A button icon (Lucide shapes), stroked in the text colour.
pub(crate) fn ic(name: &str) -> Markup {
    let d = match name {
        "log-out" => r#"<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><path d="m16 17 5-5-5-5"/><path d="M21 12H9"/>"#,
        "monitor-off" => r#"<path d="M17 17H4a2 2 0 0 1-2-2V5c0-1.5 1-2 1-2"/><path d="M22 15V5a2 2 0 0 0-2-2H9"/><path d="M8 21h8"/><path d="M12 17v4"/><path d="m2 2 20 20"/>"#,
        "square-pen" => r#"<path d="M12 3H5a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.375 2.625a1 1 0 0 1 3 3l-9.013 9.014a2 2 0 0 1-.853.505l-2.873.84a.5.5 0 0 1-.62-.62l.84-2.873a2 2 0 0 1 .506-.852z"/>"#,
        "paperclip" => r#"<path d="m16 6-8.414 8.586a2 2 0 0 0 2.829 2.829l8.414-8.586a4 4 0 1 0-5.657-5.657l-8.379 8.551a6 6 0 1 0 8.485 8.485l8.379-8.551"/>"#,
        "alert" => r#"<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"/><path d="M12 9v4"/><path d="M12 17h.01"/>"#,
        "log-in" => r#"<path d="M15 3h4a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-4"/><path d="m10 17 5-5-5-5"/><path d="M15 12H3"/>"#,
        "terminal" => r#"<path d="m4 17 6-6-6-6"/><path d="M12 19h8"/>"#,
        "arrow-left" => r#"<path d="m12 19-7-7 7-7"/><path d="M19 12H5"/>"#,
        "arrow-right" => r#"<path d="M5 12h14"/><path d="m12 5 7 7-7 7"/>"#,
        "copy" => r#"<rect width="14" height="14" x="8" y="8" rx="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>"#,
        "mail" => r#"<rect width="20" height="16" x="2" y="4" rx="2"/><path d="m22 7-8.97 5.7a1.94 1.94 0 0 1-2.06 0L2 7"/>"#,
        "plug" => r#"<path d="M12 22v-5"/><path d="M9 8V2"/><path d="M15 8V2"/><path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z"/>"#,
        "user-plus" => r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M19 8v6"/><path d="M22 11h-6"/>"#,
        "user-minus" => r#"<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 11h-6"/>"#,
        "eye" => r#"<path d="M2 12s3-7 10-7 10 7 10 7-3 7-10 7-10-7-10-7Z"/><circle cx="12" cy="12" r="3"/>"#,
        "zap" => r#"<path d="M13 2 3 14h9l-1 8 10-12h-9l1-8z"/>"#,
        "pause" => r#"<rect x="6" y="4" width="4" height="16" rx="1"/><rect x="14" y="4" width="4" height="16" rx="1"/>"#,
        "shield" => r#"<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10"/>"#,
        "x" => r#"<path d="M18 6 6 18"/><path d="m6 6 12 12"/>"#,
        "check" => r#"<path d="M20 6 9 17l-5-5"/>"#,
        "link" => r#"<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>"#,
        "trash" => r#"<path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>"#,
        "rotate" => r#"<path d="M21 12a9 9 0 1 1-9-9c2.52 0 4.93 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/>"#,
        "upload" => r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><path d="m17 8-5-5-5 5"/><path d="M12 3v12"/>"#,
        "key" => r#"<circle cx="7.5" cy="15.5" r="5.5"/><path d="m21 2-9.6 9.6"/><path d="m15.5 7.5 3 3L22 7l-3-3"/>"#,
        "send" => r#"<path d="m22 2-7 20-4-9-9-4Z"/><path d="M22 2 11 13"/>"#,
        _ => "",
    };
    maud::PreEscaped(format!(r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{d}</svg>"#))
}

pub(crate) fn orb(class: &str) -> Markup {
    html! {
        @if class == "xl" { span.orb-wrap aria-hidden="true" { span.orb.xl {} } }
        @else { span class=(format!("orb {class}")) aria-hidden="true" {} }
    }
}

fn initials(s: &str) -> String {
    let i: String = s.split(|c: char| c.is_whitespace() || c == '.' || c == '@').filter(|w| !w.is_empty()).take(2)
        .filter_map(|w| w.chars().next()).collect::<String>().to_uppercase();
    if i.is_empty() { "?".into() } else { i }
}

fn hhmm(t: &DateTime<Utc>) -> String {
    t.format("%H:%M").to_string()
}

pub(crate) fn ago(t: &DateTime<Utc>) -> String {
    let s = (now() - *t).num_seconds().max(0);
    match s {
        0..=59 => "now".into(),
        60..=3599 => format!("{}m", s / 60),
        3600..=86399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn stage_tag(stage: &str) -> Markup {
    let (cls, label) = match stage {
        "pending_approval" => ("warn", "waiting for approval"),
        "held" => ("bad", "held"),
        "approved" => ("", "queued"),
        "dispatched" | "accepted" => ("", "with the relay"),
        "submitted" | "working" => ("ok", "agent working"),
        "awaiting_sender" => ("warn", "question for the tester"),
        "answered" => ("", "answered"),
        "completed" => ("ok", "done"),
        "rejected" => ("", "declined"),
        "expired" => ("", "expired"),
        "cancelled" => ("", "cancelled"),
        "uncertain" => ("bad", "unconfirmed"),
        "failed" => ("bad", "failed"),
        "interrupted" => ("bad", "interrupted"),
        s => ("", s),
    };
    html! { span class=(format!("tag {cls}")) { (label) } }
}

fn state_dot(state: &str) -> &'static str {
    match state {
        "idle" | "done" => "ok",
        "working" => "blue",
        "blocked" | "needs_rebind" => "warn",
        "offline" | "unbound" => "",
        _ => "",
    }
}

/// A notice: a solid card with a coloured icon chip. kind: ok, warn or bad.
pub(crate) fn notice(kind: &str, body: Markup) -> Markup {
    let icon = match kind { "ok" => "check", "bad" => "x", _ => "alert" };
    html! { div class=(format!("notice {kind}")) role=(if kind == "ok" { "status" } else { "alert" }) { span.ni { (ic(icon)) } div.nb { (body) } } }
}

/// Stages of a message that still waits to reach an agent.
const QUEUED: &[&str] = &["pending_approval", "held", "approved", "dispatched"];

/// No agent receives the project's messages right now.
pub fn agent_gone(state: &str) -> bool {
    matches!(state, "needs_rebind" | "offline" | "unbound")
}

/// Agents on the owner's online computers, the ones in the project's folder first.
async fn rebind_candidates(app: &AppInner, owner: &str, p: &Project) -> Result<Vec<(String, String, serde_json::Value)>, Fail> {
    let ms: Vec<(String, String, serde_json::Value)> = sqlx::query_as("SELECT id, name, panes FROM machines WHERE account_id=$1 AND NOT revoked").bind(owner).fetch_all(&app.db).await?;
    let mut out: Vec<(String, String, serde_json::Value)> = ms.into_iter().filter(|m| app.machine_online(&m.0))
        .flat_map(|(id, name, panes)| panes.as_array().cloned().unwrap_or_default().into_iter().map(move |pn| (id.clone(), name.clone(), pn)))
        .filter(|(_, _, pn)| !pn["agent"].as_str().unwrap_or("").is_empty()).collect();
    out.sort_by_key(|(_, _, pn)| pn["cwd"].as_str() != Some(p.cwd.as_str()));
    Ok(out)
}

#[derive(Deserialize)]
pub struct BindForm {
    csrf: String,
    machine_id: String,
    pane_id: String,
}

/// Move here on the project page.
pub async fn project_bind(State(app): State<App>, headers: HeaderMap, User(a): User, Path(pid): Path<String>, Form(f): Form<BindForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let p = domain::project(&app, &pid).await?;
    if p.owner_id != a.id {
        return Err(Fail::new(404, "no such project"));
    }
    let panes: serde_json::Value = sqlx::query_scalar("SELECT panes FROM machines WHERE id=$1 AND account_id=$2 AND NOT revoked")
        .bind(&f.machine_id).bind(&a.id).fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "no such computer"))?;
    let pn = panes.as_array().into_iter().flatten().find(|x| x["pane_id"] == f.pane_id.as_str()).ok_or_else(|| Fail::new(404, "that pane is gone"))?;
    let g = |k: &str| pn[k].as_str().unwrap_or("").to_string();
    let b = crate::relay::Bind { pane_id: f.pane_id.clone(), agent_session: g("agent_session"), agent: g("agent"), cwd: g("cwd") };
    crate::relay::bind_project(&app, &a.id, &p, &f.machine_id, &b).await?;
    Ok(Redirect::to(&format!("/projects/{pid}")).into_response())
}

/// The agent's state as a pill: free, working or needs you.
fn status_tag(state: &str) -> Markup {
    let (cls, txt) = match state {
        "idle" | "done" => ("ok", "free"),
        "working" => ("blue", "working"),
        "blocked" => ("warn", "needs you"),
        "needs_rebind" => ("bad", "pane gone"),
        "offline" => ("off", "offline"),
        "unbound" => ("off", "no pane"),
        _ => ("", "unknown"),
    };
    html! { span class=(format!("tag {cls}")) { (txt) } }
}

fn state_label(state: &str) -> &'static str {
    match state {
        "idle" | "done" => "idle, ready for input",
        "working" => "working",
        "blocked" => "waiting at a permission prompt",
        "needs_rebind" => "pane gone: rebind needed",
        "offline" => "offline",
        "unbound" => "not bound to a pane",
        _ => "state unknown",
    }
}

pub fn layout(title: &str, acct: Option<&Account>, live: Option<String>, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Substrate" }
                meta name="description" content="Give people a direct line to the agents building your software.";
                link rel="icon" href="/static/favicon.svg" type="image/svg+xml";
                link rel="preconnect" href="https://fonts.googleapis.com";
                link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Inter+Tight:wght@400;500;600&family=JetBrains+Mono:wght@400;500&display=swap";
                link rel="stylesheet" href=(format!("/static/app.css?v={}", crate::asset_v()));
                script src=(format!("/static/app.js?v={}", crate::asset_v())) defer {}
            }
            body data-live=[live] {
                div.wrap {
                    header.top {
                        div.row style="gap:28px" {
                            a.brand href="/" { (orb("")) "Substrate" }
                            @if let Some(a) = acct {
                                nav.links aria-label="Main" {
                                    a href="/" { "Dashboard" }
                                    a href="/approvals" { "Approvals" }
                                    a href="/chat" { "Chat" }
                                    a href="/links" { "Access links" }
                                    a href="/machines" { "Machines" }
                                    a href="/audit" { "Audit log" }
                                }
                                span.sr { (a.email) }
                            } @else {
                                nav.links aria-label="Main" {
                                    a href="/#how" { "How it works" }
                                    a href="/#connect" { "Connect an assistant" }
                                    a href="/#cli" { "CLI" }
                                    a href="/#security" { "Security" }
                                    a href="https://gliph.sh" { "Gliph" }
                                }
                            }
                        }
                        div.row {
                            @if let Some(a) = acct {
                                a.row.me href="/profile" title="Profile" { img.avatar-img src=(format!("/accounts/{}/avatar", a.id)) alt="" width="24" height="24"; span.muted style="font-size:14px" { (a.label()) } }
                                form.inline method="post" action="/logout" { button.btn.small.tertiary type="submit" { (ic("log-out")) "Sign out" } }
                            } @else {
                                a.btn.primary.small href="/login" { (ic("terminal")) "Console" }
                            }
                        }
                    }
                    main { (body) }
                    footer.foot {
                        span { "Substrate is part of " a href="https://gliph.sh" { "gliph.sh" } }
                        span id="security" { "Your agent keeps its own permissions when you approve a message." }
                    }
                }
            }
        }
    }
}

pub(crate) fn csrf(a: &Account) -> Markup {
    html! { input type="hidden" name="csrf" value=(a.csrf); }
}

pub fn error_page(title: &str, msg: &str) -> Html<String> {
    Html(layout(title, None, None, html! {
        div.narrow style="padding:64px 0" { div.card { h1 { (title) } p.muted { (msg) } a.btn href="/" { (ic("arrow-left")) "Back to Substrate" } } }
    }).into_string())
}

// ---- landing & sign-in ----

pub async fn home(State(app): State<App>, headers: HeaderMap) -> Result<Response, Fail> {
    match session(&app, &headers).await {
        Some(a) => dashboard(&app, &a).await.map(IntoResponse::into_response),
        None => Ok(landing().into_response()),
    }
}

fn landing() -> Html<String> {
    Html(layout("Let testers talk to your coding agent", None, None, html! {
        section style="padding:56px 0 28px" class="stack" {
            h1.hero { "Let testers talk to " br; span.grad { "your coding agent." } }
            p.lead { "Testers tell their own assistant what they found. You approve the message, and it goes to the Claude Code session you have open. The reply goes back to their assistant." }
            div.between style="padding-top:8px" {
                div.row {
                    a.btn.primary href="/login" { (ic("terminal")) "Console" }
                    a.btn href="/auth/gliph" { (gliph_mark(18)) "Sign in with Gliph" }
                }
                a.muted href="#connect" style="text-decoration:none;font-size:14px" { (gliph_mark(18)) " Use it from a Gliph channel" }
            }
        }
        section.stage aria-label="How an approval looks" {
            div.horizon {}
            div.inner {
                (orb("xl"))
                div.card.glow style="width:100%" {
                    div.row style="font-size:17px;font-weight:500" { "Needs approval " span.badge { "2" } }
                }
                div.card style="width:100%;box-shadow:0 0 0 1px var(--ring)" {
                    div.between {
                        div.row { span.avatar { "SC" } div { div style="font-weight:500" { "Sarah Chen" } div.muted style="font-size:12px" { (logo(Some("claude"), "")) " via Claude → verna" } } }
                        span.mono style="color:var(--amber);font-size:12px" { "pending approval" }
                    }
                    div { "The settings page doesn't save my preferences. Screenshot attached." }
                }
            }
        }
        section id="connect" class="stack" style="align-items:center;padding:56px 0 16px" {
            span.muted style="font-size:14px" { "Your testers use the assistant they already have" }
            div.row style="gap:36px;justify-content:center;font-size:19px;font-weight:500" {
                span.row { img.logo.lg src="/static/logos/claude.svg" alt=""; "Claude" }
                span.row { img.logo.lg src="/static/logos/openai.svg" alt=""; "ChatGPT" }
                span.row { img.logo.lg src="/static/logos/gemini.svg" alt=""; "Gemini" }
                span.row { img.logo.lg src="/static/logos/copilot.svg" alt=""; "Copilot" }
                span.row { img.logo.lg src="/static/logos/cursor.svg" alt=""; "Cursor" }
            }
        }
        section id="how" class="stack" style="padding:56px 0 16px" {
            h2 { "Four steps from chat to fix" }
            div.grid {
                div.card { span.mono style="color:#8FA8FF;font-size:12px" { "01" } h3 { "Testers ask their assistant" } span.muted { "They tell Claude, ChatGPT or another MCP client. It sends their words and screenshots to Substrate." } }
                div.card { span.mono style="color:var(--amber);font-size:12px" { "02" } h3 { "You approve" } span.muted { "Approve on the web, in the CLI or with /substrate approve in Gliph. Auto relay skips this step." } }
                div.card { span.mono style="color:var(--teal);font-size:12px" { "03" } h3 { "Your agent gets the message" } span.muted { "The relay on your Mac connects out to Substrate and types the message into your Herdr pane. You open no ports." } }
                div.card { span.mono style="color:#C79BFF;font-size:12px" { "04" } h3 { "Testers hear back" } span.muted { "They get progress, questions and CI and deploy updates, then a note when the fix is ready to retest." } }
            }
        }
        section id="cli" class="grid" style="padding:40px 0;grid-template-columns:repeat(auto-fit,minmax(320px,1fr))" {
            div.card {
                h3 { "Give a tester access" }
                span.muted { "Create an access link for the panes they may reach, with the permissions you pick. They paste it into Claude or ChatGPT as a custom connector. They need no account." }
                div.row { a.btn href="/links/new" { (ic("link")) "Create an access link" } }
            }
            div.card {
                h3 { "Install the relay " span.muted style="font-weight:400" { "· macOS" } }
                (cmd(&["curl -fsSL https://substrate.gliph.sh/install | sh", "substrate login", "substrate daemon install"]))
            }
        }
    }).into_string())
}

#[derive(Deserialize)]
pub struct NextQ {
    #[serde(default)]
    next: String,
}

pub async fn login_get(State(app): State<App>, headers: HeaderMap, Query(q): Query<NextQ>) -> Response {
    if session(&app, &headers).await.is_some() {
        return Redirect::to(&crate::auth::safe_next(&q.next)).into_response();
    }
    login_page(&app, &q.next, None).into_response()
}

pub fn login_page(_app: &AppInner, next: &str, err: Option<&str>) -> Html<String> {
    let next = crate::auth::safe_next(next);
    Html(layout("Sign in", None, None, html! {
        div.page-horizon {}
        div.narrow style="padding:48px 0 160px" class="stack" {
            div style="display:flex;justify-content:center;padding:12px 0 24px" { (orb("xl")) }
            h1 style="text-align:center;margin-bottom:16px" { "Sign in to " span.grad { "Substrate" } }
            div.card {
                @if let Some(e) = err { (notice("bad", html! { (e) })) }
                form method="post" action="/login" class="stack" {
                    input type="hidden" name="next" value=(next);
                    div.field { label for="email" { "Email" } input id="email" type="email" name="email" required autocomplete="email" placeholder="you@company.com"; }
                    div.row.pair {
                        button.btn.primary type="submit" { (ic("mail")) "Send sign-in link" }
                        a.btn href=(format!("/auth/gliph?next={}", urlencoding::encode(&next))) { (gliph_mark(18)) "Sign in with Gliph" }
                    }
                }
            }
            p.faint style="text-align:center;font-size:13px" { "We email you a link that works once." }
        }
    }).into_string())
}

pub fn check_email_page(email: &str) -> Html<String> {
    Html(layout("Check your email", None, None, html! {
        div.narrow style="padding:64px 0" class="stack" {
            div style="display:flex;justify-content:center;padding:12px 0 24px" { (orb("xl")) }
            div.card { h1 { "Check your email" } p.muted { "We sent a sign-in link to " strong { (email) } ". It works once and expires in 15 minutes." } }
        }
    }).into_string())
}

pub fn magic_confirm_page(t: &str) -> Html<String> {
    Html(layout("Finish signing in", None, None, html! {
        div.narrow style="padding:64px 0" class="stack" {
            div.card {
                h1 { "Finish signing in" }
                form method="post" action="/auth/magic" { input type="hidden" name="t" value=(t); button.btn.primary type="submit" { (ic("log-in")) "Sign in" } }
            }
        }
    }).into_string())
}

// ---- dashboard ----

async fn dashboard(app: &AppInner, a: &Account) -> Result<Html<String>, Fail> {
    let owned = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1 ORDER BY name").bind(&a.id).fetch_all(&app.db).await?;
    let testing: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT p.id, p.name, acc.email, p.icon_etag FROM members m JOIN projects p ON p.id=m.project_id JOIN accounts acc ON acc.id=p.owner_id WHERE m.account_id=$1 AND m.role='tester' ORDER BY p.name",
    ).bind(&a.id).fetch_all(&app.db).await?;
    let approvals = sqlx::query_as::<_, Message>(
        "SELECT m.* FROM messages m JOIN projects p ON p.id=m.project_id WHERE p.owner_id=$1 AND m.stage IN ('pending_approval','held') ORDER BY m.order_n",
    ).bind(&a.id).fetch_all(&app.db).await?;
    let recent = sqlx::query_as::<_, Message>(
        "SELECT m.* FROM messages m JOIN projects p ON p.id=m.project_id WHERE p.owner_id=$1 ORDER BY m.updated_at DESC LIMIT 12",
    ).bind(&a.id).fetch_all(&app.db).await?;
    let machines: Vec<(String, String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT id, name, last_seen FROM machines WHERE account_id=$1 AND NOT revoked ORDER BY created_at",
    ).bind(&a.id).fetch_all(&app.db).await?;
    let hour = (now() + Duration::hours(0)).format("%H").to_string().parse::<u32>().unwrap_or(12);
    let greet = match hour { 5..=11 => "Good morning,", 12..=17 => "Good afternoon,", _ => "Good evening," };
    let completed: i64 = sqlx::query_scalar("SELECT count(*) FROM messages m JOIN projects p ON p.id=m.project_id WHERE p.owner_id=$1 AND m.stage='completed'").bind(&a.id).fetch_one(&app.db).await?;
    let queued: Vec<(String, i64)> = sqlx::query_as("SELECT m.project_id, count(*) FROM messages m JOIN projects p ON p.id=m.project_id WHERE p.owner_id=$1 AND m.stage = ANY($2) GROUP BY 1")
        .bind(&a.id).bind(QUEUED).fetch_all(&app.db).await?;
    let gone: Vec<(&Project, i64)> = owned.iter().filter(|p| agent_gone(&p.state))
        .map(|p| (p, queued.iter().find(|q| q.0 == p.id).map(|q| q.1).unwrap_or(0)))
        .filter(|(p, q)| p.state != "unbound" || *q > 0).collect();

    Ok(Html(layout("Dashboard", Some(a), Some("/live".into()), html! {
        div.stack style="padding:12px 0 48px" {
            div.between style="padding:8px 4px 12px" {
                div style="font-size:clamp(32px,4vw,44px);font-weight:500;letter-spacing:-.03em;line-height:1.05" { span.grad { (greet) } br; (a.label()) }
                div.row { a.btn.primary href="/links/new" { (ic("link")) "New access link" } }
            }
            @if !approvals.is_empty() {
                div.card.glow {
                    div.row style="font-size:19px;font-weight:500" { "Needs approval " span.badge { (approvals.len()) } }
                    div.list style="padding:0;background:transparent" {
                        @for m in &approvals {
                            a.item href=(format!("/conversations/{}", m.conversation_id)) {
                                span.l { span class=(format!("dot {}", if m.stage == "held" { "bad" } else { "warn" })) {} span { (truncate(&m.text, 90)) } @if m.stage == "held" { span.tag.bad { "held · " (m.risk) } } }
                                span.r { (m.sender_label) " · " (logo(client_logo(&m.client_name), "")) (m.client_name) " · " (ago(&m.created_at)) }
                            }
                        }
                    }
                }
            }
            @if !gone.is_empty() {
                div.card.attention {
                    div.row style="font-size:19px;font-weight:500" { span.ni { (ic("alert")) } "Agents gone" }
                    div.list style="padding:0;background:transparent" {
                        @for (p, q) in &gone {
                            a.item href=(format!("/projects/{}", p.id)) {
                                span.l { (picon(&p.id, &p.name, &p.icon_etag, 24)) strong style="font-weight:500" { (p.name) } (status_tag(&p.state)) }
                                span.r { @if *q > 0 { (q) " queued · " } "Rebind" }
                            }
                        }
                    }
                }
            }
            div.grid.tight {
                div.card.stat { span.k { "Projects" } span.big { (owned.len()) } }
                div.card.stat { span.k { "Machines online" } span.v { (machines.iter().filter(|m| app.machine_online(&m.0)).count()) " of " (machines.len()) } }
                div.card.stat { span.k { "Waiting" } span.v style="color:var(--amber)" { (approvals.len()) } }
                div.card.stat { span.k { "Completed" } span.big.grad { (completed) } }
            }
            @if owned.is_empty() {
                div.card {
                    h3 { "Set up your first project" }
                    p.muted { "Install the relay on the Mac where your agents run. Each agent in Herdr gets a project." }
                    (cmd(&[&format!("curl -fsSL {}/install | sh", app.cfg.public_url), "substrate login", "substrate daemon install"]))
                }
            }
            div.split {
                div.main.stack {
                    @if !owned.is_empty() {
                        div.list {
                            div.label { "Your projects" }
                            @for p in &owned {
                                a.item href=(format!("/projects/{}", p.id)) {
                                    span.l { (picon(&p.id, &p.name, &p.icon_etag, 28)) strong style="font-weight:500" { (p.name) } span.muted.mono { (p.handle) } span class=(format!("dot {}", state_dot(&p.state))) {} (logo(agent_logo(&p.agent), "")) }
                                    span.r { (status_tag(&p.state)) " " @if p.auto_active() { span.tag.warn { "auto relay" } } @else { "approval required" } }
                                }
                            }
                        }
                    }
                    @if !recent.is_empty() {
                        div.list {
                            div.label { "Recent" }
                            @for m in &recent {
                                a.item href=(format!("/conversations/{}", m.conversation_id)) {
                                    span.l { (stage_tag(&m.stage)) span { (truncate(&m.text, 70)) } }
                                    span.r { (m.sender_label) " · " (logo(client_logo(&m.client_name), "")) " " (ago(&m.updated_at)) }
                                }
                            }
                        }
                    }
                }
                div.side.stack {
                    @if !testing.is_empty() {
                        div.list {
                            div.label { "Projects you test" }
                            @for (pid, name, owner, etag) in &testing { a.item href=(format!("/projects/{pid}")) { span.l { (picon(pid, name, etag, 24)) (name) } span.r { (owner) } } }
                            a.item href="/connect" { span.l { (ic("plug")) "Connect your assistant" } }
                        }
                    }
                    div.list {
                        div.label { "Machines" }
                        @if machines.is_empty() { div.item { span.muted { "None yet. Run " span.mono { "substrate login" } } } }
                        @for (mid, name, seen) in &machines {
                            a.item href="/machines" {
                                span.l { span class=(format!("dot {}", if app.machine_online(mid) { "ok" } else { "" })) {} (name) }
                                span.r { @if app.machine_online(mid) { "online" } @else if let Some(s) = seen { "seen " (ago(s)) " ago" } @else { "never connected" } }
                            }
                        }
                    }
                }
            }
        }
    }).into_string()))
}

fn human_size(n: i64) -> String {
    match n {
        0..=1023 => format!("{n} bytes"),
        1024..=1_048_575 => format!("{} KB", n / 1024),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.0),
    }
}

fn truncate(s: &str, n: usize) -> String {
    let one = s.replace('\n', " ");
    if one.chars().count() > n { format!("{}…", one.chars().take(n).collect::<String>()) } else { one }
}

// ---- project ----

pub async fn project_page(State(app): State<App>, User(a): User, Path(pid): Path<String>, Query(f): Query<FlashQ>) -> Result<Response, Fail> {
    domain::role(&app, &pid, &a.id).await?.ok_or_else(|| Fail::new(404, "no such project"))?;
    let p = domain::project(&app, &pid).await?;
    let owner = p.owner_id == a.id;
    let convs: Vec<(String, String, DateTime<Utc>, String, String, String, String)> = sqlx::query_as(
        "SELECT c.id, c.title, c.updated_at, COALESCE(m.stage,''), COALESCE(m.sender_label,''), COALESCE(m.client_name,''), COALESCE(m.deploy_stage,'')
         FROM conversations c LEFT JOIN LATERAL (SELECT * FROM messages WHERE conversation_id=c.id ORDER BY order_n DESC LIMIT 1) m ON true
         WHERE c.project_id=$1 AND ($2 OR c.sender_id=$3) ORDER BY c.updated_at DESC LIMIT 50",
    ).bind(&pid).bind(owner).bind(&a.id).fetch_all(&app.db).await?;
    let pending: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE project_id=$1 AND stage IN ('pending_approval','held')").bind(&pid).fetch_one(&app.db).await?;
    let queued: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE project_id=$1 AND stage = ANY($2)").bind(&pid).bind(QUEUED).fetch_one(&app.db).await?;
    let candidates = if owner && agent_gone(&p.state) { rebind_candidates(&app, &a.id, &p).await? } else { vec![] };
    let held: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE project_id=$1 AND stage='held'").bind(&pid).fetch_one(&app.db).await?;
    let completed: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE project_id=$1 AND stage='completed'").bind(&pid).fetch_one(&app.db).await?;
    let deploying: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE project_id=$1 AND deploy_stage IN ('fix_committed','ci_running','ci_passed','deploying')").bind(&pid).fetch_one(&app.db).await?;
    let members: Vec<(String, String, String, String, bool, bool, bool)> = sqlx::query_as(
        "SELECT a.id, a.email, a.name, m.role, m.trusted_auto, m.can_deploy_test, m.can_deploy_prod FROM members m JOIN accounts a ON a.id=m.account_id WHERE m.project_id=$1 ORDER BY m.role, a.email",
    ).bind(&pid).fetch_all(&app.db).await?;
    let invites: Vec<(String, String, i32, i32, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, label, uses, max_uses, expires_at FROM invites WHERE project_id=$1 AND NOT revoked AND expires_at > now() AND uses < max_uses ORDER BY created_at DESC",
    ).bind(&pid).fetch_all(&app.db).await?;
    let machine: Option<String> = sqlx::query_scalar("SELECT name FROM machines WHERE id=$1").bind(&p.machine_id).fetch_optional(&app.db).await?;
    let online = app.machine_online(&p.machine_id);
    let owner_name = domain::account_label(&app, &p.owner_id).await;

    Ok(Html(layout(&p.name, Some(&a), Some(format!("/projects/{pid}/live")), html! {
        div.stack style="padding:12px 0 48px" {
            @if let Some(msg) = &f.flash { (notice("ok", html! { (msg) })) }
            @if let Some(secret) = &f.invite {
                div.card.glow {
                    h3 { "Invitation link" }
                    p.muted { "Send this to your tester. You see it only once." }
                    div.secret { (format!("{}/invites/{}", app.cfg.public_url, secret)) }
                    div.row { button.btn.small type="button" data-copy=(format!("{}/invites/{}", app.cfg.public_url, secret)) { (ic("copy")) "Copy link" } }
                }
            }
            div.between style="padding:8px 4px" {
                div.row style="gap:16px;align-items:center" { (picon(&p.id, &p.name, &p.icon_etag, 56)) div {
                    h1 { (p.name) }
                    div.row.muted style="padding-top:6px" { (logo(agent_logo(&p.agent), "")) (agent_label(&p.agent)) " in herdr " span.mono { (if p.pane_id.is_empty() { "none" } else { &p.pane_id }) } @if !owner { " · by " (owner_name) } }
                } }
                @if owner {
                    div.row { a.btn href="#invite" { (ic("user-plus")) "Invite a tester" } }
                }
            }
            @if owner && pending > 0 {
                div.card.glow {
                    div.between {
                        div { div.row style="font-size:19px;font-weight:500" { "Needs approval " span.badge { (pending) } } @if held > 0 { div.muted style="font-size:14px" { (held) " held by a risk flag." } } }
                        a.btn.primary href="#conversations" { (ic("eye")) "Review" }
                    }
                }
            }
            @if owner && agent_gone(&p.state) {
                div.card.attention {
                    h3.row { span.ni { (ic("alert")) } @match p.state.as_str() { "offline" => "The computer is offline", "unbound" => "No pane", _ => "The agent's session ended" } }
                    p.muted style="margin:0" {
                        @if queued > 0 { (queued) @if queued == 1 { " message waits" } @else { " messages wait" } " for an agent. " }
                        "Testers see the agent as offline. Move the project to a running agent:"
                    }
                    @if candidates.is_empty() { p.muted { "No agent runs on an online computer. Start one in Herdr." } }
                    div.list style="padding:0;background:transparent" {
                        @for (mid, mname, pn) in &candidates {
                            @let pid_ = pn["pane_id"].as_str().unwrap_or("");
                            div.item {
                                span.l { (logo(agent_logo(pn["agent"].as_str().unwrap_or("")), "")) span { (pn["title"].as_str().unwrap_or("")) } span.mono.faint { (pid_) } span.faint { (mname) }
                                    @if pn["cwd"].as_str() == Some(p.cwd.as_str()) && !p.cwd.is_empty() { span.tag.ok { "same folder" } } }
                                span.r { (status_tag(pn["status"].as_str().unwrap_or("")))
                                    form.inline method="post" action=(format!("/projects/{}/bind", p.id)) { (csrf(&a)) input type="hidden" name="machine_id" value=(mid); input type="hidden" name="pane_id" value=(pid_); button.btn.small.primary type="submit" { (ic("arrow-right")) "Move here" } } }
                            }
                        }
                    }
                }
            }
            div.grid.tight {
                div.card.stat { span.k { "Agent" } span.v style=(format!("color:{}", if matches!(p.state.as_str(), "idle"|"done") { "var(--teal)" } else { "var(--text)" })) { (state_label(&p.state)) } }
                div.card.stat { span.k { "Connection" } span.v { @if online { "Online" } @else { "Offline" } } span.faint style="font-size:12px" { (machine.clone().unwrap_or_else(|| "no machine".into())) } }
                div.card.stat { span.k { "Deploying" } span.v { (logo(Some("github-actions"), "")) " " (deploying) } }
                div.card.stat { span.k { "Completed" } span.big.grad { (completed) } }
            }
            @if owner {
                div.split {
                    div.main {
                        @if p.auto_active() {
                            section.card.attention {
                                div.between { span style="font-size:13px;color:var(--amber)" { (p.name) " · agent access" } @if let Some(u) = p.auto_until { span.tag.warn { "until " (u.format("%H:%M UTC %b %d")) } } }
                                div.row style="font-size:26px;font-weight:500;letter-spacing:-.02em" { span.ni { (ic("zap")) } "Auto relay is on" }
                                p.muted style="margin:0" { "Messages go to your agent without your approval. Flagged messages still wait for you. Your agent's permission prompts stay on." }
                                form method="post" action=(format!("/projects/{pid}/mode")) { (csrf(&a)) input type="hidden" name="mode" value="approve"; button.btn.amber type="submit" { (ic("pause")) "Turn off auto relay" } }
                            }
                        } @else {
                            section.card {
                                div.between { h3 { "Relay mode: approval required" } span.tag { "default" } }
                                p.muted style="margin:0" { "Every message waits for your approval. Auto relay sends messages without it. Flagged messages still wait." }
                                form method="post" action=(format!("/projects/{pid}/mode")) class="row" data-confirm="Turn on auto relay? Testers' messages reach your agent without your approval." {
                                    (csrf(&a)) input type="hidden" name="mode" value="auto";
                                    label.sr for="minutes" { "Duration" }
                                    select id="minutes" name="minutes" style="width:auto" { option value="60" { "for 1 hour" } option value="480" selected { "for 8 hours" } option value="0" { "until I turn it off" } }
                                    input type="hidden" name="confirm" value="yes";
                                    button.btn type="submit" { (ic("zap")) "Turn on auto relay" }
                                }
                            }
                        }
                    }
                    div.side {
                        section.card {
                            h3 { "Collaborators" }
                            div.stack {
                                @for (aid, email, name, role, trusted, dtest, dprod) in &members {
                                    div.between {
                                        span.row { span.avatar style=(if *trusted { "box-shadow:0 0 0 2px var(--teal)" } else { "" }) { (initials(if name.is_empty() { email } else { name })) } span { (if name.is_empty() { email.as_str() } else { name.as_str() }) br; span.faint style="font-size:12px" { @if email.ends_with("@links.substrate.invalid") { a href="/links" { "access link" } } @else { (role) } @if *trusted { " · may auto relay" } } } }
                                        @if role != "owner" {
                                            div.row {
                                                form.inline method="post" action=(format!("/projects/{pid}/members/{aid}/trust")) { (csrf(&a)) input type="hidden" name="on" value=(if *trusted { "0" } else { "1" }); button.btn.small type="submit" { (ic("shield")) @if *trusted { "Untrust" } @else { "Trust" } } }
                                                @for (target, label, on) in [("test", "Test", *dtest), ("production", "Prod", *dprod)] {
                                                    form.inline method="post" action=(format!("/projects/{pid}/members/{aid}/deploy")) title=(format!("{} deploys to {target}", if on { "May ask for" } else { "May not ask for" })) {
                                                        (csrf(&a)) input type="hidden" name="target" value=(target); input type="hidden" name="on" value=(if on { "0" } else { "1" });
                                                        button.btn.small type="submit" { span class=(format!("tag {}", if on { "ok" } else { "off" })) { (label) } }
                                                    }
                                                }
                                                form.inline method="post" action=(format!("/projects/{pid}/members/{aid}/remove")) data-confirm="Remove this tester? Their queued messages will not be delivered." { (csrf(&a)) button.btn.small.danger type="submit" { (ic("user-minus")) "Remove" } }
                                            }
                                        }
                                    }
                                }
                            }
                            span.faint style="font-size:12px" { "Messages from trusted testers skip approval. A ring marks them. Test and Prod: what a tester may ask the agent to deploy to." }
                        }
                    }
                }
            }
            section id="conversations" class="list" {
                div.label { "Conversations" }
                @if convs.is_empty() { div.item { span.muted { "No conversations yet." } } }
                @for (cid, title, upd, stage, from, client, dep) in &convs {
                    a.item href=(format!("/conversations/{cid}")) {
                        span.l { (stage_tag(stage)) span { (truncate(title, 80)) } @if !dep.is_empty() { span.tag.ok { (dep.replace('_', " ")) } } }
                        span.r { (from) " · " (logo(client_logo(client), "")) (client) " · " (ago(upd)) }
                    }
                }
            }
            @if owner {
                div.split {
                    section.main.card id="invite" {
                        h3 { "Invite a tester" }
                        p.muted style="margin:0" { "Testers can send messages and screenshots to this project and read replies. They can't change its settings or your machine." }
                        form method="post" action=(format!("/projects/{pid}/invites")) class="grid" style="align-items:end" {
                            (csrf(&a))
                            div.field { label for="label" { "Tester's name" } input id="label" type="text" name="label" placeholder="Sarah" maxlength="64"; }
                            div.field { label for="days" { "Expires" } select id="days" name="days" { option value="1" { "in 1 day" } option value="7" { "in 7 days" } option value="14" selected { "in 14 days" } option value="30" { "in 30 days" } } }
                            div.field { label for="uses" { "Uses" } select id="uses" name="max_uses" { option value="1" selected { "one person" } option value="5" { "up to 5 people" } option value="25" { "up to 25 people" } } }
                            button.btn.primary type="submit" { (ic("link")) "Create link" }
                        }
                        @if !invites.is_empty() {
                            div.stack {
                                @for (iid, label, uses, max, exp) in &invites {
                                    div.inset.between {
                                        span { (if label.is_empty() { "invitation" } else { label }) span.faint style="font-size:13px" { " · " (uses) "/" (max) " used · expires " (exp.format("%b %d")) } }
                                        form.inline method="post" action=(format!("/projects/{pid}/invites/{iid}/revoke")) { (csrf(&a)) button.btn.small.danger type="submit" { (ic("trash")) "Revoke" } }
                                    }
                                }
                            }
                        }
                    }
                    section.side.card {
                        h3 { "Settings" }
                        form method="post" action=(format!("/projects/{pid}/settings")) class="stack" {
                            (csrf(&a))
                            div.field { label for="name" { "Name" } input id="name" type="text" name="name" value=(p.name) maxlength="64"; }
                            div.field { label for="desc" { "Description for testers" } textarea id="desc" name="description" maxlength="500" { (p.description) } }
                            div.field { label for="hosts" { "Hosts for retest links, comma-separated" } input id="hosts" type="text" name="retest_hosts" value=(p.retest_hosts) placeholder="verna.app, staging.verna.app"; }
                            label.row style="color:var(--text-2);font-size:14px" { input type="checkbox" name="ci_public" value="1" checked[p.ci_public]; "Show CI links to testers" }
                            button.btn type="submit" { (ic("check")) "Save" }
                        }
                        div.inset style="font-size:13px" {
                            "Gliph channel: "
                            @if p.gliph_channel.is_empty() { span.muted { "not linked. In a Gliph channel, run " span.mono { "/invite substrate" } ", then " span.mono { "/substrate link " (p.handle) } } }
                            @else { span.tag.ok { "linked" } form.inline method="post" action=(format!("/projects/{pid}/unlink-gliph")) { (csrf(&a)) " " button.btn.small.tertiary type="submit" { (ic("x")) "Unlink" } } }
                        }
                    }
                }
            }
        }
    }).into_string()).into_response())
}

#[derive(Deserialize)]
pub struct FlashQ {
    #[serde(default)]
    flash: Option<String>,
    #[serde(default)]
    invite: Option<String>,
}

#[derive(Deserialize)]
pub struct CsrfOnly {
    csrf: String,
}

#[derive(Deserialize)]
pub struct ModeForm {
    csrf: String,
    mode: String,
    #[serde(default)]
    minutes: Option<i64>,
    #[serde(default)]
    confirm: String,
}

pub async fn project_mode(State(app): State<App>, headers: HeaderMap, User(a): User, Path(pid): Path<String>, Form(f): Form<ModeForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let p = domain::require_owner(&app, &pid, &a.id).await?;
    if f.mode == "auto" && f.confirm != "yes" {
        return Err(Fail::new(400, "auto relay needs explicit confirmation"));
    }
    set_mode(&app, &a.id, &p, &f.mode, f.minutes.filter(|m| *m > 0), "web").await?;
    if f.mode == "approve" {
        domain::pump(&app, &pid).await.ok();
    }
    Ok(Redirect::to(&format!("/projects/{pid}?flash={}", urlencoding::encode(if f.mode == "auto" { "Auto relay is on." } else { "Auto relay is off. Every message waits for you." }))).into_response())
}

#[derive(Deserialize)]
pub struct SettingsForm {
    csrf: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    retest_hosts: String,
    #[serde(default)]
    ci_public: Option<String>,
}

pub async fn project_settings(State(app): State<App>, headers: HeaderMap, User(a): User, Path(pid): Path<String>, Form(f): Form<SettingsForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    domain::require_owner(&app, &pid, &a.id).await?;
    let hosts: Vec<String> = f.retest_hosts.split(',').map(|h| h.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_ascii_lowercase())
        .filter(|h| !h.is_empty() && h.len() < 200 && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')).take(10).collect();
    let name = substrate_proto::sanitize(&f.name, 64);
    sqlx::query("UPDATE projects SET name=CASE WHEN $2='' THEN name ELSE $2 END, description=$3, retest_hosts=$4, ci_public=$5 WHERE id=$1")
        .bind(&pid).bind(&name).bind(substrate_proto::sanitize(&f.description, 500)).bind(hosts.join(",")).bind(f.ci_public.is_some())
        .execute(&app.db).await?;
    audit(&app, &a.id, &pid, &a.label(), "project.settings", &pid, &format!("retest_hosts={} ci_public={}", hosts.join(","), f.ci_public.is_some())).await;
    Ok(Redirect::to(&format!("/projects/{pid}?flash=Saved.")).into_response())
}

#[derive(Deserialize)]
pub struct InviteForm {
    csrf: String,
    #[serde(default)]
    label: String,
    days: i64,
    max_uses: i32,
}

pub async fn invite_create(State(app): State<App>, headers: HeaderMap, User(a): User, Path(pid): Path<String>, Form(f): Form<InviteForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    domain::require_owner(&app, &pid, &a.id).await?;
    let t = token();
    let iid = id("inv");
    sqlx::query("INSERT INTO invites (id, project_id, token_hash, label, max_uses, created_by, expires_at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(&iid).bind(&pid).bind(hash(&t)).bind(substrate_proto::sanitize(&f.label, 64)).bind(f.max_uses.clamp(1, 25))
        .bind(&a.id).bind(now() + Duration::days(f.days.clamp(1, 30))).execute(&app.db).await?;
    audit(&app, &a.id, &pid, &a.label(), "invite.created", &iid, &f.label).await;
    Ok(Redirect::to(&format!("/projects/{pid}?invite={t}#top")).into_response())
}

pub async fn invite_revoke(State(app): State<App>, headers: HeaderMap, User(a): User, Path((pid, iid)): Path<(String, String)>, Form(f): Form<CsrfOnly>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    domain::require_owner(&app, &pid, &a.id).await?;
    sqlx::query("UPDATE invites SET revoked=true WHERE id=$1 AND project_id=$2").bind(&iid).bind(&pid).execute(&app.db).await?;
    audit(&app, &a.id, &pid, &a.label(), "invite.revoked", &iid, "").await;
    Ok(Redirect::to(&format!("/projects/{pid}?flash=Invitation+revoked.")).into_response())
}

#[derive(Deserialize)]
pub struct TrustForm {
    csrf: String,
    on: String,
}

pub async fn member_trust(State(app): State<App>, headers: HeaderMap, User(a): User, Path((pid, aid)): Path<(String, String)>, Form(f): Form<TrustForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    domain::require_owner(&app, &pid, &a.id).await?;
    let on = f.on == "1";
    sqlx::query("UPDATE members SET trusted_auto=$3 WHERE project_id=$1 AND account_id=$2 AND role='tester'").bind(&pid).bind(&aid).bind(on).execute(&app.db).await?;
    audit(&app, &a.id, &pid, &a.label(), if on { "member.trusted" } else { "member.untrusted" }, &aid, "").await;
    Ok(Redirect::to(&format!("/projects/{pid}")).into_response())
}

#[derive(Deserialize)]
pub struct DeployForm {
    csrf: String,
    target: String,
    on: String,
}

pub async fn member_deploy(State(app): State<App>, headers: HeaderMap, User(a): User, Path((pid, aid)): Path<(String, String)>, Form(f): Form<DeployForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    domain::require_owner(&app, &pid, &a.id).await?;
    let col = match f.target.as_str() { "test" => "can_deploy_test", "production" => "can_deploy_prod", _ => return Err(Fail::new(400, "target is test or production")) };
    let on = f.on == "1";
    sqlx::query(&format!("UPDATE members SET {col}=$3 WHERE project_id=$1 AND account_id=$2 AND role='tester'")).bind(&pid).bind(&aid).bind(on).execute(&app.db).await?;
    audit(&app, &a.id, &pid, &a.label(), "member.deploy", &aid, &format!("{}={on}", f.target)).await;
    Ok(Redirect::to(&format!("/projects/{pid}")).into_response())
}

pub async fn member_remove(State(app): State<App>, headers: HeaderMap, User(a): User, Path((pid, aid)): Path<(String, String)>, Form(f): Form<CsrfOnly>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    domain::require_owner(&app, &pid, &a.id).await?;
    sqlx::query("DELETE FROM members WHERE project_id=$1 AND account_id=$2 AND role='tester'").bind(&pid).bind(&aid).execute(&app.db).await?;
    audit(&app, &a.id, &pid, &a.label(), "member.removed", &aid, "").await;
    Ok(Redirect::to(&format!("/projects/{pid}?flash=Tester+removed.")).into_response())
}

pub async fn unlink_gliph(State(app): State<App>, headers: HeaderMap, User(a): User, Path(pid): Path<String>, Form(f): Form<CsrfOnly>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    domain::require_owner(&app, &pid, &a.id).await?;
    sqlx::query("UPDATE projects SET gliph_channel='' WHERE id=$1").bind(&pid).execute(&app.db).await?;
    app.gliph(crate::app::GliphOut::Resubscribe);
    audit(&app, &a.id, &pid, &a.label(), "gliph.unlinked", &pid, "web").await;
    Ok(Redirect::to(&format!("/projects/{pid}?flash=Unlinked+from+Gliph.")).into_response())
}

/// Server-sent "changed" whenever any of these accounts' agents change: the
/// relay pushes pane and agent state over its websocket, this passes it on.
pub fn account_live(app: &AppInner, accounts: &[String]) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>> + use<>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    for acct in accounts {
        let mut w = app.waiter(&format!("acct:{acct}"));
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) = w.recv().await {
                if tx.send(()).is_err() {
                    break;
                }
            }
        });
    }
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await?;
        // Coalesce a burst (Hello, reconcile, pumps) into one refresh.
        tokio::time::sleep(StdDuration::from_millis(300)).await;
        while rx.try_recv().is_ok() {}
        Some((Ok(SseEvent::default().data("changed")), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// GET /live: the dashboard and Machines refresh when the owner's agents change.
pub async fn live(State(app): State<App>, User(a): User) -> impl IntoResponse {
    account_live(&app, std::slice::from_ref(&a.id))
}

pub async fn project_live(State(app): State<App>, User(a): User, Path(pid): Path<String>) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, Fail> {
    domain::role(&app, &pid, &a.id).await?.ok_or_else(|| Fail::new(404, "no such project"))?;
    let stream = futures_util::stream::unfold((app, pid, String::new()), |(app, pid, last)| async move {
        loop {
            tokio::time::sleep(StdDuration::from_secs(4)).await;
            let sig: Option<String> = sqlx::query_scalar(
                "SELECT concat(p.state, p.mode, coalesce(max(m.updated_at)::text,'')) FROM projects p LEFT JOIN messages m ON m.project_id=p.id WHERE p.id=$1 GROUP BY p.state, p.mode",
            ).bind(&pid).fetch_optional(&app.db).await.ok().flatten();
            let sig = format!("{}{}", sig.unwrap_or_default(), app.machine_online(&domain::project(&app, &pid).await.map(|p| p.machine_id).unwrap_or_default()));
            if !last.is_empty() && sig != last {
                return Some((Ok(SseEvent::default().data("changed")), (app, pid, sig)));
            }
            if last.is_empty() {
                return Some((Ok(SseEvent::default().comment("ready")), (app, pid, sig)));
            }
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ---- conversation ----

pub async fn conversation(State(app): State<App>, User(a): User, Path(cid): Path<String>) -> Result<Response, Fail> {
    let row: Option<(String, String, String)> = sqlx::query_as("SELECT project_id, sender_id, title FROM conversations WHERE id=$1").bind(&cid).fetch_optional(&app.db).await?;
    let Some((pid, sender, title)) = row else { return Err(Fail::new(404, "no such conversation")) };
    let p = domain::project(&app, &pid).await?;
    let owner = p.owner_id == a.id;
    if !owner && (sender != a.id || domain::role(&app, &pid, &a.id).await?.is_none()) {
        return Err(Fail::new(404, "no such conversation"));
    }
    let events = sqlx::query_as::<_, Event>("SELECT * FROM events WHERE conversation_id=$1 ORDER BY seq").bind(&cid).fetch_all(&app.db).await?;
    let msgs = sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE conversation_id=$1 ORDER BY order_n").bind(&cid).fetch_all(&app.db).await?;
    let atts = sqlx::query_as::<_, Attachment>("SELECT * FROM attachments WHERE conversation_id=$1 ORDER BY created_at").bind(&cid).fetch_all(&app.db).await?;
    let machine: Option<String> = sqlx::query_scalar("SELECT name FROM machines WHERE id=$1").bind(&p.machine_id).fetch_optional(&app.db).await?;
    let online = app.machine_online(&p.machine_id);
    let latest = msgs.last();
    let own_host = app.cfg.host();
    let status = match latest { Some(m) => Some(domain::status_block(&app, m, i64::MAX, !owner).await?), None => None };

    Ok(Html(layout(&title, Some(&a), Some(format!("/conversations/{cid}/live")), html! {
        div.between style="padding:4px 0 16px" {
            div.crumbs { (picon(&p.id, &p.name, &p.icon_etag, 24)) a href=(format!("/projects/{pid}")) style="text-decoration:none" { (p.name) } span.sep { "/" } span.muted { (truncate(&title, 60)) } }
            div.row { span.pill { span class=(format!("dot {}", if online { "ok" } else { "" })) {} (machine.clone().unwrap_or_else(|| "no machine".into())) @if online { " online" } @else { " offline" } } span.pill { @if p.auto_active() { "Auto relay" } @else { "Approval required" } } }
        }
        div.split style="padding-bottom:48px" {
            div.main.stack {
                @for e in &events {
                    @if !e.held || owner { (event_card(&app, &a, &p, &msgs, &atts, e, owner, &own_host)) }
                }
                @if let Some(m) = latest {
                    @if m.stage == "awaiting_sender" && !owner {
                        form.card method="post" action=(format!("/conversations/{cid}/answer")) {
                            (csrf(&a))
                            h3 { "Answer the agent's question" }
                            label.sr for="answer" { "Your answer" }
                            textarea id="answer" name="text" required maxlength="8000" {}
                            div.row { button.btn.primary type="submit" { (ic("send")) "Send answer" } }
                        }
                    }
                }
            }
            div.side.stack {
                @if let Some(s) = &status {
                    div.card {
                        span.muted style="font-size:13px" { @if owner { "Status for the tester's assistant" } @else { "Status" } }
                        dl.kv.mono style="font-size:12.5px" {
                            dt { "stage" } dd { (s["stage"].as_str().unwrap_or("")) }
                            @if let Some(d) = s["deployment_stage"].as_str() { dt { "deployment" } dd { (d) } }
                            dt { "next_action" } dd style="color:var(--amber)" { (s["next_action"].as_str().unwrap_or("")) }
                        }
                        div.inset { (s["summary"].as_str().unwrap_or("")) }
                    }
                }
                div.card {
                    span.muted style="font-size:13px" { "Route" }
                    dl.kv {
                        dt { "Project" } dd.row style="gap:8px" { (picon(&p.id, &p.name, &p.icon_etag, 18)) (p.name) }
                        dt { "Machine" } dd { (machine.unwrap_or_else(|| "none".into())) }
                        dt { "Pane" } dd.mono { (if p.pane_id.is_empty() { "none".to_string() } else { p.pane_id.clone() }) }
                        dt { "Agent" } dd { (logo(agent_logo(&p.agent), "")) " " (agent_label(&p.agent)) }
                        @if let Some(m) = msgs.first() { dt { "Sender" } dd { (m.sender_label) " · " (logo(client_logo(&m.client_name), "")) " " (m.client_name) } }
                    }
                }
                @if !p.gliph_channel.is_empty() {
                    div.card { div.row style="font-size:14px" { (gliph_mark(28)) "Linked to a Gliph channel" } }
                }
            }
        }
    }).into_string()).into_response())
}

#[allow(clippy::too_many_arguments)]
fn event_card(_app: &AppInner, a: &Account, p: &Project, msgs: &[Message], atts: &[Attachment], e: &Event, owner: bool, own_host: &str) -> Markup {
    let m = msgs.iter().find(|m| m.id == e.message_id);
    let my_atts: Vec<&Attachment> = atts.iter().filter(|x| x.message_id == e.message_id && ((e.actor_kind == "human" && x.origin == "tester") || (e.actor_kind == "agent" && x.origin == "agent" && e.data["attachments"].as_array().map(|l| l.iter().any(|v| v.as_str() == Some(&x.id))).unwrap_or(false)))).collect();
    let text = if e.actor_kind == "agent" || e.actor_kind == "deploy" { if owner { e.text.clone() } else { crate::safety::defang_links(&e.text, own_host, &p.retest_hosts) } } else { e.text.clone() };
    match e.actor_kind.as_str() {
        "human" => html! {
            article.card.ev {
                div.head {
                    div.who {
                        span.avatar { (initials(&e.actor_name)) @if let Some(m) = m { (logo(client_logo(&m.client_name), "")) } }
                        div { div style="font-weight:500" { (e.actor_name) span.role { " · " @if m.map(|m| m.sender_id == p.owner_id).unwrap_or(false) { "developer" } @else { "tester" } } }
                              @if let Some(m) = m { @if !m.client_name.is_empty() { div.role { "written by their assistant, " (m.client_name) } } } }
                    }
                    span.time { (hhmm(&e.created_at)) }
                }
                div.body style="font-size:16px" { (text) }
                @if !my_atts.is_empty() { div.row { @for x in &my_atts { a.inset href=(format!("/attachments/{}", x.id)) style="text-decoration:none;font-size:13px" { (x.name) span.faint { " " (human_size(x.size)) " · " (x.mime) } } } } }
            }
        },
        "system" => {
            let stage = e.data["stage"].as_str().unwrap_or("");
            let open = m.map(|m| m.stage == stage).unwrap_or(false);
            let attention = owner && open && (stage == "pending_approval" || stage == "held");
            html! {
                article class=(if attention { "card ev attention" } else { "card ev" }) {
                    div.head { span.who { span style="font-weight:500" { "Substrate" } @if !stage.is_empty() { (stage_tag(stage)) } } span.time { (hhmm(&e.created_at)) } }
                    div.body style="color:var(--text-2)" { (text) }
                    @if attention { @if let Some(m) = m {
                        @if !m.risk.is_empty() { div.tag.bad { "risk flags: " (m.risk) } }
                        p.muted style="margin:0;font-size:14px" { "Your agent gets this text:" }
                        div.envelope { (envelope_markup(&m.envelope)) }
                        div.row {
                            form.inline method="post" action=(format!("/messages/{}/approve", m.id)) data-confirm=[(!m.risk.is_empty()).then_some("This message matched a risk rule. Deliver it to your agent anyway?")] { (csrf(a)) button.btn.primary type="submit" { (ic("check")) "Approve" } }
                            form.inline method="post" action=(format!("/messages/{}/reject", m.id)) { (csrf(a)) button.btn type="submit" { (ic("x")) "Reject" } }
                        }
                    } }
                    @if owner && open { @if let Some(m) = m {
                        @if matches!(m.stage.as_str(), "approved" | "dispatched" | "accepted") { form.inline method="post" action=(format!("/messages/{}/cancel", m.id)) { (csrf(a)) button.btn.small.tertiary type="submit" { (ic("x")) "Cancel delivery" } } }
                        @if matches!(m.stage.as_str(), "uncertain" | "failed" | "interrupted" | "expired") { form.inline method="post" action=(format!("/messages/{}/retry", m.id)) data-confirm="Deliver it again? The agent may get it twice." { (csrf(a)) button.btn.small type="submit" { (ic("rotate")) "Deliver again" } } }
                    } }
                }
            }
        }
        "relay" => {
            let stage = e.data["stage"].as_str().unwrap_or("");
            html! {
                article.card.ev {
                    div.head { span.who { span style="font-weight:500" { "Local relay" } span.role.mono { (p.pane_id) } } span.time { (hhmm(&e.created_at)) } }
                    div.row { span class=(format!("tag {}", if matches!(stage, "uncertain" | "failed") { "bad" } else { "ok" })) { (text) } }
                    @if owner && stage == "working" { @if let Some(m) = m { @if m.agent_idle && !m.excerpt.is_empty() {
                        details { summary { "Terminal excerpt (only you see this)" } div.code style="margin-top:8px" { (m.excerpt) } }
                    } } }
                }
            }
        }
        "agent" => html! {
            article class=(if e.held { "card ev danger" } else { "card ev" }) {
                div.head {
                    span.who { (logo(agent_logo(&p.agent), "")) span style="font-weight:500" { (e.actor_name) } span.role { "agent · " (match e.kind.as_str() { "final" => "final reply", "question" => "question for the tester", "progress" => "progress", _ => "reply" }) } }
                    span.time { (hhmm(&e.created_at)) }
                }
                @if e.held {
                    div.tag.bad { "Held: looks like it contains " (e.data["secret_hits"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()) ". The tester can't see it." }
                }
                div.body { (text) }
                @if !my_atts.is_empty() { div.row { @for x in &my_atts { a.inset href=(format!("/attachments/{}", x.id)) style="text-decoration:none;font-size:13px" { (x.name) } } } }
                @if e.held && owner {
                    form method="post" action=(format!("/conversations/{}/release/{}", e.conversation_id, e.seq)) data-confirm="Show this reply to the tester as it is?" { (csrf(a)) button.btn.small type="submit" { (ic("eye")) "Release to tester" } }
                }
            }
        },
        "deploy" => {
            let d = &e.data["deploy"];
            let st = d["stage"].as_str().unwrap_or("");
            let order = ["fix_committed", "ci_running", "ci_passed", "deploying", "deployed", "verified", "ready_to_retest"];
            let at = order.iter().position(|s| *s == st);
            let failed = matches!(st, "ci_failed" | "deploy_failed" | "unknown");
            let ci = d["ci_url"].as_str().unwrap_or("");
            let verify = d["verify_url"].as_str().unwrap_or("");
            let gitlab = ci.contains("gitlab");
            html! {
                article.card.ev id="deploy" {
                    div.head { span.who { (logo(Some(if gitlab { "gitlab" } else { "github-actions" }), "")) span style="font-weight:500" { "CI and deploy" } span.role { "watched by the relay" } } span.time { (hhmm(&e.created_at)) } }
                    div.body { (text) }
                    @if !failed { div.deploy {
                        @for (i, s) in order.iter().enumerate() {
                            @if at.map(|a| i <= a).unwrap_or(false) { div class=(if *s == "ready_to_retest" { "on" } else { "" }) { span style=(if *s == "ready_to_retest" { "" } else { "color:var(--teal)" }) { "✓ " (s.replace('_', " ")) } } }
                        }
                    } } @else { div.tag.bad { (st.replace('_', " ")) } }
                    div.row style="font-size:14px" {
                        @if !ci.is_empty() && (owner || p.ci_public) { a href=(ci) rel="noopener noreferrer" target="_blank" { "CI run" } }
                        @if !verify.is_empty() && crate::safety::host_allowed(verify, own_host, &p.retest_hosts) { a href=(verify) rel="noopener noreferrer" target="_blank" { (verify) } }
                        @else if !verify.is_empty() && owner { span.faint { (verify) " (not on the retest allowlist; testers don't get this link)" } }
                    }
                }
            }
        }
        _ => html! {},
    }
}

/// The envelope with the untrusted block highlighted and the attachment folder
/// placeholder shown in words.
fn envelope_markup(env: &str) -> Markup {
    let env = env.replace(substrate_proto::ATTACH_DIR, "<attachment folder>");
    let start = env.find("<<<UNTRUSTED");
    let end = env.rfind("\nUNTRUSTED ");
    match (start, end) {
        (Some(s), Some(e)) if e > s => {
            let open_end = env[s..].find('\n').map(|i| s + i + 1).unwrap_or(s);
            html! { (env[..open_end].to_string()) span.untrusted { (env[open_end..e].to_string()) } (env[e..].to_string()) }
        }
        _ => html! { (env) },
    }
}

pub async fn message_action(State(app): State<App>, headers: HeaderMap, User(a): User, Path((mid, action)): Path<(String, String)>, Form(f): Form<CsrfOnly>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let m = domain::decide(&app, &a.id, &mid, &action).await?;
    Ok(Redirect::to(&format!("/conversations/{}", m.conversation_id)).into_response())
}

/// GET /approvals: every tester message waiting for the owner, grouped by who sent it
/// (an access link or an invited tester), with the exact prompt the agent will get.
pub async fn approvals(State(app): State<App>, User(a): User) -> Result<Response, Fail> {
    let rows = sqlx::query_as::<_, Message>(
        "SELECT m.* FROM messages m JOIN projects p ON p.id=m.project_id WHERE p.owner_id=$1 AND m.stage IN ('pending_approval','held') ORDER BY m.sender_id, m.order_n",
    ).bind(&a.id).fetch_all(&app.db).await?;
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1").bind(&a.id).fetch_all(&app.db).await?;
    let mut groups: Vec<(String, Vec<&Message>)> = vec![];
    for m in &rows {
        match groups.last_mut() {
            Some((s, l)) if *s == m.sender_id => l.push(m),
            _ => groups.push((m.sender_id.clone(), vec![m])),
        }
    }
    Ok(Html(layout("Approvals", Some(&a), Some("/live".into()), html! {
        div.narrow.stack style="padding:16px 0 48px" {
            h1 { "Approvals" }
            p.lead { "Each tester message waits here until you approve it. You see the exact prompt the agent gets." }
            @if groups.is_empty() { div.card { h3 { "Nothing waits" } p.muted { "New tester messages show up here." } } }
            @for (_, list) in &groups {
                section.list {
                    div.label { (list[0].sender_label) " · " (list.len()) @if list.len() == 1 { " message" } @else { " messages" } }
                    @for m in list {
                        @let p = projects.iter().find(|p| p.id == m.project_id);
                        div.item.approval {
                            div.stack style="gap:8px;min-width:0;flex:1" {
                                div.row {
                                    @if let Some(p) = p { (picon(&p.id, &p.name, &p.icon_etag, 22)) strong style="font-weight:500" { (p.name) } }
                                    span.faint style="font-size:13px" { (ago(&m.created_at)) " ago" }
                                    @if m.stage == "held" { span.tag.bad { "held · " (m.risk) } }
                                    @if m.envelope.contains("Deploy to production") { span.tag.warn { "deploys to production" } }
                                    @else if m.envelope.contains("Deploy to test") { span.tag.blue { "deploys to test" } }
                                }
                                div style="white-space:pre-wrap" { (m.text) }
                                details { summary.faint style="font-size:13px;cursor:pointer" { "The prompt the agent gets" } pre.envelope { (envelope_markup(&m.envelope)) } }
                            }
                            div.row style="flex:none;align-self:flex-start" {
                                form.inline method="post" action=(format!("/approvals/{}/approve", m.id)) { (csrf(&a)) button.btn.primary.small type="submit" { (ic("check")) "Approve" } }
                                form.inline method="post" action=(format!("/approvals/{}/reject", m.id)) { (csrf(&a)) button.btn.small.tertiary type="submit" { (ic("x")) "Reject" } }
                                a.btn.small.tertiary href=(format!("/conversations/{}", m.conversation_id)) { (ic("eye")) }
                            }
                        }
                    }
                }
            }
        }
    }).into_string()).into_response())
}

pub async fn approval_action(State(app): State<App>, headers: HeaderMap, User(a): User, Path((mid, action)): Path<(String, String)>, Form(f): Form<CsrfOnly>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    if !matches!(action.as_str(), "approve" | "reject") {
        return Err(Fail::new(404, "no such action"));
    }
    domain::decide(&app, &a.id, &mid, &action).await?;
    Ok(Redirect::to("/approvals").into_response())
}

pub async fn release_held(State(app): State<App>, headers: HeaderMap, User(a): User, Path((cid, seq)): Path<(String, i64)>, Form(f): Form<CsrfOnly>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let pid: String = sqlx::query_scalar("SELECT project_id FROM conversations WHERE id=$1").bind(&cid).fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "no such conversation"))?;
    domain::require_owner(&app, &pid, &a.id).await?;
    let e = sqlx::query_as::<_, Event>("UPDATE events SET held=false WHERE conversation_id=$1 AND seq=$2 AND held RETURNING *").bind(&cid).bind(seq).fetch_optional(&app.db).await?;
    if let Some(e) = e {
        audit(&app, &a.id, &pid, &a.label(), "reply.released", &e.message_id, &seq.to_string()).await;
        app.wake(&cid, seq);
    }
    Ok(Redirect::to(&format!("/conversations/{cid}")).into_response())
}

#[derive(Deserialize)]
pub struct AnswerForm {
    csrf: String,
    text: String,
}

pub async fn answer(State(app): State<App>, headers: HeaderMap, User(a): User, Path(cid): Path<String>, Form(f): Form<AnswerForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let m = sqlx::query_as::<_, Message>("SELECT * FROM messages WHERE conversation_id=$1 AND stage='awaiting_sender' ORDER BY order_n DESC LIMIT 1")
        .bind(&cid).fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(409, "the agent is not waiting for an answer"))?;
    if m.sender_id != a.id {
        return Err(Fail::new(403, "only the person the question was for can answer it"));
    }
    domain::set_stage(&app, &m.id, "answered").await?;
    domain::create_message(&app, domain::NewMessage {
        project_id: &m.project_id, sender_id: &a.id, sender_label: "", client_name: "Substrate web", conversation_id: Some(&cid),
        kind: "answer", text: &f.text, idem_key: "", attachment_ids: &[], deploy: None,
    }).await?;
    Ok(Redirect::to(&format!("/conversations/{cid}")).into_response())
}

pub async fn conversation_live(State(app): State<App>, User(a): User, Path(cid): Path<String>) -> Result<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>, Fail> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT project_id, sender_id FROM conversations WHERE id=$1").bind(&cid).fetch_optional(&app.db).await?;
    let Some((pid, sender)) = row else { return Err(Fail::new(404, "no such conversation")) };
    let p = domain::project(&app, &pid).await?;
    if p.owner_id != a.id && sender != a.id {
        return Err(Fail::new(404, "no such conversation"));
    }
    let rx = app.waiter(&cid);
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(seq) => Some((Ok(SseEvent::default().data(seq.to_string())), rx)),
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Some((Ok(SseEvent::default().data("lag")), rx)),
            Err(_) => None,
        }
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

// ---- files ----

pub fn signed_file_url(app: &AppInner, att: &str, account: &str) -> String {
    let exp = (now() + Duration::hours(1)).timestamp();
    let sig = app.sign(&format!("file:{att}:{account}:{exp}"));
    format!("{}/files/{att}?u={account}&exp={exp}&sig={sig}", app.cfg.public_url)
}

#[derive(Deserialize)]
pub struct FileQ {
    u: String,
    exp: i64,
    sig: String,
}

async fn serve_attachment(app: &AppInner, att: &Attachment) -> Result<Response, Fail> {
    let bytes = app.files.get(&format!("attachments/{}", att.id)).await?.ok_or_else(|| Fail::new(404, "gone"))?;
    let inline = att.mime.starts_with("image/");
    Ok((
        [
            (header::CONTENT_TYPE, if att.mime.starts_with("text/") { "text/plain; charset=utf-8".to_string() } else { att.mime.clone() }),
            (header::CONTENT_DISPOSITION, format!("{}; filename=\"{}\"", if inline { "inline" } else { "attachment" }, att.name)),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; sandbox".into()),
            (header::CACHE_CONTROL, "private, max-age=300".into()),
        ],
        bytes,
    ).into_response())
}

pub async fn file_signed(State(app): State<App>, Path(aid): Path<String>, Query(q): Query<FileQ>) -> Result<Response, Fail> {
    if q.exp < now().timestamp() || !app.verify(&format!("file:{aid}:{}:{}", q.u, q.exp), &q.sig) {
        return Err(Fail::new(403, "this link expired"));
    }
    let att = sqlx::query_as::<_, Attachment>("SELECT * FROM attachments WHERE id=$1").bind(&aid).fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "gone"))?;
    domain::role(&app, &att.project_id, &q.u).await?.ok_or_else(|| Fail::new(403, "no access"))?;
    serve_attachment(&app, &att).await
}

pub async fn file_session(State(app): State<App>, User(a): User, Path(aid): Path<String>) -> Result<Response, Fail> {
    let att = sqlx::query_as::<_, Attachment>("SELECT * FROM attachments WHERE id=$1").bind(&aid).fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "gone"))?;
    let p = domain::project(&app, &att.project_id).await?;
    let sender: Option<String> = sqlx::query_scalar("SELECT sender_id FROM conversations WHERE id=$1").bind(&att.conversation_id).fetch_optional(&app.db).await?;
    if p.owner_id != a.id && sender.as_deref() != Some(a.id.as_str()) && att.uploader_id != a.id {
        return Err(Fail::new(404, "gone"));
    }
    serve_attachment(&app, &att).await
}

// ---- upload links ----

async fn upload_token(app: &AppInner, t: &str) -> Result<(String, String), Fail> {
    let row: Option<(String, String)> = sqlx::query_as("SELECT account_id, ref FROM tokens WHERE hash=$1 AND kind='upload' AND expires_at > now()")
        .bind(hash(t)).fetch_optional(&app.db).await?;
    row.ok_or_else(|| Fail::new(404, "this upload link expired; ask your assistant for a new one"))
}

pub async fn upload_get(State(app): State<App>, Path(t): Path<String>) -> Result<Response, Fail> {
    let (account, pid) = upload_token(&app, &t).await?;
    let p = domain::project(&app, &pid).await?;
    let done: Vec<String> = sqlx::query_scalar("SELECT name FROM attachments WHERE project_id=$1 AND uploader_id=$2 AND message_id='' AND created_at > now() - interval '1 day'")
        .bind(&pid).bind(&account).fetch_all(&app.db).await?;
    Ok(Html(layout("Upload files", None, None, html! {
        div.narrow style="padding:48px 0" class="stack" {
            h1 { "Upload files for " span.grad { (p.name) } }
            p.lead { "Screenshots, PDFs, logs or text files, up to 10 MB each. Then tell your assistant they're uploaded." }
            form.card method="post" enctype="multipart/form-data" {
                label for="f" { "Files" }
                input id="f" type="file" name="file" multiple accept="image/png,image/jpeg,image/webp,application/pdf,text/plain,.md,.log,.txt" required;
                button.btn.primary type="submit" { (ic("upload")) "Upload" }
            }
            @if !done.is_empty() { div.card { h3 { "Uploaded" } @for n in &done { div.inset { (n) } } } }
        }
    }).into_string()).into_response())
}

pub async fn upload_post(State(app): State<App>, Path(t): Path<String>, mut mp: Multipart) -> Result<Response, Fail> {
    let (account, pid) = upload_token(&app, &t).await?;
    let mut n = 0;
    while let Some(field) = mp.next_field().await.map_err(|_| Fail::new(400, "bad upload"))? {
        let name = field.file_name().unwrap_or("file").to_string();
        let bytes = field.bytes().await.map_err(|_| Fail::new(413, "file too large"))?;
        if bytes.is_empty() {
            continue;
        }
        domain::store_attachment(&app, &pid, &account, "tester", &name, &bytes, "", "").await?;
        n += 1;
        if n >= 10 {
            break;
        }
    }
    Ok(Redirect::to(&format!("/upload/{t}")).into_response())
}

// ---- profile ----

/// GET /accounts/{id}/avatar: the uploaded picture, or a lettered circle.
pub async fn avatar(State(app): State<App>, Path(aid): Path<String>) -> Result<Response, Fail> {
    let row: Option<(Option<Vec<u8>>, String, String, String)> = sqlx::query_as("SELECT avatar, avatar_mime, name, email FROM accounts WHERE id=$1")
        .bind(&aid).fetch_optional(&app.db).await?;
    let Some((bytes, mime, name, email)) = row else { return Err(Fail::new(404, "no such account")) };
    let cache = (header::CACHE_CONTROL, "public, max-age=300".to_string());
    if let (Some(b), false) = (bytes, mime.is_empty()) {
        return Ok(([(header::CONTENT_TYPE, mime), cache], b).into_response());
    }
    let who = if name.is_empty() { email } else { name };
    let letter: String = who.chars().next().unwrap_or('?').to_uppercase().collect();
    let svg = format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><rect width="64" height="64" rx="32" fill="#2A2F55"/><text x="32" y="41" text-anchor="middle" font-family="system-ui,sans-serif" font-size="28" font-weight="600" fill="#F5F5F7">{}</text></svg>"##,
        maud::html! { (letter) }.into_string());
    Ok(([(header::CONTENT_TYPE, "image/svg+xml".to_string()), cache], svg).into_response())
}

pub async fn profile(State(app): State<App>, User(a): User, Query(f): Query<FlashQ>) -> Result<Response, Fail> {
    let _ = &app;
    Ok(Html(layout("Profile", Some(&a), None, html! {
        div.narrow.stack style="padding:16px 0 48px" {
            @if let Some(msg) = &f.flash { (notice("ok", html! { (msg) })) }
            h1 { "Profile" }
            form.card.stack method="post" action="/profile" enctype="multipart/form-data" {
                (csrf(&a))
                div.row style="gap:16px" {
                    img.avatar-img src=(format!("/accounts/{}/avatar?t={}", a.id, crate::app::now().timestamp())) alt="" width="64" height="64" style="width:64px;height:64px";
                    div.field style="flex:1" { label for="av" { "Picture" } input id="av" type="file" name="avatar" accept="image/png,image/jpeg,image/webp"; span.faint style="font-size:12px" { "PNG, JPEG or WebP, 1 MB at most. Testers see it next to your name." } }
                }
                div.field { label for="nm" { "Name" } input id="nm" type="text" name="name" value=(a.name) maxlength="64" placeholder=(a.label()); }
                div.row { button.btn.primary type="submit" { (ic("check")) "Save" } }
            }
        }
    }).into_string()).into_response())
}

pub async fn profile_post(State(app): State<App>, headers: HeaderMap, User(a): User, mut mp: Multipart) -> Result<Response, Fail> {
    let (mut token, mut name, mut pic): (String, Option<String>, Option<Vec<u8>>) = (String::new(), None, None);
    while let Some(field) = mp.next_field().await.map_err(|_| Fail::new(400, "bad form"))? {
        match field.name().unwrap_or("") {
            "csrf" => token = field.text().await.unwrap_or_default(),
            "name" => name = Some(field.text().await.unwrap_or_default()),
            "avatar" => { let b = field.bytes().await.map_err(|_| Fail::new(413, "picture too large"))?; if !b.is_empty() { pic = Some(b.to_vec()); } }
            _ => {}
        }
    }
    check_csrf(&app, &headers, &a, &token)?;
    if let Some(n) = name {
        sqlx::query("UPDATE accounts SET name=$2 WHERE id=$1").bind(&a.id).bind(substrate_proto::sanitize(n.trim(), 64)).execute(&app.db).await?;
    }
    if let Some(b) = pic {
        if b.len() > 1024 * 1024 {
            return Ok(Redirect::to("/profile?flash=The+picture+is+over+1+MB.").into_response());
        }
        let Some(mime) = crate::files::sniff(&b, "").filter(|m| m.starts_with("image/")) else {
            return Ok(Redirect::to("/profile?flash=Use+a+PNG,+JPEG+or+WebP+picture.").into_response());
        };
        let etag = hex::encode(&<sha2::Sha256 as sha2::Digest>::digest(&b)[..6]);
        sqlx::query("UPDATE accounts SET avatar=$2, avatar_mime=$3, avatar_etag=$4 WHERE id=$1").bind(&a.id).bind(&b).bind(mime).bind(etag).execute(&app.db).await?;
    }
    Ok(Redirect::to("/profile?flash=Saved.").into_response())
}

// ---- invitations ----

async fn invite_by_token(app: &AppInner, t: &str) -> Result<(String, String, String, String, i32, i32, bool, DateTime<Utc>), Fail> {
    let row: Option<(String, String, String, String, i32, i32, bool, DateTime<Utc>)> = sqlx::query_as(
        "SELECT i.id, i.project_id, p.name, p.owner_id, i.uses, i.max_uses, i.revoked, i.expires_at FROM invites i JOIN projects p ON p.id=i.project_id WHERE i.token_hash=$1",
    ).bind(hash(t)).fetch_optional(&app.db).await?;
    row.ok_or_else(|| Fail::new(404, "this invitation does not exist"))
}

pub async fn invite_get(State(app): State<App>, headers: HeaderMap, Path(t): Path<String>) -> Result<Response, Fail> {
    let (_, pid, name, owner, uses, max, revoked, exp) = invite_by_token(&app, &t).await?;
    let acct = session(&app, &headers).await;
    let owner_name = domain::account_label(&app, &owner).await;
    let p = domain::project(&app, &pid).await?;
    let dead = revoked || exp < now() || uses >= max;
    let already = match &acct { Some(a) => domain::role(&app, &pid, &a.id).await?.is_some(), None => false };
    Ok(Html(layout("Invitation", acct.as_ref(), None, html! {
        div.page-horizon {}
        div.narrow style="padding:40px 0 180px" class="stack" {
            (picon(&p.id, &p.name, &p.icon_etag, 72))
            h1 style="font-size:clamp(34px,5vw,46px)" { (owner_name) " invited you to test " span.grad { (name) } "." }
            @if !p.description.is_empty() { p.lead { (p.description) } }
            p.lead { "Tell your own AI assistant what you find. Your words go to the agent building " (name) ", and you hear when a fix is ready to try." }
            div.row { span.pill { "Send feedback and screenshots" } span.pill { "Read replies" } span.pill { "Expires " (exp.format("%b %d")) } }
            @if dead {
                div.card.danger { h3 { "This invitation can't be used" } p.muted { "It expired, was revoked, or has been used. Ask " (owner_name) " for a new one." } }
            } @else if already {
                div.card { h3 { "You're in" } p.muted { "You already have access to " (name) "." } a.btn.primary href="/connect" { (ic("plug")) "Connect your assistant" } }
            } @else if let Some(a) = &acct {
                form.card method="post" { (csrf(a)) div.row { span.badge { "1" } h3 { "Accept as " (a.email) } } button.btn.primary type="submit" { (ic("check")) "Accept invitation" } }
            } @else {
                div.card {
                    div.row { span.badge { "1" } h3 { "Accept with your email" } }
                    form method="post" action="/login" class="row" {
                        input type="hidden" name="next" value=(format!("/invites/{t}"));
                        label.sr for="em" { "Email" }
                        input id="em" type="email" name="email" required placeholder="you@example.com" style="flex:1 1 240px;width:auto";
                        button.btn.primary type="submit" { (ic("mail")) "Send sign-in link" }
                    }
                    a href=(format!("/auth/gliph?next=/invites/{t}")) class="row" style="font-size:14px;color:#BFC8FF;text-decoration:none" { (gliph_mark(18)) "Already on Gliph? Sign in with Gliph" }
                }
            }
            (assistant_steps(&app))
            p.faint style="font-size:13px" { "The developer sees your name, your assistant and your message before anything reaches their agent. You can leave at any time." }
        }
    }).into_string()).into_response())
}

fn assistant_steps(app: &AppInner) -> Markup {
    let url = format!("{}/mcp", app.cfg.public_url);
    html! {
        div.card {
            div.row { span.badge { "2" } h3 { "Add Substrate to your assistant" } }
            div.grid {
                div.inset.stack style="box-shadow:0 0 0 1px #D97757" { div.row style="font-weight:500" { img.logo.lg src="/static/logos/claude.svg" alt=""; "Claude" } span.muted style="font-size:13px" { "Settings › Connectors › Add custom connector. Paste the address and sign in. Available on Free (one connector), Pro, Max, Team and Enterprise." } }
                div.inset.stack { div.row style="font-weight:500" { img.logo.lg src="/static/logos/openai.svg" alt=""; "ChatGPT" } span.muted style="font-size:13px" { "Settings › Apps › Advanced settings › Developer mode, then create an app with this address. Business, Enterprise and Edu have it after an admin turns it on. Some Plus and Pro accounts have it too." } }
                div.inset.stack { div.row style="font-weight:500" { img.logo.lg src="/static/logos/copilot.svg" alt=""; "Other clients" } span.muted style="font-size:13px" { "Claude Code: " span.mono { "claude mcp add --transport http substrate " (url) } ". Others: OAuth, or a personal token from " a href="/connect" { "Connect" } "." } }
            }
            div.row { span.code style="flex:1 1 280px" { (url) } button.btn type="button" data-copy=(url) { (ic("copy")) "Copy" } }
        }
        div.card {
            div.row { span.badge { "3" } h3 { "Say it like you would to a person" } }
            div.inset style="font-size:16px" { "\u{201c}The settings page doesn't save my dark mode. Here's a screenshot. Can you tell the developer?\u{201d}" }
            span.muted { "Later, ask \u{201c}Any news on the settings bug?\u{201d} Your assistant checks and tells you when the fix is live." }
        }
    }
}

pub async fn invite_post(State(app): State<App>, headers: HeaderMap, User(a): User, Path(t): Path<String>, Form(f): Form<CsrfOnly>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let (iid, pid, name, _, _, _, _, _) = invite_by_token(&app, &t).await?;
    let n = sqlx::query("UPDATE invites SET uses=uses+1 WHERE id=$1 AND NOT revoked AND expires_at > now() AND uses < max_uses").bind(&iid).execute(&app.db).await?.rows_affected();
    if n != 1 {
        return Ok(error_page("Invitation unavailable", "It expired, was revoked, or has been used.").into_response());
    }
    sqlx::query("INSERT INTO members (project_id, account_id, role, invite_id) VALUES ($1,$2,'tester',$3) ON CONFLICT DO NOTHING")
        .bind(&pid).bind(&a.id).bind(&iid).execute(&app.db).await?;
    audit(&app, &a.id, &pid, &a.label(), "invite.accepted", &iid, &name).await;
    Ok(Redirect::to(&format!("/connect?flash={}", urlencoding::encode(&format!("You can now send messages to {name}.")))).into_response())
}

// ---- connect (MCP setup, personal tokens) ----

pub async fn connect(State(app): State<App>, User(a): User, Query(f): Query<FlashQ>) -> Result<Response, Fail> {
    let tokens: Vec<(String, String, DateTime<Utc>)> = sqlx::query_as("SELECT hash, ref, expires_at FROM tokens WHERE account_id=$1 AND kind='pat' AND expires_at > now() ORDER BY created_at DESC")
        .bind(&a.id).fetch_all(&app.db).await?;
    Ok(Html(layout("Connect an assistant", Some(&a), None, html! {
        div.narrow.stack style="padding:16px 0 48px" {
            @if let Some(msg) = &f.flash { (notice("ok", html! { (msg) })) }
            h1 { "Connect your " span.grad { "assistant" } }
            p.lead { "Add Substrate once. Then tell your assistant what you found." }
            (assistant_steps(&app))
            div.card {
                h3 { "Personal token" }
                p.muted style="margin:0" { "For clients that can't sign in with OAuth. Send it as " span.mono { "Authorization: Bearer <token>" } ". It expires in 90 days." }
                @if let Some(t) = &f.invite { div.secret { (t) } div.row { button.btn.small type="button" data-copy=(t) { (ic("copy")) "Copy token" } span.faint style="font-size:13px" { "Shown once." } } }
                form method="post" action="/connect/token" class="row" { (csrf(&a)) label.sr for="tl" { "Label" } input id="tl" type="text" name="label" placeholder="Label, e.g. Claude Code on laptop" style="flex:1 1 240px;width:auto" maxlength="64"; button.btn type="submit" { (ic("key")) "Create token" } }
                @for (h, label, exp) in &tokens {
                    div.inset.between { span { (if label.is_empty() { "token" } else { label }) span.faint { " · expires " (exp.format("%b %d")) } } form.inline method="post" action=(format!("/connect/token/{}/revoke", &h[..16])) { (csrf(&a)) button.btn.small.danger type="submit" { (ic("trash")) "Revoke" } } }
                }
            }
        }
    }).into_string()).into_response())
}

#[derive(Deserialize)]
pub struct PatForm {
    csrf: String,
    #[serde(default)]
    label: String,
}

pub async fn pat_create(State(app): State<App>, headers: HeaderMap, User(a): User, Form(f): Form<PatForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let t = crate::oauth::create_pat(&app, &a.id, &f.label).await?;
    Ok(Redirect::to(&format!("/connect?invite={t}")).into_response())
}

pub async fn pat_revoke(State(app): State<App>, headers: HeaderMap, User(a): User, Path(prefix): Path<String>, Form(f): Form<CsrfOnly>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    if prefix.len() == 16 && prefix.chars().all(|c| c.is_ascii_hexdigit()) {
        sqlx::query("DELETE FROM tokens WHERE account_id=$1 AND kind='pat' AND left(hash,16)=$2").bind(&a.id).bind(&prefix).execute(&app.db).await?;
        audit(&app, &a.id, "", &a.label(), "pat.revoked", &prefix, "").await;
    }
    Ok(Redirect::to("/connect?flash=Token+revoked.").into_response())
}

pub fn consent_page(a: &Account, client: &str, q: &AuthorizeQ, projects: &[(String, String, String)]) -> Html<String> {
    Html(layout("Allow access", Some(a), None, html! {
        div.narrow.stack style="padding:32px 0 48px" {
            div style="display:flex;justify-content:center;gap:18px;align-items:center;padding:12px 0" {
                @if let Some(l) = client_logo(client) { img src=(format!("/static/logos/{l}.svg")) alt="" width="48" height="48"; }
                span.faint { "→" }
                (orb("xl"))
            }
            h1 style="text-align:center" { "Allow " (client) " to use Substrate?" }
            div.card {
                p style="margin:0" { "Signed in as " strong { (a.email) } ". " (client) " will be able to:" }
                ul style="margin:0;padding-left:20px;color:var(--text-2)" {
                    li { "send messages and files to the projects you can test, which the developer approves" }
                    li { "read the status and replies of your own messages" }
                }
                p.muted style="margin:0;font-size:14px" { "It can't change any project, see other people's messages, or reach anyone's computer." }
                @if projects.is_empty() { div.inset { "You have no projects yet. Open an invitation link first." } }
                @else { div.stack { @for (n, h, r) in projects { div.inset.between { span { (n) span.faint.mono { " " (h) } } span.tag { (r) } } } } }
                form method="post" action="/oauth/authorize" class="row" {
                    (csrf(a))
                    @for (k, v) in [("response_type", &q.response_type), ("client_id", &q.client_id), ("redirect_uri", &q.redirect_uri), ("state", &q.state), ("code_challenge", &q.code_challenge), ("code_challenge_method", &q.code_challenge_method), ("scope", &q.scope), ("resource", &q.resource)] {
                        input type="hidden" name=(k) value=(v);
                    }
                    button.btn.primary type="submit" name="decision" value="allow" { (ic("check")) "Allow" }
                    button.btn.tertiary type="submit" name="decision" value="deny" { (ic("x")) "Cancel" }
                }
            }
        }
    }).into_string())
}

// ---- pairing ----

#[derive(Deserialize)]
pub struct PairQ {
    #[serde(default)]
    code: String,
}

pub async fn pair_get(State(app): State<App>, User(a): User, Query(q): Query<PairQ>) -> Result<Response, Fail> {
    let code = q.code.trim().to_uppercase();
    let row: Option<(String, String, String)> = sqlx::query_as("SELECT machine_name, platform, status FROM pairings WHERE user_code=$1 AND expires_at > now()")
        .bind(&code).fetch_optional(&app.db).await?;
    Ok(Html(layout("Pair a computer", Some(&a), None, html! {
        div.narrow.stack style="padding:32px 0 48px" {
            h1 { "Pair a " span.grad { "computer" } }
            @match row {
                Some((name, platform, status)) if status == "pending" => {
                    div.card.glow {
                        p style="margin:0" { "The relay on " strong { (name) } " (" (platform) ") wants to sign in as " strong { (a.email) } "." }
                        div.secret style="font-size:22px;text-align:center;letter-spacing:.2em" { (code) }
                        p.muted style="margin:0" { "Approve only if this code matches the one in your terminal." }
                        form method="post" class="row" { (csrf(&a)) input type="hidden" name="code" value=(code);
                            button.btn.primary type="submit" name="decision" value="approve" { (ic("check")) "Approve" }
                            button.btn type="submit" name="decision" value="deny" { (ic("x")) "Deny" } }
                    }
                }
                Some(_) => { div.card { p.muted { "This code was already used. Run " span.mono { "substrate login" } " again for a new one." } } }
                None => {
                    form.card method="get" { label for="code" { "Code from your terminal" } input id="code" type="text" name="code" placeholder="BCDF-GHJK" autocomplete="off"; button.btn.primary type="submit" { (ic("arrow-right")) "Continue" } }
                }
            }
        }
    }).into_string()).into_response())
}

#[derive(Deserialize)]
pub struct PairForm {
    csrf: String,
    code: String,
    decision: String,
}

pub async fn pair_post(State(app): State<App>, headers: HeaderMap, User(a): User, Form(f): Form<PairForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let status = if f.decision == "approve" { "approved" } else { "denied" };
    let n = sqlx::query("UPDATE pairings SET status=$2, account_id=$3 WHERE user_code=$1 AND status='pending' AND expires_at > now()")
        .bind(f.code.trim().to_uppercase()).bind(status).bind(&a.id).execute(&app.db).await?.rows_affected();
    if n != 1 {
        return Ok(error_page("Code expired", "Run substrate login again for a new code.").into_response());
    }
    audit(&app, &a.id, "", &a.label(), &format!("pairing.{status}"), &f.code, "").await;
    Ok(Html(layout("Paired", Some(&a), None, html! {
        div.narrow.stack style="padding:48px 0" { div.card { h1 { @if status == "approved" { "Paired" } @else { "Denied" } } p.muted { @if status == "approved" { "Go back to your terminal." } @else { "The relay can't sign in." } } } }
    }).into_string()).into_response())
}

// ---- machines & audit ----

pub async fn machines(State(app): State<App>, User(a): User) -> Result<Response, Fail> {
    let ms: Vec<(String, String, String, String, Option<DateTime<Utc>>, serde_json::Value)> = sqlx::query_as(
        "SELECT id, name, platform, version, last_seen, panes FROM machines WHERE account_id=$1 AND NOT revoked ORDER BY created_at",
    ).bind(&a.id).fetch_all(&app.db).await?;
    let projects = sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE owner_id=$1").bind(&a.id).fetch_all(&app.db).await?;
    Ok(Html(layout("Machines", Some(&a), Some("/live".into()), html! {
        div.stack style="padding:12px 0 48px" {
            h1 { "Machines" }
            @if ms.is_empty() { div.card { p.muted { "No computer is paired. Install the relay and run " span.mono { "substrate login" } "." } (cmd(&[&format!("curl -fsSL {}/install | sh", app.cfg.public_url), "substrate login"])) } }
            @for (mid, name, platform, version, seen, panes) in &ms {
                div.card {
                    div.between {
                        div.row { span class=(format!("dot {}", if app.machine_online(mid) { "ok" } else { "" })) {} h3 { (name) } span.faint { (platform) " · relay " (version) } }
                        form.inline method="post" action=(format!("/machines/{mid}/revoke")) data-confirm="Disconnect this computer? Its relay stops and its projects get no messages." { (csrf(&a)) button.btn.small.danger type="submit" { (ic("monitor-off")) "Disconnect" } }
                    }
                    span.muted style="font-size:13px" { @if app.machine_online(mid) { "Online now." } @else if let Some(s) = seen { "Last seen " (ago(s)) " ago." } @else { "Never connected." } }
                    @for (space, list) in spaces(panes) {
                        div.list style="padding:0;background:transparent" {
                            div.label { (space) }
                            @for pn in list {
                                @let pid = pn["pane_id"].as_str().unwrap_or("");
                                @let bound = projects.iter().find(|p| &p.machine_id == mid && p.pane_id == pid);
                                @let status = pn["status"].as_str().unwrap_or("");
                                @match bound {
                                    Some(p) => a.item href=(format!("/projects/{}", p.id)) {
                                        span.l { (picon(&p.id, &p.name, &p.icon_etag, 24)) strong style="font-weight:500" { (p.name) } (logo(agent_logo(pn["agent"].as_str().unwrap_or("")), "")) span.mono.faint { (pid) } }
                                        span.r { (status_tag(status)) }
                                    },
                                    None => div.item {
                                        span.l { (logo(agent_logo(pn["agent"].as_str().unwrap_or("")), "")) span { (pn["title"].as_str().unwrap_or("")) } span.mono.faint { (pid) } }
                                        span.r { (status_tag(status)) }
                                    },
                                }
                            }
                        }
                    }
                }
            }
        }
    }).into_string()).into_response())
}

/// Panes grouped by Herdr workspace, in Herdr's own order. An older relay
/// sends no label, so fall back to the id before the colon.
fn spaces(panes: &serde_json::Value) -> Vec<(String, Vec<&serde_json::Value>)> {
    let mut out: Vec<(String, Vec<&serde_json::Value>)> = vec![];
    for pn in panes.as_array().into_iter().flatten() {
        let ws = pn["workspace"].as_str().filter(|w| !w.is_empty()).map(str::to_string)
            .unwrap_or_else(|| pn["pane_id"].as_str().unwrap_or("").split(':').next().unwrap_or("").to_string());
        match out.iter_mut().find(|(w, _)| *w == ws) {
            Some((_, l)) => l.push(pn),
            None => out.push((ws, vec![pn])),
        }
    }
    out
}

pub async fn machine_revoke(State(app): State<App>, headers: HeaderMap, User(a): User, Path(mid): Path<String>, Form(f): Form<CsrfOnly>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    crate::relay::revoke_machine(&app, &a.id, &mid, "disconnected from the web").await?;
    Ok(Redirect::to("/machines").into_response())
}

pub async fn audit_page(State(app): State<App>, User(a): User) -> Result<Response, Fail> {
    let rows: Vec<(DateTime<Utc>, String, String, String, String, String)> = sqlx::query_as(
        "SELECT au.created_at, au.actor, au.action, au.target, au.detail, coalesce(p.name,'') FROM audit au LEFT JOIN projects p ON p.id=au.project_id
         WHERE au.account_id=$1 OR au.project_id IN (SELECT id FROM projects WHERE owner_id=$1) ORDER BY au.created_at DESC LIMIT 300",
    ).bind(&a.id).fetch_all(&app.db).await?;
    Ok(Html(layout("Audit log", Some(&a), None, html! {
        div.stack style="padding:12px 0 48px" {
            h1 { "Audit log" }
            p.muted { "Every approval, mode change, binding, invitation, sign-in, hold and release on your projects." }
            div.scroll-x { table.audit {
                @for (t, actor, action, target, detail, proj) in &rows {
                    tr { td.mono.faint style="white-space:nowrap" { (t.format("%b %d %H:%M:%S")) } td { (proj) } td { (actor) } td.mono { (action) } td.mono.faint { (target) } td.faint { (detail) } }
                }
            } }
        }
    }).into_string()).into_response())
}

pub async fn healthz(State(app): State<App>) -> Response {
    match sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(&app.db).await {
        Ok(_) => axum::Json(json!({"ok": true, "version": env!("CARGO_PKG_VERSION"), "relays": app.hub.lock().unwrap().len()})).into_response(),
        Err(_) => (axum::http::StatusCode::SERVICE_UNAVAILABLE, "db").into_response(),
    }
}

pub async fn install_sh(State(app): State<App>) -> Response {
    let s = include_str!("../../cli/install.sh").replace("__SUBSTRATE_URL__", &app.cfg.public_url);
    ([(header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], s).into_response()
}

pub async fn download(State(app): State<App>, Path(rest): Path<String>) -> Result<Response, Fail> {
    if rest.contains("..") || !rest.chars().all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c)) {
        return Err(Fail::new(404, "not found"));
    }
    let bytes = app.files.get(&format!("downloads/substrate/{rest}")).await?.ok_or_else(|| Fail::new(404, "not found"))?;
    let ct = if rest.ends_with(".sha256") || rest.ends_with(".json") { "text/plain; charset=utf-8" } else { "application/octet-stream" };
    Ok(([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "public, max-age=60")], bytes).into_response())
}

pub fn not_found() -> Html<String> {
    error_page("Not found", "There's nothing at this address.")
}
