//! The work log and live agents on the website (owner only), drawn from the same
//! JSON as the app API (`worklog`) with Substrate's components.

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::Form;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use maud::{html, Markup};
use serde::Deserialize;
use serde_json::Value;

use crate::app::{now, App, Fail};
use crate::auth::{check_csrf, Account, User};
use crate::web::{agent_logo, ago_text, cmd, csrf, ic, layout, logo, notice, status_tag, truncate};
use crate::worklog::{self, offset, ListQ};

// ---- small pieces ----

/// The reader's offset from UTC in minutes, from the `tz` cookie app.js sets.
fn tz(headers: &HeaderMap) -> i32 {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix("tz=").and_then(|v| v.parse().ok())).unwrap_or(0)
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}
fn n(v: &Value, k: &str) -> i64 {
    v[k].as_i64().unwrap_or(0)
}
fn when(v: &Value) -> Option<DateTime<Utc>> {
    v.as_str().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|d| d.with_timezone(&Utc))
}
fn clock(v: &Value, tz: i32) -> String {
    when(v).map(|t| t.with_timezone(&offset(tz)).format("%H:%M").to_string()).unwrap_or_default()
}
fn date_time(v: &Value, tz: i32) -> String {
    when(v).map(|t| t.with_timezone(&offset(tz)).format("%-d %b %H:%M").to_string()).unwrap_or_default()
}
fn ago_v(v: &Value) -> String {
    when(v).map(|t| ago_text(&t)).unwrap_or_default()
}

pub fn dur(ms: i64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}
pub fn toks(t: i64) -> String {
    match t {
        0..=999 => t.to_string(),
        1000..=999_999 => format!("{:.1}k", t as f64 / 1e3),
        _ => format!("{:.1}M", t as f64 / 1e6),
    }
}
fn money(v: &Value) -> String {
    v.as_f64().map(|c| if c < 0.01 && c > 0.0 { "<$0.01".to_string() } else { format!("${c:.2}") }).unwrap_or_else(|| "n/a".into())
}

fn day_label(day: &str, tz: i32) -> String {
    let today = now().with_timezone(&offset(tz)).date_naive();
    match NaiveDate::parse_from_str(day, "%Y-%m-%d") {
        Ok(d) if d == today => "Today".into(),
        Ok(d) if d == today - Duration::days(1) => "Yesterday".into(),
        Ok(d) => d.format("%A %-d %B").to_string(),
        Err(_) => day.into(),
    }
}

fn verdict_tag(v: &str) -> Markup {
    html! {
        @match v {
            "clean" => span.tag.ok { "clean" },
            "watch" => span.tag.warn { "watch" },
            "broken" => span.tag.bad { "broken" },
            _ => {},
        }
    }
}
fn impact_tag(v: &str) -> Markup {
    html! { span class=(format!("tag {}", match v { "high" => "bad", "medium" => "warn", _ => "off" })) { (v) } }
}
fn summary_tag(st: &str) -> Markup {
    html! {
        @match st {
            "queued" => span.tag { "summary queued" },
            "running" => span.tag.blue { "summarising" },
            "failed" => span.tag.bad { "summary failed" },
            _ => {},
        }
    }
}

/// A work-log project's logo, or its letter.
fn ricon(p: &Value, size: u32) -> Markup {
    let style = format!("width:{size}px;height:{size}px");
    html! {
        @match p["icon_url"].as_str() {
            Some(u) => img.picon src=(u) alt="" width=(size) height=(size) style=(style);,
            None => span.picon.tile style=(format!("{style};font-size:{}px", size * 9 / 20)) aria-hidden="true" { (s(p, "initial")) },
        }
    }
}

fn range_seg(base: &str, range: &str, extra: &str) -> Markup {
    html! {
        div.seg role="group" aria-label="Range" {
            @for (k, l) in [("today", "Today"), ("week", "7 days"), ("month", "30 days"), ("all", "All")] {
                a.seg-btn.on[range == k] href=(format!("{base}?range={k}{extra}")) aria-current=[(range == k).then_some("true")] { (l) }
            }
        }
    }
}

fn agent_mark(agent: &str) -> Markup {
    logo(agent_logo(agent), agent)
}

/// `[[word]]` from the search as highlighted text; everything else escaped.
fn snippet(t: &str) -> Markup {
    html! {
        @for (i, part) in t.split("[[").enumerate() {
            @if i == 0 { (part) } @else {
                @let (hit, rest) = part.split_once("]]").unwrap_or((part, ""));
                mark { (hit) } (rest)
            }
        }
    }
}

/// Where a form goes back to: a path on this site only.
fn back(b: &str, fallback: &str) -> String {
    if b.starts_with('/') && !b.starts_with("//") && !b.contains('\\') { b.to_string() } else { fallback.to_string() }
}

fn with_flash(to: &str, msg: &str) -> Response {
    let sep = if to.contains('?') { '&' } else { '?' };
    Redirect::to(&format!("{to}{sep}flash={}", urlencoding::encode(msg))).into_response()
}

#[derive(Deserialize, Default)]
pub struct Flash {
    flash: Option<String>,
}

fn flash_notice(f: &Option<String>) -> Markup {
    html! { @if let Some(m) = f { (notice("warn", html! { (m) })) } }
}

fn page(title: &str, a: &Account, body: Markup) -> Response {
    Html(layout(title, Some(a), Some("/live".into()), body).into_string()).into_response()
}

// ---- pages ----

/// GET /log: the overview.
pub async fn pulse_page(State(app): State<App>, headers: HeaderMap, User(a): User, Query(q): Query<ListQ>, Query(f): Query<Flash>) -> Result<Response, Fail> {
    let tz = tz(&headers);
    let range = match q.range.as_deref() { Some("today") => "today", Some("month") => "month", Some("all") => "all", _ => "week" };
    let p = worklog::pulse(&app, &a.id, range, tz).await?;
    let repos = worklog::repos_list(&app, &a.id, range, tz).await?;
    let t = &p["totals"];
    let w = &p["worker"];
    let bars = p["bars"].as_array().cloned().unwrap_or_default();
    let top = bars.iter().map(|b| n(b, "claude") + n(b, "codex")).max().unwrap_or(0).max(1);
    Ok(page("Log", &a, html! {
        div.stack style="padding:12px 0 48px" {
            div.between {
                h1 style="margin:0" { "Log" }
                form.row method="get" action="/log/search" role="search" {
                    input type="search" name="q" placeholder="Search prompts, summaries, files" aria-label="Search the log" style="min-width:220px";
                    button.btn.small type="submit" { (ic("search")) "Search" }
                }
            }
            (flash_notice(&f.flash))
            div.between { (range_seg("/log", range, "")) span.faint style="font-size:13px" { "Each Claude Code and Codex turn on your Macs, summarised on the Mac." } }
            @if n(t, "turns") == 0 && repos.as_array().map(|r| r.is_empty()).unwrap_or(true) {
                div.card {
                    h3 { "No turns yet" }
                    p.muted { "Add the work-log hooks on each Mac. The relay records every finished turn and writes its summary there." }
                    (cmd(&["substrate log install", "substrate log backfill --summarize --since 7d"]))
                }
            }
            div.grid.tight {
                div.card.stat { span.k { (ic("activity")) "Turns" } span.v { (n(t, "turns")) } span.faint { @for (k, v) in t["by_agent"].as_object().into_iter().flatten() { (k) " " (v) " " } } }
                div.card.stat { span.k { (ic("clock")) "Agent time" } span.v { (dur(n(t, "agent_ms"))) } }
                div.card.stat { span.k { (ic("zap")) "Tokens" } span.v { (toks(n(t, "tokens"))) } span.faint { (format!("{:.0}% from cache", t["cache_share"].as_f64().unwrap_or(0.0) * 100.0)) } }
                div.card.stat { span.k { (ic("globe")) "Estimated cost" } span.v { (money(&t["cost"])) } span.faint { "at API list prices, " (n(t, "priced")) " of " (n(t, "turns")) " priced" } }
                div.card.stat {
                    span.k { (ic("list")) "Summaries" }
                    span.v { (n(w, "written")) }
                    span.faint { (n(w, "queued")) " queued" @if let Some(d) = w["median_delay_s"].as_i64() { ", ready in " (dur(d * 1000)) } " " (money(&w["cost"])) }
                    @if n(w, "failed") > 0 {
                        form method="post" action="/log/jobs/retry-failed" { (csrf(&a)) button.btn.small type="submit" { (ic("rotate")) "Retry " (n(w, "failed")) " failed" } }
                    }
                }
            }
            @if n(t, "turns") > 0 {
                div.card {
                    div.bars role="img" aria-label="Tokens per period, Claude and Codex" {
                        @for b in &bars {
                            @let (c, x) = (n(b, "claude"), n(b, "codex"));
                            div.bar title=(format!("{}: Claude {}, Codex {}", s(b, "label"), toks(c), toks(x))) {
                                div.col { div.codex style=(format!("height:{}%", x * 100 / top)) {} div.claude style=(format!("height:{}%", c * 100 / top)) {} }
                                span.faint { (s(b, "label")) }
                            }
                        }
                    }
                    div.row.faint style="font-size:12px;gap:14px;margin-top:6px" { span.row { span.swatch.claude {} "Claude Code" } span.row { span.swatch.codex {} "Codex" } }
                }
            }
            @if let Some(att) = p["attention"].as_array().filter(|x| !x.is_empty()) {
                div.list {
                    div.label { (ic("alert")) "To check " span.badge { (n(&p, "open_watch")) } }
                    @for w in att { (watch_row(&a, w, "/log", true)) }
                }
            }
            @for d in p["days"].as_array().into_iter().flatten() {
                div.list {
                    div.label { (ic("clock")) (day_label(s(d, "day"), tz)) }
                    @for it in d["items"].as_array().into_iter().flatten() {
                        @let pr = &it["project"];
                        div.item style="align-items:flex-start" {
                            div.stack style="gap:6px;min-width:0;flex:1" {
                                a.row href=(format!("/log/repos/{}?range={range}", n(pr, "id"))) style="text-decoration:none;gap:10px" {
                                    (ricon(pr, 24)) strong style="font-weight:500" { (s(pr, "name")) }
                                    (verdict_tag(s(it, "health")))
                                    span.faint style="font-size:13px" { (n(it, "turns")) " turns" @if n(it, "files") > 0 { ", " (n(it, "files")) " files" } @if n(it, "pending") > 0 { ", " (n(it, "pending")) " summaries pending" } }
                                }
                                @for b in it["bullets"].as_array().into_iter().flatten() {
                                    a.row href=(format!("/log/turns/{}", n(b, "turn_id"))) style="gap:8px;font-size:14px;color:var(--text-2)" { span class=(format!("dot {}", match s(b, "verdict") { "broken" => "bad", "watch" => "warn", _ => "ok" })) {} span { (s(b, "text")) } }
                                }
                            }
                            span.r.faint style="font-size:13px" { (clock(&it["last_at"], tz)) }
                        }
                    }
                }
            }
            @if let Some(ds) = p["decisions"].as_array().filter(|x| !x.is_empty()) {
                div.list {
                    div.label { (ic("check")) "Decisions " span.badge { (n(&p, "decisions_count")) } }
                    @for d in ds {
                        a.item href=(format!("/log/turns/{}", n(d, "turn_id"))) { span.l { (ricon(&d["project"], 20)) span { (s(d, "text")) } } span.r.faint { (ago_v(&d["created_at"])) } }
                    }
                }
            }
            @if let Some(list) = repos.as_array().filter(|x| !x.is_empty()) {
                div.list {
                    div.label { (ic("folder")) "Projects" }
                    @for r in list {
                        a.item.link-row href=(format!("/log/repos/{}?range={range}", n(r, "id"))) {
                            span.l { (ricon(r, 24)) strong style="font-weight:500" { (s(r, "name")) } @if r["hidden"] == true { span.tag.off { "hidden" } } @if n(r, "open_watch") > 0 { span.tag.warn { (n(r, "open_watch")) " to check" } } }
                            span.r.faint style="font-size:13px" { (n(r, "turns")) " turns, " (money(&r["cost"])) @if !r["last_at"].is_null() { ", " (ago_v(&r["last_at"])) } }
                        }
                    }
                }
            }
        }
    }))
}

fn watch_row(a: &Account, w: &Value, back_to: &str, with_project: bool) -> Markup {
    let done = s(w, "status") == "done";
    html! {
        div.item {
            span.l style="min-width:0" {
                (impact_tag(s(w, "impact")))
                @if with_project { (ricon(&w["project"], 20)) }
                a href=(format!("/log/turns/{}", n(w, "turn_id"))) style=(if done { "text-decoration:line-through;color:var(--muted)" } else { "" }) { (s(w, "text")) }
            }
            form.inline method="post" action=(format!("/log/watch/{}/{}", n(w, "id"), if done { "open" } else { "done" })) {
                (csrf(a)) input type="hidden" name="back" value=(back_to);
                button.btn.small type="submit" { @if done { (ic("rotate")) "Reopen" } @else { (ic("check")) "Done" } }
            }
        }
    }
}

fn decision_row(a: &Account, d: &Value, back_to: &str) -> Markup {
    let active = s(d, "status") == "active";
    html! {
        div.item style="align-items:flex-start" {
            div.stack style="gap:4px;min-width:0" {
                a href=(format!("/log/turns/{}", n(d, "turn_id"))) style=(if active { "" } else { "color:var(--muted)" }) { (s(d, "text")) }
                @if !s(d, "why").is_empty() { span.faint style="font-size:13px" { "Why: " (s(d, "why")) } }
                @if !s(d, "alternative").is_empty() { span.faint style="font-size:13px" { "Not chosen: " (s(d, "alternative")) } }
                @if !active { span.tag.off { (s(d, "status")) } }
            }
            form.inline method="post" action=(format!("/log/decisions/{}/{}", n(d, "id"), if active { "dismiss" } else { "restore" })) {
                (csrf(a)) input type="hidden" name="back" value=(back_to);
                button.btn.small type="submit" { @if active { (ic("x")) "Dismiss" } @else { (ic("rotate")) "Restore" } }
            }
        }
    }
}

/// GET /log/repos/{id}: one project's turns, decisions and watch items.
pub async fn repo_page(State(app): State<App>, headers: HeaderMap, User(a): User, Path(id): Path<i64>, Query(q): Query<ListQ>, Query(f): Query<Flash>) -> Result<Response, Fail> {
    let tz = tz(&headers);
    let range = match q.range.as_deref() { Some("today") => "today", Some("month") => "month", Some("all") => "all", _ => "week" };
    let agent = q.agent.clone().unwrap_or_default();
    let only = q.only.clone().unwrap_or_default();
    let d = worklog::repo_detail(&app, &a.id, id, range, tz, Some(&agent), Some(&only)).await?;
    let p = &d["project"];
    let t = &d["totals"];
    let here = format!("/log/repos/{id}?range={range}&agent={agent}&only={only}");
    Ok(page(&format!("Log: {}", s(p, "name")), &a, html! {
        div.stack style="padding:12px 0 48px" {
            div.crumbs { a href="/log" style="text-decoration:none" { "Log" } span.sep { "/" } span.muted { (s(p, "name")) } }
            div.between {
                div.row style="gap:14px" {
                    (ricon(p, 44))
                    div {
                        h1 style="margin:0" { (s(p, "name")) }
                        div.faint style="font-size:13px" {
                            (n(&d, "total_turns")) " turns"
                            @if !d["first_at"].is_null() { " since " (date_time(&d["first_at"], tz)) }
                            @for m in d["machines"].as_array().into_iter().flatten() { ", " (s(m, "machine")) }
                        }
                    }
                }
                a.btn.small href=(format!("/log/repos/{id}/export")) { (ic("download")) "Export" }
            }
            (flash_notice(&f.flash))
            div.between {
                (range_seg(&format!("/log/repos/{id}"), range, &format!("&agent={agent}&only={only}")))
                div.seg role="group" aria-label="Filter" {
                    @for (k, l) in [("", "All agents"), ("claude", "Claude Code"), ("codex", "Codex")] {
                        a.seg-btn.on[agent == k] href=(format!("/log/repos/{id}?range={range}&agent={k}&only={only}")) { (l) }
                    }
                    a.seg-btn.on[only == "notes"] href=(format!("/log/repos/{id}?range={range}&agent={agent}&only={}", if only == "notes" { "" } else { "notes" })) { "With notes" }
                }
            }
            div.grid.tight {
                div.card.stat { span.k { "Turns" } span.v { (n(t, "turns")) } }
                div.card.stat { span.k { "Agent time" } span.v { (dur(n(t, "agent_ms"))) } }
                div.card.stat { span.k { "Tokens" } span.v { (toks(n(t, "tokens"))) } }
                div.card.stat { span.k { "Estimated cost" } span.v { (money(&t["cost"])) } }
            }
            div.list {
                div.label { (ic("activity")) "Turns" }
                @if d["turns"].as_array().map(|x| x.is_empty()).unwrap_or(true) { div.item { span.muted { "No turns in this range." } } }
                @for tr in d["turns"].as_array().into_iter().flatten() {
                    a.item.link-row href=(format!("/log/turns/{}", n(tr, "id"))) style="align-items:flex-start" {
                        span.l style="min-width:0;align-items:flex-start" {
                            span.faint.mono style="font-size:12px;padding-top:2px" { (date_time(&tr["started_at"], tz)) }
                            (agent_mark(s(tr, "agent")))
                            span.stack style="gap:3px;min-width:0" {
                                span { @if s(tr, "headline").is_empty() { (truncate(s(tr, "prompt"), 140)) } @else { (s(tr, "headline")) } }
                                span.row style="gap:6px;flex-wrap:wrap" {
                                    (verdict_tag(s(tr, "verdict"))) (summary_tag(s(tr, "summary_status")))
                                    @if n(tr, "decisions") > 0 { span.tag { (n(tr, "decisions")) " decisions" } }
                                    @if n(tr, "watch") > 0 { span.tag.warn { (n(tr, "watch")) " to check" } }
                                    @if let Some(fl) = tr["files"].as_array().filter(|x| !x.is_empty()) { span.tag { (fl.len()) " files" } }
                                }
                            }
                        }
                        span.r.faint style="font-size:13px;white-space:nowrap" { (dur(n(tr, "duration_ms"))) ", " (money(&tr["cost"])) }
                    }
                }
            }
            @if let Some(ws) = d["watch"].as_array().filter(|x| !x.is_empty()) {
                div.list { div.label { (ic("alert")) "To check" } @for w in ws { (watch_row(&a, w, &here, false)) } }
            }
            @if let Some(ds) = d["decisions"].as_array().filter(|x| !x.is_empty()) {
                div.list { div.label { (ic("check")) "Decisions" } @for x in ds { (decision_row(&a, x, &here)) } }
            }
            @if let Some(ss) = d["sessions"].as_array().filter(|x| !x.is_empty()) {
                div.list {
                    div.label { (ic("terminal")) "Sessions" }
                    @for x in ss {
                        div.item { span.l { (agent_mark(s(x, "agent"))) span.mono.faint { (truncate(s(x, "session_id"), 12)) } span { (s(x, "machine")) } @if x["active"] == true { span.tag.blue { "active" } } } span.r.faint { (ago_v(&x["last_at"])) } }
                    }
                }
            }
            div.card.stack {
                h3 style="margin:0" { "Settings" }
                form.row method="post" action=(format!("/log/repos/{id}/settings")) style="flex-wrap:wrap" {
                    (csrf(&a)) input type="hidden" name="back" value=(here);
                    label.field style="flex:1;min-width:200px" { span { "Name" } input type="text" name="name" value=(s(p, "name")) maxlength="100"; }
                    label.row style="gap:8px" { input type="checkbox" name="hidden" value="1" checked[p["hidden"] == true]; "Hide from the overview" }
                    button.btn.small type="submit" { (ic("check")) "Save" }
                }
                p.faint style="font-size:13px;margin:0" { "Repo " span.mono { (s(p, "key")) } }
            }
        }
    }))
}

/// GET /log/turns/{id}: one turn. Diffs, images and the tool log come from the Mac that recorded it.
pub async fn turn_page(State(app): State<App>, headers: HeaderMap, User(a): User, Path(id): Path<i64>, Query(f): Query<Flash>) -> Result<Response, Fail> {
    let tz = tz(&headers);
    let d = worklog::turn_detail(&app, &a.id, id, &format!("/log/turns/{id}/images")).await?;
    let t = &d["turn"];
    let p = &d["project"];
    let here = format!("/log/turns/{id}");
    let title = if s(t, "headline").is_empty() { truncate(s(t, "prompt"), 120) } else { s(t, "headline").to_string() };
    let diffs = &t["diffs"];
    Ok(page(&format!("Log: {}", s(p, "name")), &a, html! {
        div.stack style="padding:12px 0 48px" {
            div.crumbs {
                a href="/log" style="text-decoration:none" { "Log" } span.sep { "/" }
                a href=(format!("/log/repos/{}", n(p, "id"))) style="text-decoration:none" { (s(p, "name")) } span.sep { "/" }
                span.muted { "Turn " (n(&d["session"], "index")) " of " (n(&d["session"], "count")) " in its session" }
            }
            div.between style="align-items:flex-start" {
                div.stack style="gap:8px;min-width:0" {
                    h1 style="margin:0;font-size:26px" { (title) }
                    div.row style="gap:8px;flex-wrap:wrap" {
                        (verdict_tag(s(t, "verdict"))) (summary_tag(s(t, "summary_status")))
                        span.pill { (agent_mark(s(t, "agent"))) (if s(t, "model").is_empty() { s(t, "agent") } else { s(t, "model") }) }
                        span.pill { (ic("clock")) (date_time(&t["started_at"], tz)) ", " (dur(n(t, "duration_ms"))) }
                        span.pill { (ic("zap")) (toks(n(&t["tokens"], "input") + n(&t["tokens"], "output") + n(&t["tokens"], "cache_read") + n(&t["tokens"], "cache_write"))) " tokens, " (money(&t["cost"])) }
                        span.pill { (ic("monitor")) (s(t, "machine")) @if !s(t, "branch").is_empty() { ", " span.mono { (s(t, "branch")) } } }
                    }
                }
                div.row {
                    @if let Some(pv) = d["prev"].as_i64() { a.btn.small href=(format!("/log/turns/{pv}")) aria-label="Earlier turn" { (ic("arrow-left")) } }
                    @if let Some(nx) = d["next"].as_i64() { a.btn.small href=(format!("/log/turns/{nx}")) aria-label="Later turn" { (ic("arrow-right")) } }
                }
            }
            (flash_notice(&f.flash))
            @if let Some(sm) = t["summary"].as_array().filter(|x| !x.is_empty()) {
                div.card.stack { @for line in sm { p style="margin:0" { (line.as_str().unwrap_or("")) } } }
            } @else if s(t, "summary_status") == "failed" {
                (notice("bad", html! { "The summary failed: " (s(t, "summary_error")) }))
            }
            @if let Some(st) = t["steps"].as_array().filter(|x| !x.is_empty()) {
                div.list {
                    div.label { (ic("list")) "Steps" }
                    @for x in st {
                        div.item style="align-items:flex-start" {
                            span.l style="align-items:flex-start" {
                                span.mono.faint style="font-size:12px;padding-top:2px;min-width:44px" { (s(x, "at")) }
                                span class=(format!("dot {}", match s(x, "tone") { "good" => "ok", "problem" => "bad", _ => "" })) style="margin-top:7px" {}
                                span.stack style="gap:2px" { strong style="font-weight:500" { (s(x, "title")) } span.muted style="font-size:14px" { (s(x, "text")) } }
                            }
                        }
                    }
                }
            }
            @if let Some(ws) = d["watch"].as_array().filter(|x| !x.is_empty()) {
                div.list { div.label { (ic("alert")) "To check" } @for w in ws { (watch_row(&a, w, &here, false)) } }
            }
            @if let Some(ds) = d["decisions"].as_array().filter(|x| !x.is_empty()) {
                div.list { div.label { (ic("check")) "Decisions" } @for x in ds { (decision_row(&a, x, &here)) } }
            }
            div.card.stack {
                div.label style="padding:0" { (ic("message")) "Prompt" }
                div.code { (s(t, "prompt")) }
                @if !s(t, "final_text").is_empty() {
                    div.label style="padding:0" { (ic("send")) "Last reply" }
                    div.code style="max-height:480px;overflow:auto" { (s(t, "final_text")) }
                }
            }
            @if !d["local_error"].is_null() { (notice("warn", html! { (s(&d, "local_error")) })) }
            @let atts = d["attachments"].as_array().cloned().unwrap_or_default();
            @if !atts.is_empty() {
                div.list {
                    div.label { (ic("paperclip")) "Attachments" }
                    @for at in &atts {
                        div.item style="align-items:flex-start" {
                            span.l style="min-width:0;align-items:flex-start" {
                                span.tag { @if s(at, "dir") == "in" { "attached" } @else { "shared" } }
                                span.stack style="gap:6px;min-width:0" {
                                    @if !s(at, "url").is_empty() { a href=(s(at, "url")) rel="noopener noreferrer" target="_blank" { (if s(at, "name").is_empty() { s(at, "url") } else { s(at, "name") }) } }
                                    @else { span { (s(at, "name")) } }
                                    @if let Some(src) = at["src"].as_str() { img src=(src) alt=(s(at, "name")) loading="lazy" style="max-width:min(100%,520px);border-radius:10px"; }
                                }
                            }
                            @if n(at, "count") > 1 { span.r.faint { "published " (n(at, "count")) " times" } }
                        }
                    }
                }
            }
            @if let Some(fl) = t["files"].as_array().filter(|x| !x.is_empty()) {
                div.list {
                    div.label { (ic("folder")) "Files" }
                    @for file in fl {
                        @let path = s(file, "path");
                        @let diff = diffs[path].as_str();
                        details.item.diff-item {
                            summary.row style="gap:10px;cursor:pointer;min-width:0" {
                                span.mono style="font-size:13px;overflow-wrap:anywhere" { (path) }
                                span.mono style="font-size:12px;color:var(--teal)" { "+" (n(file, "add")) } span.mono style="font-size:12px;color:var(--red)" { "-" (n(file, "del")) }
                            }
                            @match diff {
                                Some(text) => pre.diff { @for l in text.lines() { span class=(match l.chars().next() { Some('+') => "add", Some('-') => "del", Some('@') => "hunk", _ => "" }) { (l) "\n" } } },
                                None => p.faint style="font-size:13px" { "The diff is on " (s(t, "machine")) "." },
                            }
                        }
                    }
                }
            }
            @if let Some(lg) = d["local"]["log"].as_str().filter(|x| !x.is_empty()) {
                details.card { summary style="cursor:pointer" { "Tool log" } pre.code style="max-height:560px;overflow:auto" { (lg) } }
            }
            div.card.stack {
                div.label style="padding:0" { (ic("terminal")) "Resume this session" }
                (cmd(&[s(&d, "resume")]))
                div.between {
                    span.faint style="font-size:13px" {
                        @match s(&d["job"], "status") {
                            "done" => { "Summary by " (s(&d["job"], "model")) ", " (toks(n(&d["job"], "tokens"))) " tokens, " (money(&d["job"]["cost"])) }
                            "skipped" => { "Imported without a summary." }
                            st => { "Summary: " (st) }
                        }
                    }
                    form.inline method="post" action=(format!("/log/turns/{id}/resummarize")) { (csrf(&a)) button.btn.small type="submit" { (ic("rotate")) "Summarise again" } }
                }
            }
        }
    }))
}

/// GET /log/search?q=
pub async fn search_page(State(app): State<App>, headers: HeaderMap, User(a): User, Query(q): Query<ListQ>) -> Result<Response, Fail> {
    let tz = tz(&headers);
    let query = q.q.clone().unwrap_or_default();
    let rows = worklog::search(&app, &a.id, &query).await?;
    Ok(page("Log", &a, html! {
        div.stack style="padding:12px 0 48px" {
            div.crumbs { a href="/log" style="text-decoration:none" { "Log" } span.sep { "/" } span.muted { "Search" } }
            form.row method="get" action="/log/search" role="search" {
                input type="search" name="q" value=(query) placeholder="Search prompts, summaries, files" aria-label="Search the log" style="flex:1" autofocus;
                button.btn.primary type="submit" { (ic("search")) "Search" }
            }
            @if !query.trim().is_empty() {
                div.list {
                    div.label { (rows.as_array().map(|r| r.len()).unwrap_or(0)) " turns" }
                    @if rows.as_array().map(|r| r.is_empty()).unwrap_or(true) { div.item { span.muted { "No turn matches." } } }
                    @for r in rows.as_array().into_iter().flatten() {
                        a.item.link-row href=(format!("/log/turns/{}", n(r, "id"))) style="align-items:flex-start" {
                            span.l style="min-width:0;align-items:flex-start" {
                                (ricon(&r["project"], 22))
                                span.stack style="gap:3px;min-width:0" {
                                    span { @if s(r, "headline").is_empty() { (truncate(s(r, "prompt"), 120)) } @else { (s(r, "headline")) } }
                                    span.faint style="font-size:13px" { (snippet(s(r, "snippet"))) }
                                }
                            }
                            span.r.faint style="font-size:13px;white-space:nowrap" { (date_time(&r["started_at"], tz)) }
                        }
                    }
                }
            }
        }
    }))
}

/// GET /agents: every agent on your online Macs; answer one that waits for you.
pub async fn agents_page(State(app): State<App>, headers: HeaderMap, User(a): User, Query(f): Query<Flash>) -> Result<Response, Fail> {
    let tz = tz(&headers);
    let v = worklog::agents(&app, &a.id, "/agents").await?;
    let list = v["agents"].as_array().cloned().unwrap_or_default();
    let mut list: Vec<&Value> = list.iter().collect();
    // Those that wait for you first.
    list.sort_by_key(|x| match s(x, "status") { "blocked" => 0, "done" | "idle" => 1, "working" => 2, _ => 3 });
    Ok(page("Agents", &a, html! {
        div.stack style="padding:12px 0 48px" {
            h1 style="margin:0" { "Agents" }
            (flash_notice(&f.flash))
            @if list.is_empty() {
                div.card { p.muted { "No agent runs on an online Mac. Start the relay with " span.mono { "substrate daemon install" } "." } }
            }
            @for x in list { (agent_card(&a, x, tz)) }
        }
    }))
}

fn agent_card(a: &Account, x: &Value, tz: i32) -> Markup {
    let base = format!("/agents/{}/{}", urlencoding::encode(s(x, "machine_id")), urlencoding::encode(s(x, "pane_id")));
    let name = x["project"]["name"].as_str().or(x["repo"]["name"].as_str()).unwrap_or(s(x, "title")).to_string();
    let status = s(x, "status");
    html! {
        div.card.stack.glow[status == "blocked"] {
            div.between {
                div.row style="gap:10px;min-width:0" {
                    @if !x["repo"].is_null() { (ricon(&x["repo"], 28)) } @else { span.picon.tile style="width:28px;height:28px;font-size:12px" { (name.chars().next().unwrap_or('?')) } }
                    (agent_mark(s(x, "agent")))
                    strong style="font-weight:500" { (name) }
                    (status_tag(status))
                    @if let Some(t) = when(&x["since"]) { span.faint style="font-size:13px" { "for " (ago_text(&t).trim_end_matches(" ago")) } }
                }
                span.faint style="font-size:13px" { (s(x, "machine")) ", " span.mono { (s(x, "pane_id")) } }
            }
            @if let Some(asks) = x["ask"].as_array() {
                @let ex = &x["ask_extras"];
                @if let Some(c) = ex["context"].as_str() { div.code style="max-height:240px;overflow:auto" { (c) } }
                @for at in ex["attachments"].as_array().into_iter().flatten() {
                    @if let Some(src) = at["src"].as_str() { img src=(src) alt=(s(at, "name")) loading="lazy" style="max-width:min(100%,520px);border-radius:10px"; }
                    @else if let Some(u) = at["url"].as_str() { a href=(u) rel="noopener noreferrer" target="_blank" { (s(at, "name")) } }
                    @else { span.faint { (s(at, "name")) } }
                }
                form.stack method="post" action=(format!("{base}/answer")) {
                    (csrf(a))
                    @for (i, q) in asks.iter().enumerate() {
                        fieldset.stack style="border:0;padding:0;margin:0;gap:8px" {
                            legend style="font-weight:500;padding:0 0 6px" { @if !s(q, "header").is_empty() { span.tag { (s(q, "header")) } " " } (s(q, "question")) }
                            @for o in q["options"].as_array().into_iter().flatten() {
                                label.row style="gap:10px;align-items:flex-start" {
                                    input type=(if q["multiSelect"] == true { "checkbox" } else { "radio" }) name=(format!("q{i}")) value=(s(o, "label")) required[q["multiSelect"] != true];
                                    span.stack style="gap:2px" { span { (s(o, "label")) } @if !s(o, "description").is_empty() { span.faint style="font-size:13px" { (s(o, "description")) } } }
                                }
                            }
                            input type="text" name=(format!("other{i}")) placeholder="Or write your own answer" maxlength="2000";
                        }
                    }
                    div.row { button.btn.primary type="submit" { (ic("send")) "Send answer" } }
                }
            } @else if let Some(sc) = x["screen"].as_object() {
                div.code style="max-height:320px;overflow:auto;white-space:pre" { (sc.get("text").and_then(Value::as_str).unwrap_or("")) }
                div.row style="flex-wrap:wrap" {
                    @for o in sc.get("options").and_then(Value::as_array).into_iter().flatten() {
                        form.inline method="post" action=(format!("{base}/keys")) {
                            (csrf(a)) input type="hidden" name="keys" value=(o["keys"].as_array().map(|k| k.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")).unwrap_or_default());
                            button.btn.small.primary[o["selected"] == true] type="submit" { (s(o, "label")) }
                        }
                    }
                    form.inline method="post" action=(format!("{base}/keys")) { (csrf(a)) input type="hidden" name="keys" value="esc"; button.btn.small type="submit" { "Esc" } }
                }
            }
            @if !x["last"].is_null() {
                @let l = &x["last"];
                div.stack style="gap:6px" {
                    @if let Some(q) = l["question"].as_str() { div.code { (q) } }
                    a href=(format!("/log/turns/{}", n(l, "turn_id"))) style="font-size:14px" {
                        @if let Some(h) = l["headline"].as_str() { (h) } @else { "Last turn" } " (" (clock(&l["ended_at"], tz)) ")"
                    }
                }
            }
            @if status != "working" && x["ask"].is_null() && x["screen"].is_null() {
                form.row method="post" action=(format!("{base}/prompt")) {
                    (csrf(a))
                    textarea name="text" rows="2" placeholder=(format!("A prompt for {name}")) required maxlength="20000" style="flex:1" {}
                    button.btn.primary type="submit" { (ic("send")) "Send" }
                }
            }
        }
    }
}

// ---- actions ----

#[derive(Deserialize)]
pub struct BackForm {
    csrf: String,
    #[serde(default)]
    back: String,
}

pub async fn watch_action(State(app): State<App>, headers: HeaderMap, User(a): User, Path((id, action)): Path<(i64, String)>, Form(f): Form<BackForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    worklog::set_watch(&app, &a.id, id, &action).await?;
    Ok(Redirect::to(&back(&f.back, "/log")).into_response())
}

pub async fn decision_action(State(app): State<App>, headers: HeaderMap, User(a): User, Path((id, action)): Path<(i64, String)>, Form(f): Form<BackForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    worklog::set_decision(&app, &a.id, id, &action, None).await?;
    Ok(Redirect::to(&back(&f.back, "/log")).into_response())
}

#[derive(Deserialize)]
pub struct RepoForm {
    csrf: String,
    #[serde(default)]
    back: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    hidden: Option<String>,
}

pub async fn repo_settings(State(app): State<App>, headers: HeaderMap, User(a): User, Path(id): Path<i64>, Form(f): Form<RepoForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    worklog::update_repo(&app, &a.id, id, Some(&f.name), Some(f.hidden.is_some()), false).await?;
    Ok(Redirect::to(&back(&f.back, &format!("/log/repos/{id}"))).into_response())
}

pub async fn resummarize(State(app): State<App>, headers: HeaderMap, User(a): User, Path(id): Path<i64>, Form(f): Form<BackForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let to = format!("/log/turns/{id}");
    Ok(match worklog::resummarize(&app, &a.id, id).await {
        Ok(()) => Redirect::to(&to).into_response(),
        Err(e) => with_flash(&to, &e.1),
    })
}

pub async fn retry_failed(State(app): State<App>, headers: HeaderMap, User(a): User, Form(f): Form<BackForm>) -> Result<Response, Fail> {
    check_csrf(&app, &headers, &a, &f.csrf)?;
    let r = worklog::retry_failed(&app, &a.id).await?;
    Ok(if n(&r, "offline") > 0 { with_flash("/log", &format!("{} summaries wait for a Mac that is offline.", n(&r, "offline"))) } else { Redirect::to("/log").into_response() })
}

pub async fn export(State(app): State<App>, User(a): User, Path(id): Path<i64>) -> Result<Response, Fail> {
    let v = worklog::export(&app, &a.id, id).await?;
    Ok(([(header::CONTENT_TYPE, "application/json".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"worklog-project-{id}.json\""))],
        serde_json::to_string_pretty(&v).unwrap()).into_response())
}

pub async fn turn_image(State(app): State<App>, User(a): User, Path((id, k)): Path<(i64, u32)>) -> Result<Response, Fail> {
    worklog::turn_image_bytes(&app, &a.id, id, k).await
}

pub async fn ask_file(State(app): State<App>, User(a): User, Path((machine, pane, k)): Path<(String, String, u32)>) -> Result<Response, Fail> {
    worklog::ask_file(&app, &a.id, &machine, &pane, k).await
}

/// POST /agents/{machine}/{pane}/{keys|prompt|answer}. An answer to an AskUserQuestion
/// goes in as a prompt: the relay cancels the dialog first.
pub async fn agent_action(State(app): State<App>, headers: HeaderMap, User(a): User, Path((machine, pane, action)): Path<(String, String, String)>, Form(f): Form<Vec<(String, String)>>) -> Result<Response, Fail> {
    let get = |k: &str| f.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str()).unwrap_or("");
    check_csrf(&app, &headers, &a, get("csrf"))?;
    let r = match action.as_str() {
        "keys" => worklog::agent_keys(&app, &a.id, &machine, &pane, &get("keys").split_whitespace().map(str::to_string).collect::<Vec<_>>()).await,
        "prompt" => worklog::agent_prompt(&app, &a.id, &machine, &pane, get("text")).await,
        "answer" => {
            let v = worklog::agents(&app, &a.id, "/agents").await?;
            let agent = v["agents"].as_array().into_iter().flatten().find(|x| s(x, "machine_id") == machine && s(x, "pane_id") == pane).cloned();
            let asks = agent.as_ref().and_then(|x| x["ask"].as_array().cloned()).ok_or_else(|| Fail::new(409, "the question is gone"))?;
            let text = answer_text(&asks, &f);
            worklog::agent_prompt(&app, &a.id, &machine, &pane, &text).await
        }
        _ => return Err(Fail::new(404, "keys, prompt or answer")),
    };
    Ok(match r {
        Ok(()) => Redirect::to("/agents").into_response(),
        Err(e) => with_flash("/agents", &e.1),
    })
}

/// The answers as one prompt: "<question>: <choices>" per question; an own answer wins.
fn answer_text(asks: &[Value], f: &[(String, String)]) -> String {
    let lines: Vec<String> = asks.iter().enumerate().filter_map(|(i, q)| {
        let other = f.iter().find(|(k, _)| *k == format!("other{i}")).map(|(_, v)| v.trim()).filter(|v| !v.is_empty());
        let picked: Vec<&str> = f.iter().filter(|(k, _)| *k == format!("q{i}")).map(|(_, v)| v.as_str()).collect();
        let ans = other.map(str::to_string).or_else(|| (!picked.is_empty()).then(|| picked.join(", ")))?;
        Some(if asks.len() == 1 { ans } else { format!("{}: {ans}", if s(q, "header").is_empty() { s(q, "question") } else { s(q, "header") }) })
    }).collect();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn answers_and_back() {
        let asks = vec![json!({"question": "Which DB?", "header": "DB"}), json!({"question": "Deploy?"})];
        let f = vec![("q0".into(), "Postgres".into()), ("q1".into(), "Yes".into()), ("other1".into(), "Tomorrow".into())];
        assert_eq!(answer_text(&asks, &f), "DB: Postgres\nDeploy?: Tomorrow");
        assert_eq!(answer_text(&asks[..1], &f), "Postgres");
        assert_eq!(back("//evil.com", "/log"), "/log");
        assert_eq!(back("/log/repos/1?range=week", "/log"), "/log/repos/1?range=week");
        assert_eq!(dur(65_000), "1m 5s");
        assert_eq!(toks(1500), "1.5k");
    }
}
