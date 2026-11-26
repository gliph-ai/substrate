//! The work log, ported from Theasa: each Claude Code and Codex turn on the
//! developer's Macs, with a summary, decisions, watch items, tokens and an
//! estimated cost. The relay reads the turns and writes the summaries on the Mac
//! (proto `LogBatch`, `LogSummaryReport`); the cloud keeps no diffs, images or
//! tool log and asks the recording Mac's relay for them (`rpc`). Live agents:
//! what each blocked agent asks, and the owner's answer, keys or prompt.
//!
//! The JSON here is what the app API serves (shapes kept close to Theasa's, so
//! Sidra's screens port by swapping the client) and what `logweb` renders.

use std::collections::{BTreeMap, HashMap};
use std::sync::RwLock;

use axum::body::Bytes;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{header, HeaderMap};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use chrono::{DateTime, Datelike, Duration, FixedOffset, TimeZone, Timelike, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use substrate_proto::{self as proto, LogBatch, LogSummaryReport, Pane, Tokens, TurnRef};

use crate::app::{audit, now, App, AppInner, Fail};
use crate::appapi::bearer;
use crate::relay::machine_auth;

// ---------- prices (LiteLLM's public table, in memory) ----------

#[derive(Clone, Copy)]
struct Price {
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

static PRICES: RwLock<Option<HashMap<String, Price>>> = RwLock::new(None);
const PRICE_URL: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// Load LiteLLM's price table (the JSON as published).
pub fn set_prices(v: &Value) {
    let mut m = HashMap::new();
    for (k, p) in v.as_object().into_iter().flatten() {
        if let Some(i) = p["input_cost_per_token"].as_f64() {
            m.insert(k.clone(), Price { input: i, output: p["output_cost_per_token"].as_f64().unwrap_or(0.0),
                cache_read: p["cache_read_input_token_cost"].as_f64(), cache_write: p["cache_creation_input_token_cost"].as_f64() });
        }
    }
    *PRICES.write().unwrap() = Some(m);
}

/// Estimated cost at API list price; None for a model the table does not know.
pub fn cost_of(model: &str, t: &Tokens) -> Option<f64> {
    let g = PRICES.read().unwrap();
    let table = g.as_ref()?;
    let m = model.trim_start_matches("anthropic/").trim_start_matches("openai/");
    if m.is_empty() {
        return None;
    }
    let undated = match m.rsplit_once('-') { Some((a, d)) if d.len() == 8 && d.chars().all(|c| c.is_ascii_digit()) => a, _ => m };
    let unbracketed = m.split('[').next().unwrap_or(m);
    let tries = [m.to_string(), format!("anthropic/{m}"), format!("openai/{m}"), undated.to_string(), unbracketed.to_string()];
    let p = tries.iter().find_map(|k| table.get(k))?;
    Some(t.input as f64 * p.input + t.output as f64 * p.output
        + t.cache_read as f64 * p.cache_read.unwrap_or(p.input * 0.1)
        + t.cache_write as f64 * p.cache_write.unwrap_or(p.input * 1.25))
}

/// Fetch the table at boot and daily; then price the turns stored before it was there.
pub fn spawn_prices(app: App) {
    tokio::spawn(async move {
        loop {
            let got = async { reqwest::get(PRICE_URL).await?.error_for_status()?.json::<Value>().await }.await;
            let wait = match got {
                Ok(v) => {
                    set_prices(&v);
                    if let Err(e) = fill_costs(&app).await {
                        tracing::warn!(error = %e.1, "pricing stored turns");
                    }
                    24 * 3600
                }
                Err(e) => {
                    tracing::warn!(error = %e, "price table");
                    3600
                }
            };
            tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        }
    });
}

pub async fn fill_costs(app: &AppInner) -> Result<(), Fail> {
    // pages by id, so rows of models with no price never block the rows after them
    let mut after = 0i64;
    loop {
        let rows: Vec<(i64, String, i64, i64, i64, i64)> = sqlx::query_as("SELECT id, model, in_tok, out_tok, cache_read, cache_write FROM log_turns WHERE cost IS NULL AND model <> '' AND id > $1 ORDER BY id LIMIT 5000")
            .bind(after).fetch_all(&app.db).await?;
        let Some(last) = rows.last().map(|r| r.0) else { return Ok(()) };
        after = last;
        for (id, model, i, o, cr, cw) in rows {
            if let Some(c) = cost_of(&model, &Tokens { input: i as u64, output: o as u64, cache_read: cr as u64, cache_write: cw as u64 }) {
                sqlx::query("UPDATE log_turns SET cost=$2 WHERE id=$1").bind(id).bind(c).execute(&app.db).await?;
            }
        }
    }
}

// ---------- time: the cloud runs UTC, days are the reader's ----------

/// `tz` is minutes east of UTC (the browser's `-getTimezoneOffset()`).
fn offset(tz: i32) -> FixedOffset {
    FixedOffset::east_opt(tz.clamp(-840, 840) * 60).unwrap_or(FixedOffset::east_opt(0).unwrap())
}

/// The reader's time zone: an IANA name (each date gets its own offset, so a daylight
/// saving change is right), or a fixed offset in minutes east of UTC.
#[derive(Clone, Copy, Debug)]
pub enum Zone {
    Minutes(i32),
    Named(chrono_tz::Tz),
}

impl Zone {
    pub fn parse(s: &str) -> Zone {
        let s = s.trim();
        s.parse::<chrono_tz::Tz>().map(Zone::Named).unwrap_or_else(|_| Zone::Minutes(s.parse().unwrap_or(0)))
    }
    /// The moment as the reader's wall clock shows it.
    pub fn at(&self, t: &DateTime<Utc>) -> DateTime<FixedOffset> {
        match self {
            Zone::Minutes(m) => t.with_timezone(&offset(*m)),
            Zone::Named(z) => t.with_timezone(z).fixed_offset(),
        }
    }
    /// The start of a local day.
    pub fn midnight(&self, d: chrono::NaiveDate) -> DateTime<Utc> {
        let n = d.and_hms_opt(0, 0, 0).unwrap();
        match self {
            Zone::Minutes(m) => offset(*m).from_local_datetime(&n).earliest().map(|t| t.with_timezone(&Utc)),
            Zone::Named(z) => z.from_local_datetime(&n).earliest().map(|t| t.with_timezone(&Utc)),
        }.unwrap_or_else(now)
    }
}

pub fn since_for(range: &str, tz: Zone) -> DateTime<Utc> {
    let days = match range { "today" => 1, "month" => 30, "all" => 36500, _ => 7 };
    tz.midnight(tz.at(&now()).date_naive() - Duration::days(days - 1))
}

pub fn local_day(t: &DateTime<Utc>, tz: Zone) -> String {
    tz.at(t).format("%Y-%m-%d").to_string()
}

fn norm_range(r: Option<&str>) -> &'static str {
    match r { Some("today") => "today", Some("month") => "month", Some("all") => "all", _ => "week" }
}

// ---------- rows ----------

#[derive(sqlx::FromRow, Clone)]
pub struct RepoRow {
    pub id: i64,
    pub key: String,
    pub name: String,
    pub display_name: String,
    pub root: String,
    pub remote: String,
    pub hidden: bool,
    pub icon_etag: String,
    pub icon_locked: bool,
}

const REPO_COLS: &str = "id, key, name, display_name, root, remote, hidden, icon_etag, icon_locked";

impl RepoRow {
    pub fn label(&self) -> &str {
        if self.display_name.is_empty() { &self.name } else { &self.display_name }
    }
}

#[derive(sqlx::FromRow, Clone)]
pub struct TurnRow {
    pub id: i64,
    pub repo_id: i64,
    pub machine_id: String,
    pub machine_name: String,
    pub agent: String,
    pub session_id: String,
    pub turn_key: String,
    pub cwd: String,
    pub branch: String,
    pub head: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub tool_ms: i64,
    pub prompt: String,
    pub final_text: String,
    pub model: String,
    pub in_tok: i64,
    pub out_tok: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub model_calls: i32,
    pub tools: Value,
    pub files: Value,
    pub attachments: Value,
    pub cost: Option<f64>,
    pub headline: String,
    pub summary: Value,
    pub verdict: String,
    pub steps: Value,
    pub summary_status: String,
    pub summary_error: String,
    pub summary_model: String,
    pub summary_tokens: i64,
    pub summary_cost: Option<f64>,
    pub summary_at: Option<DateTime<Utc>>,
}

impl TurnRow {
    fn tref(&self) -> TurnRef {
        TurnRef { agent: self.agent.clone(), session_id: self.session_id.clone(), turn_key: self.turn_key.clone() }
    }
}

const TURN_COLS: &str = "id, repo_id, machine_id, machine_name, agent, session_id, turn_key, cwd, branch, head, started_at, ended_at, duration_ms, tool_ms, prompt, final_text, model, in_tok, out_tok, cache_read, cache_write, model_calls, tools, files, attachments, cost, headline, summary, verdict, steps, summary_status, summary_error, summary_model, summary_tokens, summary_cost, summary_at";

pub fn repo_json(app: &AppInner, r: &RepoRow) -> Value {
    json!({"id": r.id, "key": r.key, "name": r.label(), "repo_name": r.name, "root": r.root, "remote": r.remote, "hidden": r.hidden,
        "has_icon": !r.icon_etag.is_empty(), "icon_locked": r.icon_locked,
        "icon_url": if r.icon_etag.is_empty() { Value::Null } else { json!(format!("{}/log/repos/{}/icon?v={}", app.cfg.public_url, r.id, r.icon_etag)) },
        "initial": r.label().chars().next().unwrap_or('?').to_uppercase().to_string()})
}

fn turn_json(t: &TurnRow) -> Value {
    let att = t.attachments.as_array().cloned().unwrap_or_default();
    json!({"id": t.id, "agent": t.agent, "session_id": t.session_id, "project_id": t.repo_id, "machine": t.machine_name, "machine_id": t.machine_id,
        "cwd": t.cwd, "branch": t.branch, "head": t.head, "started_at": t.started_at, "ended_at": t.ended_at, "duration_ms": t.duration_ms, "tool_ms": t.tool_ms,
        "prompt": t.prompt, "model": t.model,
        "tokens": {"input": t.in_tok, "output": t.out_tok, "cache_read": t.cache_read, "cache_write": t.cache_write},
        "model_calls": t.model_calls, "cost": t.cost, "tools": t.tools, "files": t.files,
        "headline": t.headline, "summary": t.summary, "verdict": if t.verdict.is_empty() { Value::Null } else { json!(t.verdict) }, "steps": t.steps,
        "summary_status": t.summary_status, "summary_error": if t.summary_error.is_empty() { Value::Null } else { json!(t.summary_error) },
        "att_in": att.iter().filter(|a| a["dir"] == "in").count(), "att_out": att.iter().filter(|a| a["dir"] == "out").count()})
}

async fn repos_of(app: &AppInner, owner: &str) -> Result<HashMap<i64, RepoRow>, Fail> {
    let rows = sqlx::query_as::<_, RepoRow>(&format!("SELECT {REPO_COLS} FROM log_repos WHERE owner_id=$1")).bind(owner).fetch_all(&app.db).await?;
    Ok(rows.into_iter().map(|r| (r.id, r)).collect())
}

async fn owned_repo(app: &AppInner, owner: &str, id: i64) -> Result<RepoRow, Fail> {
    sqlx::query_as::<_, RepoRow>(&format!("SELECT {REPO_COLS} FROM log_repos WHERE id=$1 AND owner_id=$2")).bind(id).bind(owner)
        .fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "no such project"))
}

pub async fn owned_turn(app: &AppInner, owner: &str, id: i64) -> Result<TurnRow, Fail> {
    sqlx::query_as::<_, TurnRow>(&format!("SELECT {TURN_COLS} FROM log_turns WHERE id=$1 AND owner_id=$2")).bind(id).bind(owner)
        .fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "no such turn"))
}

/// "canvas.json" is a file name; "Plan v1.2" is a title.
fn is_file_name(n: &str) -> bool {
    !n.contains(char::is_whitespace) && n.rsplit_once('.').map(|(_, e)| (1..=6).contains(&e.len()) && e.chars().next().unwrap().is_ascii_alphabetic() && e.chars().all(|c| c.is_ascii_alphanumeric())).unwrap_or(false)
}

/// Rebuild a turn's search text: prompt, summary, decisions, watch items, files, reply.
async fn reindex<'e, E: sqlx::PgExecutor<'e>>(db: E, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE log_turns t SET search = to_tsvector('simple', concat_ws(' ', t.prompt, t.headline, t.summary::text, left(t.final_text, 2000),
          (SELECT string_agg(f->>'path', ' ') FROM jsonb_array_elements(t.files) f),
          (SELECT string_agg(d.text || ' ' || d.why, ' ') FROM log_decisions d WHERE d.turn_id = t.id),
          (SELECT string_agg(w.text, ' ') FROM log_watch w WHERE w.turn_id = t.id))) WHERE t.id = $1")
        .bind(id).execute(db).await?;
    Ok(())
}

/// Cut to microseconds, as Postgres stores it: a repeat with nanoseconds would read as newer.
fn ts(s: &str) -> Option<DateTime<Utc>> {
    use chrono::DurationRound;
    DateTime::parse_from_rfc3339(s).ok().and_then(|d| d.with_timezone(&Utc).duration_trunc(chrono::Duration::microseconds(1)).ok())
}

fn clip(s: &str, n: usize) -> String {
    proto::sanitize(s, n)
}

// ---------- relay → cloud ----------

/// POST /relay/log/turns: one repo's finished turns.
pub async fn ingest(State(app): State<App>, headers: HeaderMap, Json(b): Json<LogBatch>) -> Result<Json<Value>, Fail> {
    let (account, machine) = machine_auth(&app, &headers).await?;
    let r = ingest_batch(&app, &account, &machine, &b).await?;
    app.changed(&account);
    Ok(Json(r))
}

pub async fn ingest_batch(app: &AppInner, account: &str, machine: &str, b: &LogBatch) -> Result<Value, Fail> {
    if b.repo.key.trim().is_empty() || b.repo.key.len() > 500 || b.turns.len() > 200 {
        return Err(Fail::new(400, "a repo key and at most 200 turns"));
    }
    let mname: String = sqlx::query_scalar("SELECT name FROM machines WHERE id=$1").bind(machine).fetch_one(&app.db).await?;
    let mut tx = app.db.begin().await?;
    let name = clip(if b.repo.name.is_empty() { &b.repo.key } else { &b.repo.name }, 200);
    let rid: i64 = sqlx::query_scalar("INSERT INTO log_repos (owner_id, key, name, root, remote) VALUES ($1,$2,$3,$4,$5)
        ON CONFLICT (owner_id, key) DO UPDATE SET name=excluded.name, root=COALESCE(NULLIF(excluded.root,''), log_repos.root), remote=COALESCE(NULLIF(excluded.remote,''), log_repos.remote)
        RETURNING id").bind(account).bind(&b.repo.key).bind(&name).bind(clip(&b.repo.root, 1000)).bind(clip(&b.repo.remote, 500)).fetch_one(&mut *tx).await?;
    let (mut stored, mut updated) = (0, 0);
    for t in &b.turns {
        let (Some(start), Some(end)) = (ts(&t.started_at), ts(&t.ended_at)) else { continue };
        if t.id.session_id.is_empty() || t.id.turn_key.is_empty() || t.id.agent.is_empty() || t.id.agent.len() > 20 {
            continue;
        }
        // Artifacts link out only to claude.ai. A publish from a file only knows the file
        // name; an earlier publish of the same page may know its title.
        let mut atts = t.attachments.clone();
        for a in atts.iter_mut() {
            a.name = clip(&a.name, 300);
            if !a.url.starts_with("https://claude.ai/") {
                a.url.clear();
            }
            if a.kind == "artifact" && !a.url.is_empty() && (a.name.is_empty() || is_file_name(&a.name)) {
                let names: Vec<String> = sqlx::query_scalar("SELECT a->>'name' FROM log_turns t, jsonb_array_elements(t.attachments) a
                    WHERE t.owner_id=$1 AND t.attachments @> jsonb_build_array(jsonb_build_object('kind', 'artifact', 'url', $2::text))
                    AND a->>'kind'='artifact' AND a->>'url'=$2 AND a->>'name' <> ''").bind(account).bind(&a.url).fetch_all(&mut *tx).await?;
                if let Some(n) = names.into_iter().find(|n| !is_file_name(n)) {
                    a.name = n;
                }
            }
        }
        // queued: a summary follows (and replaces the old one, keeping the state of identical items).
        // Anything else means none follows for this content: the old summary is cleared below.
        let status = if matches!(t.summary_status.as_str(), "queued" | "running") { "queued" } else { "skipped" };
        let cost = cost_of(&t.model, &t.tokens);
        let old: Option<(i64, DateTime<Utc>, i32, String)> = sqlx::query_as("SELECT id, ended_at, model_calls, summary_status FROM log_turns WHERE owner_id=$1 AND agent=$2 AND session_id=$3 AND turn_key=$4")
            .bind(account).bind(&t.id.agent).bind(&t.id.session_id).bind(&t.id.turn_key).fetch_optional(&mut *tx).await?;
        let mut updated_now = false;
        let tid = match old {
            // newer wins; at the same end time more model calls is the turn going on. An older copy never overwrites.
            Some((id, old_end, calls, st)) if !(end > old_end || (end == old_end && t.model_calls as i32 > calls)) => {
                // unchanged, but imported without a summary and now one is asked for
                if status == "queued" && (st == "skipped" || st == "none") {
                    sqlx::query("UPDATE log_turns SET summary_status='queued', summary_error='' WHERE id=$1").bind(id).execute(&mut *tx).await?;
                }
                continue;
            }
            Some((id, ..)) => {
                updated += 1;
                updated_now = true;
                id
            }
            None => {
                stored += 1;
                sqlx::query_scalar("INSERT INTO log_turns (owner_id, repo_id, agent, session_id, turn_key, started_at, ended_at) VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id")
                    .bind(account).bind(rid).bind(&t.id.agent).bind(&t.id.session_id).bind(&t.id.turn_key).bind(start).bind(end).fetch_one(&mut *tx).await?
            }
        };
        sqlx::query("UPDATE log_turns SET repo_id=$2, machine_id=$3, machine_name=$4, cwd=$5, branch=$6, head=$7, started_at=$8, ended_at=$9, duration_ms=$10, tool_ms=$11,
              prompt=$12, final_text=$13, model=$14, in_tok=$15, out_tok=$16, cache_read=$17, cache_write=$18, model_calls=$19, tools=$20, files=$21, attachments=$22,
              cost=$23, summary_status=$24, summary_error='' WHERE id=$1")
            .bind(tid).bind(rid).bind(machine).bind(&mname).bind(clip(&t.cwd, 1000)).bind(clip(&b.repo.branch, 200)).bind(clip(&b.repo.head, 64)).bind(start).bind(end)
            .bind(t.duration_ms as i64).bind(t.tool_ms as i64).bind(clip(&t.prompt, 100_000)).bind(clip(&t.final_text, 100_000)).bind(clip(&t.model, 100))
            .bind(t.tokens.input as i64).bind(t.tokens.output as i64).bind(t.tokens.cache_read as i64).bind(t.tokens.cache_write as i64).bind(t.model_calls as i32)
            .bind(sqlx::types::Json(&t.tools)).bind(sqlx::types::Json(&t.files)).bind(sqlx::types::Json(&atts)).bind(cost).bind(status)
            .execute(&mut *tx).await?;
        if updated_now {
            // the turn changed: its old summary no longer describes it
            sqlx::query("UPDATE log_turns SET headline='', summary='[]', verdict='', steps='[]' WHERE id=$1").bind(tid).execute(&mut *tx).await?;
        }
        if updated_now && status == "skipped" {
            // no new summary follows, so the notes go too. When one follows, its items replace
            // these and keep the done, dismissed or superseded state of identical text.
            sqlx::query("DELETE FROM log_watch WHERE turn_id=$1").bind(tid).execute(&mut *tx).await?;
            sqlx::query("UPDATE log_decisions SET status='active', superseded_by=NULL WHERE superseded_by IN (SELECT id FROM log_decisions WHERE turn_id=$1)").bind(tid).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM log_decisions WHERE turn_id=$1").bind(tid).execute(&mut *tx).await?;
        }
        reindex(&mut *tx, tid).await?;
    }
    tx.commit().await?;
    Ok(json!({"stored": stored, "updated": updated}))
}

/// POST /relay/log/summary: a summary job's state, or the summary itself.
pub async fn summary(State(app): State<App>, headers: HeaderMap, Json(r): Json<LogSummaryReport>) -> Result<Json<Value>, Fail> {
    let (account, _) = machine_auth(&app, &headers).await?;
    store_summary(&app, &account, &r).await?;
    app.changed(&account);
    Ok(Json(json!({"ok": true})))
}

pub async fn store_summary(app: &AppInner, account: &str, r: &LogSummaryReport) -> Result<(), Fail> {
    let row: Option<(i64, i64, DateTime<Utc>, i32)> = sqlx::query_as("SELECT id, repo_id, ended_at, model_calls FROM log_turns WHERE owner_id=$1 AND agent=$2 AND session_id=$3 AND turn_key=$4")
        .bind(account).bind(&r.turn.agent).bind(&r.turn.session_id).bind(&r.turn.turn_key).fetch_optional(&app.db).await?;
    let Some((tid, rid, ended, calls)) = row else { return Err(Fail::new(404, "no such turn")) };
    // a summary of an older copy of the turn: the newer copy's own job replaces it
    if r.status == "done" && (ts(&r.ended_at).is_some_and(|e| e != ended) || (r.model_calls > 0 && r.model_calls as i32 != calls)) {
        return Ok(());
    }
    // Every write below takes the report only when it is not older than the last one taken
    // (summary_gen): a late "running" never covers a newer "done".
    let gen = r.gen as i64;
    let err = clip(&r.error, 1000);
    match (r.status.as_str(), &r.summary) {
        ("queued" | "running" | "failed", _) => {
            sqlx::query("UPDATE log_turns SET summary_status=$2, summary_error=$3, summary_gen=$4 WHERE id=$1 AND summary_gen <= $4").bind(tid).bind(&r.status).bind(&err).bind(gen).execute(&app.db).await?;
            return Ok(());
        }
        ("done", Some(_)) => {}
        _ => return Err(Fail::new(400, "status is queued, running, failed, or done with a summary")),
    }
    let s = r.summary.as_ref().unwrap();
    let c = |x: &str| clip(x, 2000);
    let summary: Vec<String> = s.summary.iter().map(|x| c(x)).filter(|x| !x.is_empty()).take(6).collect();
    let watch: Vec<(String, &str)> = s.watch.iter().map(|w| (c(&w.text), match w.impact.as_str() { "high" => "high", "low" => "low", _ => "medium" })).filter(|w| !w.0.is_empty()).take(30).collect();
    let decisions: Vec<(String, String, String)> = s.decisions.iter().map(|d| (c(&d.text), c(&d.why), c(&d.alternative))).filter(|d| !d.0.is_empty()).take(30).collect();
    let steps: Vec<Value> = s.steps.iter().filter(|x| !x.title.trim().is_empty()).take(8).map(|x| json!({"at": clip(&x.at, 12), "title": c(&x.title), "text": c(&x.text),
        "tone": match x.tone.as_str() { "good" => "good", "problem" => "problem", _ => "neutral" }})).collect();
    let mut verdict = match s.verdict.as_str() { "watch" => "watch", "broken" => "broken", _ => "clean" };
    if verdict == "clean" && !watch.is_empty() {
        verdict = "watch";
    }
    let headline = { let h = c(&s.headline); if h.is_empty() { summary.first().cloned().unwrap_or_default() } else { h } };
    let summary = if summary.is_empty() && !headline.is_empty() { vec![headline.clone()] } else { summary };
    let tok = (r.tokens.input + r.tokens.output + r.tokens.cache_read + r.tokens.cache_write) as i64;
    let cost = r.cost.or_else(|| cost_of(&r.model, &r.tokens));
    let mut tx = app.db.begin().await?;
    let took = sqlx::query("UPDATE log_turns SET headline=$2, summary=$3, verdict=$4, steps=$5, summary_status='done', summary_error='', summary_model=$6, summary_tokens=$7, summary_cost=$8, summary_at=now(), summary_gen=$9
        WHERE id=$1 AND summary_gen <= $9")
        .bind(tid).bind(&headline).bind(sqlx::types::Json(&summary)).bind(verdict).bind(sqlx::types::Json(&steps)).bind(clip(&r.model, 100)).bind(tok).bind(cost).bind(gen).execute(&mut *tx).await?.rows_affected();
    if took == 0 {
        return Ok(()); // a newer report came first; the transaction rolls back unused
    }
    // A redo keeps each item whose text is the same (its id, and its done or dismissed state),
    // adds the new ones and removes the ones that are gone.
    let old_w: Vec<(i64, String)> = sqlx::query_as("SELECT id, text FROM log_watch WHERE turn_id=$1").bind(tid).fetch_all(&mut *tx).await?;
    let old_d: Vec<(i64, String)> = sqlx::query_as("SELECT id, text FROM log_decisions WHERE turn_id=$1").bind(tid).fetch_all(&mut *tx).await?;
    let gone_w: Vec<i64> = old_w.iter().filter(|o| !watch.iter().any(|w| w.0 == o.1)).map(|o| o.0).collect();
    let gone_d: Vec<i64> = old_d.iter().filter(|o| !decisions.iter().any(|d| d.0 == o.1)).map(|o| o.0).collect();
    sqlx::query("DELETE FROM log_watch WHERE id = ANY($1)").bind(&gone_w).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM log_decisions WHERE id = ANY($1)").bind(&gone_d).execute(&mut *tx).await?;
    sqlx::query("UPDATE log_decisions SET status='active', superseded_by=NULL WHERE superseded_by = ANY($1)").bind(&gone_d).execute(&mut *tx).await?;
    for (text, impact) in &watch {
        match old_w.iter().find(|w| &w.1 == text) {
            Some(o) => sqlx::query("UPDATE log_watch SET impact=$2 WHERE id=$1").bind(o.0).bind(impact).execute(&mut *tx).await?,
            None => sqlx::query("INSERT INTO log_watch (turn_id, repo_id, owner_id, text, impact, created_at) VALUES ($1,$2,$3,$4,$5,$6)")
                .bind(tid).bind(rid).bind(account).bind(text).bind(impact).bind(ended).execute(&mut *tx).await?,
        };
    }
    for (text, why, alt) in &decisions {
        match old_d.iter().find(|d| &d.1 == text) {
            Some(o) => sqlx::query("UPDATE log_decisions SET why=$2, alternative=$3 WHERE id=$1").bind(o.0).bind(why).bind(alt).execute(&mut *tx).await?,
            None => sqlx::query("INSERT INTO log_decisions (turn_id, repo_id, owner_id, text, why, alternative, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7)")
                .bind(tid).bind(rid).bind(account).bind(text).bind(why).bind(alt).bind(ended).execute(&mut *tx).await?,
        };
    }
    reindex(&mut *tx, tid).await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct IconReq {
    key: String,
    #[serde(default)]
    data: String,
}

/// POST /relay/log/icon {key, mime, data}: a repo's logo the relay found (raster only).
pub async fn relay_icon(State(app): State<App>, headers: HeaderMap, Json(r): Json<IconReq>) -> Result<Json<Value>, Fail> {
    let (account, _) = machine_auth(&app, &headers).await?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(r.data.trim()).map_err(|_| Fail::new(400, "data is base64"))?;
    let n = set_icon(&app, &account, IconTarget::Key(&r.key), &bytes, false).await?;
    Ok(Json(json!({"updated": n})))
}

enum IconTarget<'a> {
    Key(&'a str),
    Id(i64),
}

async fn set_icon(app: &AppInner, owner: &str, target: IconTarget<'_>, bytes: &[u8], lock: bool) -> Result<u64, Fail> {
    if bytes.is_empty() || bytes.len() > 1_000_000 {
        return Err(Fail::new(413, "an icon is an image under 1 MB"));
    }
    let mime = match crate::files::sniff(bytes, "icon") {
        Some(m @ ("image/png" | "image/jpeg" | "image/webp")) => m,
        _ => return Err(Fail::new(415, "an icon is a PNG, JPEG or WebP")),
    };
    let etag = crate::app::hash(&hex::encode(bytes))[..16].to_string();
    let (cond, key, id) = match target { IconTarget::Key(k) => ("key=$6 AND NOT icon_locked", k.to_string(), 0), IconTarget::Id(id) => ("id=$7", String::new(), id) };
    let n = sqlx::query(&format!("UPDATE log_repos SET icon=$2, icon_mime=$3, icon_etag=$4, icon_locked = icon_locked OR $5 WHERE owner_id=$1 AND {cond} AND $6::text IS NOT NULL AND $7::bigint IS NOT NULL"))
        .bind(owner).bind(bytes).bind(mime).bind(&etag).bind(lock).bind(key).bind(id).execute(&app.db).await?.rows_affected();
    app.changed(owner);
    Ok(n)
}

/// GET /log/repos/{id}/icon: public, like project icons (Sidra's image views send no token).
pub async fn repo_icon(State(app): State<App>, Path(id): Path<i64>) -> Result<Response, Fail> {
    let row: Option<(Option<Vec<u8>>, String)> = sqlx::query_as("SELECT icon, icon_mime FROM log_repos WHERE id=$1").bind(id).fetch_optional(&app.db).await?;
    match row {
        Some((Some(b), mime)) if !mime.is_empty() => Ok(raster(mime, b, "public, max-age=86400")),
        _ => Err(Fail::new(404, "no icon")),
    }
}

/// Image bytes that can never run script on this origin.
pub fn raster(mime: String, bytes: Vec<u8>, cache: &'static str) -> Response {
    ([(header::CONTENT_TYPE, mime), (header::CACHE_CONTROL, cache.to_string()), (header::X_CONTENT_TYPE_OPTIONS, "nosniff".into()),
        (header::CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox".into())], bytes).into_response()
}

const RASTER: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// A relay's `{media_type, data}` as an image response, raster types only.
fn relay_image(v: &Value) -> Result<Response, Fail> {
    let mt = v["media_type"].as_str().unwrap_or("");
    if !RASTER.contains(&mt) {
        return Err(Fail::new(404, "no image"));
    }
    let b = base64::engine::general_purpose::STANDARD.decode(v["data"].as_str().unwrap_or("")).map_err(|_| Fail::new(502, "the relay sent a broken image"))?;
    Ok(raster(mt.to_string(), b, "private, max-age=3600"))
}

// ---------- reads ----------

const RANK: &[(&str, i32)] = &[("broken", 3), ("watch", 2), ("clean", 1)];
fn rank(v: &str) -> i32 {
    RANK.iter().find(|r| r.0 == v).map(|r| r.1).unwrap_or(0)
}

#[derive(sqlx::FromRow)]
struct PulseRow {
    id: i64,
    repo_id: i64,
    agent: String,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    duration_ms: i64,
    in_tok: i64,
    out_tok: i64,
    cache_read: i64,
    cache_write: i64,
    cost: Option<f64>,
    headline: String,
    verdict: String,
    files: Value,
    summary_status: String,
    n_dec: i64,
    n_watch: i64,
}

async fn worker_stats(app: &AppInner, owner: &str, since: DateTime<Utc>) -> Result<Value, Fail> {
    let (written, tokens, cost): (i64, i64, f64) = sqlx::query_as("SELECT count(*), COALESCE(sum(summary_tokens),0)::bigint, COALESCE(sum(summary_cost),0)::float8
        FROM log_turns WHERE owner_id=$1 AND summary_status='done' AND summary_at >= $2").bind(owner).bind(since).fetch_one(&app.db).await?;
    let (queued, running, failed): (i64, i64, i64) = sqlx::query_as("SELECT count(*) FILTER (WHERE summary_status='queued'), count(*) FILTER (WHERE summary_status='running'),
        count(*) FILTER (WHERE summary_status='failed') FROM log_turns WHERE owner_id=$1").bind(owner).fetch_one(&app.db).await?;
    let median: Option<f64> = sqlx::query_scalar("SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY extract(epoch FROM summary_at - ended_at))::float8 FROM log_turns
        WHERE owner_id=$1 AND summary_status='done' AND summary_at >= $2 AND summary_at - ended_at BETWEEN interval '0' AND interval '1 day'").bind(owner).bind(since).fetch_one(&app.db).await?;
    Ok(json!({"written": written, "tokens": tokens, "cost": cost, "queued": queued, "running": running, "failed": failed, "median_delay_s": median.map(|m| m.round() as i64)}))
}

/// The overview: totals, bars, each day's work per project, what needs attention.
pub async fn pulse(app: &AppInner, owner: &str, range: &str, tz: Zone) -> Result<Value, Fail> {
    let since = since_for(range, tz);
    let repos = repos_of(app, owner).await?;
    let turns = sqlx::query_as::<_, PulseRow>("SELECT t.id, t.repo_id, t.agent, t.started_at, t.ended_at, t.duration_ms, t.in_tok, t.out_tok, t.cache_read, t.cache_write, t.cost,
        t.headline, t.verdict, t.files, t.summary_status,
        (SELECT count(*) FROM log_decisions d WHERE d.turn_id=t.id AND d.status='active') n_dec,
        (SELECT count(*) FROM log_watch w WHERE w.turn_id=t.id) n_watch
        FROM log_turns t JOIN log_repos r ON r.id=t.repo_id WHERE t.owner_id=$1 AND t.started_at >= $2 AND NOT r.hidden ORDER BY t.started_at DESC")
        .bind(owner).bind(since).fetch_all(&app.db).await?;
    let tok = |t: &PulseRow| t.in_tok + t.out_tok + t.cache_read + t.cache_write;
    let input_all: i64 = turns.iter().map(|t| t.in_tok + t.cache_read + t.cache_write).sum();
    let mut by_agent: BTreeMap<&str, i64> = BTreeMap::new();
    for t in &turns {
        *by_agent.entry(&t.agent).or_default() += 1;
    }
    let totals = json!({"turns": turns.len(), "agent_ms": turns.iter().map(|t| t.duration_ms).sum::<i64>(), "tokens": turns.iter().map(tok).sum::<i64>(),
        "cache_share": if input_all > 0 { turns.iter().map(|t| t.cache_read).sum::<i64>() as f64 / input_all as f64 } else { 0.0 },
        "cost": turns.iter().filter_map(|t| t.cost).sum::<f64>(), "priced": turns.iter().filter(|t| t.cost.is_some()).count(), "by_agent": by_agent});
    // bars: hours for today, days otherwise; tokens per agent
    let mut bars: Vec<Value> = vec![];
    if range == "today" {
        for h in 0..24 {
            bars.push(json!({"label": format!("{h:02}"), "claude": 0, "codex": 0}));
        }
        for t in &turns {
            let h = tz.at(&t.started_at).hour() as usize;
            let k = if t.agent == "codex" { "codex" } else { "claude" };
            bars[h][k] = json!(bars[h][k].as_i64().unwrap_or(0) + tok(t));
        }
    } else if range == "all" {
        // one bar per month, from the first turn (at most 24)
        let today = tz.at(&now()).date_naive();
        let first = turns.iter().map(|t| t.started_at).min().map(|d| tz.at(&d).date_naive()).unwrap_or(today);
        let months = ((today.year() - first.year()) * 12 + today.month() as i32 - first.month() as i32).clamp(0, 23);
        for i in (0..=months).rev() {
            let m = today.with_day(1).unwrap() - chrono::Months::new(i as u32);
            bars.push(json!({"label": m.format("%b").to_string(), "day": m.format("%Y-%m").to_string(), "claude": 0, "codex": 0}));
        }
        for t in &turns {
            let month = &local_day(&t.started_at, tz)[..7];
            if let Some(b) = bars.iter_mut().find(|b| b["day"] == month) {
                let k = if t.agent == "codex" { "codex" } else { "claude" };
                b[k] = json!(b[k].as_i64().unwrap_or(0) + tok(t));
            }
        }
    } else {
        let days = if range == "month" { 30 } else { 7 };
        let today = tz.at(&now()).date_naive();
        for i in (0..days).rev() {
            let d = today - Duration::days(i);
            bars.push(json!({"label": if days > 7 { d.format("%-d").to_string() } else { d.format("%a").to_string() }, "day": d.format("%Y-%m-%d").to_string(), "claude": 0, "codex": 0}));
        }
        for t in &turns {
            let day = local_day(&t.started_at, tz);
            if let Some(b) = bars.iter_mut().find(|b| b["day"] == day.as_str()) {
                let k = if t.agent == "codex" { "codex" } else { "claude" };
                b[k] = json!(b[k].as_i64().unwrap_or(0) + tok(t));
            }
        }
    }
    // per day per project, newest first
    let mut days: Vec<(String, Vec<(i64, Vec<&PulseRow>)>)> = vec![];
    for t in &turns {
        let d = local_day(&t.started_at, tz);
        if days.last().map(|x| x.0 != d).unwrap_or(true) {
            days.push((d.clone(), vec![]));
        }
        let items = &mut days.last_mut().unwrap().1;
        match items.iter_mut().find(|x| x.0 == t.repo_id) {
            Some(x) => x.1.push(t),
            None => items.push((t.repo_id, vec![t])),
        }
    }
    let files_of = |t: &PulseRow| t.files.as_array().map(|a| a.len()).unwrap_or(0);
    let days: Vec<Value> = days.iter().map(|(day, items)| json!({"day": day, "items": items.iter().map(|(rid, ts)| {
        let mut paths: Vec<&str> = ts.iter().flat_map(|t| t.files.as_array().into_iter().flatten().filter_map(|f| f["path"].as_str())).collect();
        paths.sort();
        paths.dedup();
        let weight = |t: &PulseRow| (if t.verdict == "broken" { 100 } else { 0 }) + t.n_watch * 10 + t.n_dec * 10 + files_of(t) as i64;
        let mut top: Vec<&&PulseRow> = ts.iter().filter(|t| !t.headline.is_empty()).collect();
        top.sort_by_key(|t| -weight(t));
        let mut agents: Vec<&str> = ts.iter().map(|t| t.agent.as_str()).collect();
        agents.dedup();
        json!({"project": repos.get(rid).map(|r| repo_json(app, r)), "turns": ts.len(), "agents": agents, "last_at": ts[0].ended_at,
            "health": ts.iter().map(|t| t.verdict.as_str()).max_by_key(|v| rank(v)).filter(|v| !v.is_empty()),
            "pending": ts.iter().filter(|t| t.summary_status == "queued" || t.summary_status == "running").count(),
            "bullets": top.iter().take(3).map(|t| json!({"turn_id": t.id, "text": t.headline, "verdict": t.verdict})).collect::<Vec<_>>(),
            "decisions": ts.iter().map(|t| t.n_dec).sum::<i64>(), "watch": ts.iter().map(|t| t.n_watch).sum::<i64>(), "files": paths.len()})
    }).collect::<Vec<_>>()})).collect();
    let attention: Vec<(i64, String, String, DateTime<Utc>, i64, i64)> = sqlx::query_as("SELECT w.id, w.text, w.impact, w.created_at, w.turn_id, w.repo_id FROM log_watch w JOIN log_repos r ON r.id=w.repo_id
        WHERE w.owner_id=$1 AND w.status='open' AND NOT r.hidden ORDER BY CASE w.impact WHEN 'high' THEN 0 WHEN 'medium' THEN 1 ELSE 2 END, w.created_at DESC LIMIT 30")
        .bind(owner).fetch_all(&app.db).await?;
    let open_watch: i64 = sqlx::query_scalar("SELECT count(*) FROM log_watch w JOIN log_repos r ON r.id=w.repo_id WHERE w.owner_id=$1 AND w.status='open' AND NOT r.hidden").bind(owner).fetch_one(&app.db).await?;
    let decisions: Vec<(i64, String, DateTime<Utc>, i64, i64)> = sqlx::query_as("SELECT d.id, d.text, d.created_at, d.turn_id, d.repo_id FROM log_decisions d JOIN log_repos r ON r.id=d.repo_id
        WHERE d.owner_id=$1 AND d.status='active' AND d.created_at >= $2 AND NOT r.hidden ORDER BY d.created_at DESC LIMIT 8").bind(owner).bind(since).fetch_all(&app.db).await?;
    let decisions_count: i64 = sqlx::query_scalar("SELECT count(*) FROM log_decisions WHERE owner_id=$1 AND status='active' AND created_at >= $2").bind(owner).bind(since).fetch_one(&app.db).await?;
    let active: Vec<(String, String, String, DateTime<Utc>, i64)> = sqlx::query_as("SELECT agent, session_id, max(machine_name), max(ended_at), (array_agg(repo_id ORDER BY ended_at DESC))[1]
        FROM log_turns WHERE owner_id=$1 AND ended_at >= now() - interval '10 minutes' GROUP BY agent, session_id ORDER BY 4 DESC").bind(owner).fetch_all(&app.db).await?;
    let badge = |rid: &i64| repos.get(rid).map(|r| repo_json(app, r));
    Ok(json!({"range": range, "since": since, "totals": totals, "bars": bars, "days": days,
        "attention": attention.iter().map(|w| json!({"id": w.0, "text": w.1, "impact": w.2, "created_at": w.3, "turn_id": w.4, "project_id": w.5, "project": badge(&w.5)})).collect::<Vec<_>>(),
        "open_watch": open_watch,
        "decisions": decisions.iter().map(|d| json!({"id": d.0, "text": d.1, "created_at": d.2, "turn_id": d.3, "project_id": d.4, "project": badge(&d.4)})).collect::<Vec<_>>(),
        "decisions_count": decisions_count, "worker": worker_stats(app, owner, since).await?,
        "active": active.iter().map(|a| json!({"agent": a.0, "session_id": a.1, "machine": a.2, "last_at": a.3, "project_id": a.4, "project": repos.get(&a.4).map(|r| r.label().to_string())})).collect::<Vec<_>>()}))
}

pub async fn repos_list(app: &AppInner, owner: &str, range: &str, tz: Zone) -> Result<Value, Fail> {
    let since = since_for(range, tz);
    let repos = repos_of(app, owner).await?;
    let rows: Vec<(i64, Option<DateTime<Utc>>, i64, i64, f64, i64, i64, Vec<String>)> = sqlx::query_as("SELECT r.id,
        (SELECT max(ended_at) FROM log_turns WHERE repo_id=r.id),
        (SELECT count(*) FROM log_turns WHERE repo_id=r.id AND started_at >= $2),
        (SELECT COALESCE(sum(in_tok+out_tok+cache_read+cache_write),0)::bigint FROM log_turns WHERE repo_id=r.id AND started_at >= $2),
        (SELECT COALESCE(sum(cost),0)::float8 FROM log_turns WHERE repo_id=r.id AND started_at >= $2),
        (SELECT COALESCE(sum(duration_ms),0)::bigint FROM log_turns WHERE repo_id=r.id AND started_at >= $2),
        (SELECT count(*) FROM log_watch WHERE repo_id=r.id AND status='open'),
        ARRAY(SELECT DISTINCT machine_name FROM log_turns WHERE repo_id=r.id)
        FROM log_repos r WHERE r.owner_id=$1 ORDER BY 2 DESC NULLS LAST").bind(owner).bind(since).fetch_all(&app.db).await?;
    Ok(json!(rows.iter().filter_map(|r| repos.get(&r.0).map(|p| {
        let mut v = repo_json(app, p);
        v["last_at"] = json!(r.1);
        v["turns"] = json!(r.2);
        v["tokens"] = json!(r.3);
        v["cost"] = json!(r.4);
        v["agent_ms"] = json!(r.5);
        v["open_watch"] = json!(r.6);
        v["machines"] = json!(r.7);
        v
    })).collect::<Vec<_>>()))
}

fn decision_json(d: &(i64, i64, String, String, String, String, Option<i64>, DateTime<Utc>, String)) -> Value {
    json!({"id": d.0, "turn_id": d.1, "text": d.2, "why": d.3, "alternative": d.4, "status": d.5, "superseded_by": d.6, "created_at": d.7, "agent": d.8})
}
fn watch_json(w: &(i64, i64, String, String, String, Option<DateTime<Utc>>, DateTime<Utc>, String)) -> Value {
    json!({"id": w.0, "turn_id": w.1, "text": w.2, "impact": w.3, "status": w.4, "done_at": w.5, "created_at": w.6, "agent": w.7})
}
const DEC_COLS: &str = "d.id, d.turn_id, d.text, d.why, d.alternative, d.status, d.superseded_by, d.created_at, t.agent";
const WATCH_COLS: &str = "w.id, w.turn_id, w.text, w.impact, w.status, w.done_at, w.created_at, t.agent";

/// One project: its turns (with their notes), decisions, watch items and sessions.
pub async fn repo_detail(app: &AppInner, owner: &str, id: i64, range: &str, tz: Zone, agent: Option<&str>, only: Option<&str>) -> Result<Value, Fail> {
    let r = owned_repo(app, owner, id).await?;
    let since = since_for(range, tz);
    let (first_at, last_at, total): (Option<DateTime<Utc>>, Option<DateTime<Utc>>, i64) = sqlx::query_as("SELECT min(started_at), max(ended_at), count(*) FROM log_turns WHERE repo_id=$1").bind(id).fetch_one(&app.db).await?;
    let machines: Vec<(String, i64)> = sqlx::query_as("SELECT machine_name, count(*) FROM log_turns WHERE repo_id=$1 GROUP BY 1 ORDER BY 2 DESC").bind(id).fetch_all(&app.db).await?;
    let agent = agent.filter(|a| !a.is_empty() && *a != "all");
    let turns = sqlx::query_as::<_, TurnRow>(&format!("SELECT {TURN_COLS} FROM log_turns WHERE repo_id=$1 AND started_at >= $2 AND ($3::text IS NULL OR agent=$3)
        AND (NOT $4 OR EXISTS (SELECT 1 FROM log_decisions d WHERE d.turn_id=log_turns.id AND d.status <> 'dismissed') OR EXISTS (SELECT 1 FROM log_watch w WHERE w.turn_id=log_turns.id))
        ORDER BY started_at DESC LIMIT 500"))
        .bind(id).bind(since).bind(agent).bind(only == Some("notes")).fetch_all(&app.db).await?;
    let ids: Vec<i64> = turns.iter().map(|t| t.id).collect();
    let notes_d: Vec<(i64, i64, String, String)> = sqlx::query_as("SELECT id, turn_id, text, status FROM log_decisions WHERE turn_id = ANY($1) AND status <> 'dismissed'").bind(&ids).fetch_all(&app.db).await?;
    let notes_w: Vec<(i64, i64, String, String, String)> = sqlx::query_as("SELECT id, turn_id, text, impact, status FROM log_watch WHERE turn_id = ANY($1)").bind(&ids).fetch_all(&app.db).await?;
    let turns_json: Vec<Value> = turns.iter().filter_map(|t| {
        let notes: Vec<Value> = notes_d.iter().filter(|d| d.1 == t.id).map(|d| json!({"kind": "decision", "id": d.0, "text": d.2, "status": d.3}))
            .chain(notes_w.iter().filter(|w| w.1 == t.id).map(|w| json!({"kind": "watch", "id": w.0, "text": w.2, "impact": w.3, "status": w.4}))).collect();
        if only == Some("notes") && notes.is_empty() {
            return None;
        }
        let mut v = turn_json(t);
        v["decisions"] = json!(notes.iter().filter(|n| n["kind"] == "decision").count());
        v["watch"] = json!(notes.iter().filter(|n| n["kind"] == "watch").count());
        v["notes"] = json!(notes);
        Some(v)
    }).collect();
    let all: Vec<(i64, i64, Option<f64>, DateTime<Utc>)> = sqlx::query_as("SELECT duration_ms, in_tok+out_tok+cache_read+cache_write, cost, started_at FROM log_turns WHERE repo_id=$1 AND started_at >= $2 AND ($3::text IS NULL OR agent=$3)")
        .bind(id).bind(since).bind(agent).fetch_all(&app.db).await?;
    let totals = json!({"turns": all.len(), "agent_ms": all.iter().map(|t| t.0).sum::<i64>(), "tokens": all.iter().map(|t| t.1).sum::<i64>(), "cost": all.iter().filter_map(|t| t.2).sum::<f64>()});
    // minutes of agent time per hour (today), per day (week, month) or per month (all: the last 24)
    let mut hist = vec![0i64; match range { "today" | "all" => 24, "month" => 30, _ => 7 }];
    let this_month = { let d = tz.at(&now()); d.year() * 12 + d.month() as i32 };
    for t in &all {
        let l = tz.at(&t.3);
        let i = match range { "today" => l.hour() as i64, "all" => 23 - (this_month - (l.year() * 12 + l.month() as i32)) as i64, _ => (l.date_naive() - tz.at(&since).date_naive()).num_days() };
        if i >= 0 && (i as usize) < hist.len() {
            hist[i as usize] += (t.0 as f64 / 60000.0).round() as i64;
        }
    }
    let decisions: Vec<(i64, i64, String, String, String, String, Option<i64>, DateTime<Utc>, String)> = sqlx::query_as(&format!("SELECT {DEC_COLS} FROM log_decisions d JOIN log_turns t ON t.id=d.turn_id
        WHERE d.repo_id=$1 AND d.status <> 'dismissed' ORDER BY d.created_at DESC LIMIT 100")).bind(id).fetch_all(&app.db).await?;
    let watch: Vec<(i64, i64, String, String, String, Option<DateTime<Utc>>, DateTime<Utc>, String)> = sqlx::query_as(&format!("SELECT {WATCH_COLS} FROM log_watch w JOIN log_turns t ON t.id=w.turn_id
        WHERE w.repo_id=$1 ORDER BY w.status='open' DESC, w.created_at DESC LIMIT 100")).bind(id).fetch_all(&app.db).await?;
    let sessions: Vec<(String, String, String, DateTime<Utc>)> = sqlx::query_as("SELECT agent, session_id, max(machine_name), max(ended_at) FROM log_turns WHERE repo_id=$1
        GROUP BY agent, session_id ORDER BY 4 DESC LIMIT 5").bind(id).fetch_all(&app.db).await?;
    Ok(json!({"project": repo_json(app, &r), "first_at": first_at, "last_at": last_at, "total_turns": total,
        "machines": machines.iter().map(|m| json!({"machine": m.0, "n": m.1})).collect::<Vec<_>>(), "totals": totals, "hist": hist, "turns": turns_json,
        "decisions": decisions.iter().map(decision_json).collect::<Vec<_>>(), "watch": watch.iter().map(watch_json).collect::<Vec<_>>(),
        "sessions": sessions.iter().map(|s| json!({"agent": s.0, "session_id": s.1, "machine": s.2, "last_at": s.3, "active": now() - s.3 < Duration::minutes(10)})).collect::<Vec<_>>()}))
}

/// A path or id as one shell word: bare when plain, else single-quoted.
pub fn shq(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./~-_".contains(c)) { s.to_string() } else { format!("'{}'", s.replace('\'', r"'\''")) }
}

/// One turn. `local` is what only the recording Mac holds (diffs, images, the tool log),
/// asked from its relay; `local_error` says why it is missing.
pub async fn turn_detail(app: &AppInner, owner: &str, id: i64, image_base: &str) -> Result<Value, Fail> {
    let t = owned_turn(app, owner, id).await?;
    let r = owned_repo(app, owner, t.repo_id).await?;
    let decisions: Vec<(i64, i64, String, String, String, String, Option<i64>, DateTime<Utc>, String)> = sqlx::query_as(&format!("SELECT {DEC_COLS} FROM log_decisions d JOIN log_turns t ON t.id=d.turn_id WHERE d.turn_id=$1 ORDER BY d.id")).bind(id).fetch_all(&app.db).await?;
    let watch: Vec<(i64, i64, String, String, String, Option<DateTime<Utc>>, DateTime<Utc>, String)> = sqlx::query_as(&format!("SELECT {WATCH_COLS} FROM log_watch w JOIN log_turns t ON t.id=w.turn_id WHERE w.turn_id=$1 ORDER BY w.id")).bind(id).fetch_all(&app.db).await?;
    let seq: Vec<i64> = sqlx::query_scalar("SELECT id FROM log_turns WHERE owner_id=$1 AND agent=$2 AND session_id=$3 ORDER BY started_at").bind(owner).bind(&t.agent).bind(&t.session_id).fetch_all(&app.db).await?;
    let first_at: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT min(started_at) FROM log_turns WHERE owner_id=$1 AND agent=$2 AND session_id=$3").bind(owner).bind(&t.agent).bind(&t.session_id).fetch_one(&app.db).await?;
    let prev: Option<i64> = sqlx::query_scalar("SELECT id FROM log_turns WHERE repo_id=$1 AND started_at < $2 ORDER BY started_at DESC LIMIT 1").bind(t.repo_id).bind(t.started_at).fetch_optional(&app.db).await?;
    let next: Option<i64> = sqlx::query_scalar("SELECT id FROM log_turns WHERE repo_id=$1 AND started_at > $2 ORDER BY started_at ASC LIMIT 1").bind(t.repo_id).bind(t.started_at).fetch_optional(&app.db).await?;
    let (local, local_error) = if !app.machine_online(&t.machine_id) {
        (Value::Null, json!(format!("{} is offline. The diffs, images and tool log of this turn stay on that Mac; they show here when its relay is running.", t.machine_name)))
    } else {
        match app.rpc(&t.machine_id, "turn_local", json!({"turn": t.tref()}), 8).await {
            Ok(v) => (v, Value::Null),
            Err(e) => (Value::Null, json!(e.1)),
        }
    };
    let attachments: Vec<Value> = t.attachments.as_array().into_iter().flatten().enumerate().map(|(i, a)| {
        let mut a = a.clone();
        a["id"] = json!(i);
        let img = a["image"].as_u64();
        a["src"] = match img { Some(n) if !local.is_null() => json!(format!("{image_base}/{n}")), _ => Value::Null };
        a["has_data"] = json!(img.is_some());
        a
    }).collect();
    let mut turn = turn_json(&t);
    turn["final_text"] = json!(t.final_text);
    turn["diffs"] = local.get("diffs").cloned().unwrap_or(json!({}));
    let resume = format!("cd {} && {} {}", shq(&t.cwd), if t.agent == "codex" { "codex resume" } else { "claude --resume" }, shq(&t.session_id));
    Ok(json!({"turn": turn, "attachments": attachments, "project": repo_json(app, &r),
        "decisions": decisions.iter().map(decision_json).collect::<Vec<_>>(), "watch": watch.iter().map(watch_json).collect::<Vec<_>>(),
        "job": {"status": t.summary_status, "model": t.summary_model, "tokens": t.summary_tokens, "cost": t.summary_cost, "finished_at": t.summary_at,
            "error": if t.summary_error.is_empty() { Value::Null } else { json!(t.summary_error) }},
        "session": {"index": seq.iter().position(|x| *x == id).map(|i| i + 1).unwrap_or(0), "count": seq.len(), "first_at": first_at},
        "prev": prev, "next": next, "resume": resume, "local": local, "local_error": local_error}))
}

/// An image of the turn, from the Mac that recorded it.
pub async fn turn_image_bytes(app: &AppInner, owner: &str, id: i64, n: u32) -> Result<Response, Fail> {
    let t = owned_turn(app, owner, id).await?;
    let v = app.rpc(&t.machine_id, "turn_image", json!({"turn": t.tref(), "index": n}), 15).await?;
    relay_image(&v)
}

pub async fn search(app: &AppInner, owner: &str, q: &str) -> Result<Value, Fail> {
    let words: Vec<String> = q.split(|c: char| !c.is_alphanumeric() && c != '_').filter(|w| !w.is_empty()).take(8).map(|w| format!("{}:*", w.to_lowercase())).collect();
    if words.is_empty() {
        return Ok(json!([]));
    }
    let query = words.join(" & ");
    let rows: Vec<(i64, i64, String, DateTime<Utc>, String, String, String, String)> = sqlx::query_as(
        "SELECT t.id, t.repo_id, t.agent, t.started_at, t.headline, t.prompt, t.verdict,
           ts_headline('simple', concat_ws(' … ', t.headline, t.prompt, left(t.final_text, 2000)), q, 'StartSel=[[, StopSel=]], MaxWords=28, MinWords=10, MaxFragments=2, FragmentDelimiter= … ')
         FROM log_turns t, to_tsquery('simple', $2) q WHERE t.owner_id=$1 AND t.search @@ q ORDER BY ts_rank(t.search, q) DESC, t.started_at DESC LIMIT 50")
        .bind(owner).bind(&query).fetch_all(&app.db).await?;
    let repos = repos_of(app, owner).await?;
    Ok(json!(rows.iter().map(|r| json!({"id": r.0, "project_id": r.1, "agent": r.2, "started_at": r.3, "headline": r.4, "prompt": r.5,
        "verdict": if r.6.is_empty() { Value::Null } else { json!(r.6) }, "snippet": r.7, "project": repos.get(&r.1).map(|p| repo_json(app, p))})).collect::<Vec<_>>()))
}

pub async fn export(app: &AppInner, owner: &str, id: i64) -> Result<Value, Fail> {
    let r = owned_repo(app, owner, id).await?;
    let turns = sqlx::query_as::<_, TurnRow>(&format!("SELECT {TURN_COLS} FROM log_turns WHERE repo_id=$1 ORDER BY started_at")).bind(id).fetch_all(&app.db).await?;
    let decisions: Vec<(i64, i64, String, String, String, String, Option<i64>, DateTime<Utc>, String)> = sqlx::query_as(&format!("SELECT {DEC_COLS} FROM log_decisions d JOIN log_turns t ON t.id=d.turn_id WHERE d.repo_id=$1 ORDER BY d.id")).bind(id).fetch_all(&app.db).await?;
    let watch: Vec<(i64, i64, String, String, String, Option<DateTime<Utc>>, DateTime<Utc>, String)> = sqlx::query_as(&format!("SELECT {WATCH_COLS} FROM log_watch w JOIN log_turns t ON t.id=w.turn_id WHERE w.repo_id=$1 ORDER BY w.id")).bind(id).fetch_all(&app.db).await?;
    Ok(json!({"exported_at": now(), "project": repo_json(app, &r),
        "turns": turns.iter().map(|t| { let mut v = turn_json(t); v["final_text"] = json!(t.final_text); v }).collect::<Vec<_>>(),
        "decisions": decisions.iter().map(decision_json).collect::<Vec<_>>(), "watch_items": watch.iter().map(watch_json).collect::<Vec<_>>()}))
}

// ---------- actions ----------

pub async fn set_watch(app: &AppInner, owner: &str, id: i64, action: &str) -> Result<(), Fail> {
    let st = match action { "done" => "done", "open" => "open", _ => return Err(Fail::new(404, "done or open")) };
    let n = sqlx::query("UPDATE log_watch SET status=$3, done_at=CASE WHEN $3='done' THEN now() END WHERE id=$1 AND owner_id=$2").bind(id).bind(owner).bind(st).execute(&app.db).await?.rows_affected();
    if n == 0 {
        return Err(Fail::new(404, "no such watch item"));
    }
    app.changed(owner);
    Ok(())
}

pub async fn set_decision(app: &AppInner, owner: &str, id: i64, action: &str, by: Option<i64>) -> Result<(), Fail> {
    let n = match action {
        "dismiss" => sqlx::query("UPDATE log_decisions SET status='dismissed' WHERE id=$1 AND owner_id=$2").bind(id).bind(owner).execute(&app.db).await?,
        "restore" => sqlx::query("UPDATE log_decisions SET status='active', superseded_by=NULL WHERE id=$1 AND owner_id=$2").bind(id).bind(owner).execute(&app.db).await?,
        "supersede" => {
            let by = by.filter(|b| *b != id).ok_or_else(|| Fail::new(400, "by: the id of the newer decision is required"))?;
            sqlx::query("UPDATE log_decisions SET status='superseded', superseded_by=$3 WHERE id=$1 AND owner_id=$2 AND EXISTS (SELECT 1 FROM log_decisions WHERE id=$3 AND owner_id=$2)")
                .bind(id).bind(owner).bind(by).execute(&app.db).await?
        }
        _ => return Err(Fail::new(404, "dismiss, restore or supersede")),
    }.rows_affected();
    if n == 0 {
        return Err(Fail::new(404, "no such decision"));
    }
    app.changed(owner);
    Ok(())
}

pub async fn update_repo(app: &AppInner, owner: &str, id: i64, name: Option<&str>, hidden: Option<bool>, icon_reset: bool) -> Result<(), Fail> {
    owned_repo(app, owner, id).await?;
    if let Some(n) = name {
        sqlx::query("UPDATE log_repos SET display_name=$2 WHERE id=$1").bind(id).bind(clip(n, 100)).execute(&app.db).await?;
    }
    if let Some(h) = hidden {
        sqlx::query("UPDATE log_repos SET hidden=$2 WHERE id=$1").bind(id).bind(h).execute(&app.db).await?;
    }
    if icon_reset {
        sqlx::query("UPDATE log_repos SET icon_locked=false WHERE id=$1").bind(id).execute(&app.db).await?;
    }
    app.changed(owner);
    Ok(())
}

/// Write the turn's summary again, on the Mac that recorded it.
pub async fn resummarize(app: &AppInner, owner: &str, id: i64) -> Result<(), Fail> {
    let t = owned_turn(app, owner, id).await?;
    app.rpc(&t.machine_id, "resummarize", json!({"turn": t.tref()}), 10).await?;
    sqlx::query("UPDATE log_turns SET summary_status='queued', summary_error='' WHERE id=$1").bind(id).execute(&app.db).await?;
    audit(app, owner, "", owner, "log.resummarize", &id.to_string(), "").await;
    app.changed(owner);
    Ok(())
}

/// Queue every failed summary again, on each online Mac that has some.
pub async fn retry_failed(app: &AppInner, owner: &str) -> Result<Value, Fail> {
    let ms: Vec<(String, i64)> = sqlx::query_as("SELECT machine_id, count(*) FROM log_turns WHERE owner_id=$1 AND summary_status='failed' GROUP BY 1").bind(owner).fetch_all(&app.db).await?;
    let (mut retried, mut offline) = (0, 0);
    // The relay reports each job it queues again (POST /relay/log/summary); a turn it no longer has stays failed.
    for (m, n) in ms {
        match app.rpc(&m, "retry_failed", json!({}), 10).await {
            Ok(v) => retried += v["retried"].as_i64().unwrap_or(0),
            Err(_) => offline += n,
        }
    }
    app.changed(owner);
    Ok(json!({"retried": retried, "offline": offline}))
}

// ---------- live agents ----------

/// Remember when each pane's agent entered its state (Theasa's `since`).
pub fn note_since(app: &AppInner, machine: &str, panes: &[Pane]) {
    let mut s = app.since.lock().unwrap();
    s.retain(|k, _| !k.starts_with(&format!("{machine} ")) || panes.iter().any(|p| *k == format!("{machine} {}", p.pane_id)));
    for p in panes {
        let k = format!("{machine} {}", p.pane_id);
        if s.get(&k).map(|x| x.0 != p.status).unwrap_or(true) {
            s.insert(k, (p.status.clone(), now()));
        }
    }
}

/// The paragraph of a reply that asks something, if any.
pub fn last_question(text: &str) -> Option<String> {
    let paras: Vec<&str> = text.trim().split("\n\n").map(str::trim).filter(|p| !p.is_empty()).collect();
    paras.iter().rev().take(2).find(|p| {
        let t = p.trim_end().trim_end_matches("**").trim_end().trim_end_matches(['"', '”', ')']);
        t.ends_with('?') || p.lines().any(|l| l.trim_end().trim_end_matches("**").trim_end().ends_with('?'))
    }).map(|p| p.to_string())
}

/// Every agent on the owner's online Macs. `files_base` builds the URL of an ask's image
/// (`{files_base}/{machine}/{pane}/ask-files/{n}`).
pub async fn agents(app: &AppInner, owner: &str, files_base: &str) -> Result<Value, Fail> {
    let ms: Vec<(String, String, Value)> = sqlx::query_as("SELECT id, name, panes FROM machines WHERE account_id=$1 AND NOT revoked ORDER BY created_at").bind(owner).fetch_all(&app.db).await?;
    let projects = sqlx::query_as::<_, crate::domain::Project>("SELECT * FROM projects WHERE owner_id=$1").bind(owner).fetch_all(&app.db).await?;
    let repos: Vec<RepoRow> = repos_of(app, owner).await?.into_values().collect();
    let mut out = vec![];
    for (mid, mname, panes) in ms.iter().filter(|m| app.machine_online(&m.0)) {
        for p in serde_json::from_value::<Vec<Pane>>(panes.clone()).unwrap_or_default().into_iter().filter(|p| !p.agent.is_empty()) {
            let since = app.since.lock().unwrap().get(&format!("{mid} {}", p.pane_id)).map(|x| x.1);
            // the pane's own session names its repo; else its folder (a root is the last Mac's that uploaded)
            let by_session: Option<i64> = if p.agent_session.is_empty() { None } else {
                sqlx::query_scalar("SELECT repo_id FROM log_turns WHERE owner_id=$1 AND session_id=$2 ORDER BY ended_at DESC LIMIT 1").bind(owner).bind(&p.agent_session).fetch_optional(&app.db).await?
            };
            let repo = by_session.and_then(|id| repos.iter().find(|r| r.id == id))
                .or_else(|| repos.iter().filter(|r| !r.root.is_empty() && (p.cwd == r.root || p.cwd.starts_with(&format!("{}/", r.root)))).max_by_key(|r| r.root.len()));
            let mut a = json!({"machine_id": mid, "machine": mname, "pane_id": p.pane_id, "agent": p.agent, "status": p.status, "title": if p.title.is_empty() { &p.agent } else { &p.title },
                "cwd": p.cwd, "workspace": p.workspace, "session_id": if p.agent_session.is_empty() { Value::Null } else { json!(p.agent_session) }, "since": since,
                "project": projects.iter().find(|x| &x.machine_id == mid && x.pane_id == p.pane_id).map(|x| crate::appapi::project_json(app, x)),
                "repo": repo.map(|r| repo_json(app, r)), "ask": Value::Null, "ask_extras": Value::Null, "screen": Value::Null, "last": Value::Null});
            if let Some(l) = p.live.as_ref().filter(|_| p.status == "blocked") {
                if !l.ask.is_empty() {
                    a["ask"] = json!(l.ask);
                    let base = format!("{files_base}/{}/{}/ask-files", urlencoding::encode(mid), urlencoding::encode(&p.pane_id));
                    a["ask_extras"] = json!({"context": if l.context.is_empty() { Value::Null } else { json!(l.context) },
                        "attachments": l.files.iter().enumerate().map(|(i, f)| json!({"id": i, "kind": f.kind, "name": f.name,
                            "url": if f.url.starts_with("https://claude.ai/") { json!(f.url) } else { Value::Null },
                            "media_type": if f.media_type.is_empty() { Value::Null } else { json!(f.media_type) },
                            "src": if RASTER.contains(&f.media_type.as_str()) { json!(format!("{base}/{i}")) } else { Value::Null }})).collect::<Vec<_>>()});
                } else if !l.screen.is_empty() {
                    a["screen"] = json!({"text": l.screen, "options": l.options});
                }
            }
            if matches!(p.status.as_str(), "done" | "idle" | "blocked") && !p.agent_session.is_empty() {
                let t: Option<(i64, String, String, DateTime<Utc>)> = sqlx::query_as("SELECT id, headline, final_text, ended_at FROM log_turns WHERE owner_id=$1 AND session_id=$2 AND agent=$3 ORDER BY started_at DESC LIMIT 1")
                    .bind(owner).bind(&p.agent_session).bind(if p.agent == "codex" { "codex" } else { "claude" }).fetch_optional(&app.db).await?;
                if let Some((id, headline, text, ended)) = t.filter(|t| !t.2.is_empty()) {
                    let cut = text.char_indices().rev().nth(3999).map(|(i, _)| i);
                    let shown = match cut { Some(i) if i > 0 => format!("…{}", &text[i..]), _ => text.clone() };
                    a["last"] = json!({"turn_id": id, "headline": if headline.is_empty() { Value::Null } else { json!(headline) }, "text": shown, "question": last_question(&text), "ended_at": ended});
                }
            }
            out.push(a);
        }
    }
    Ok(json!({"available": true, "error": Value::Null, "agents": out}))
}

/// The machine is the owner's and the pane is one of its agents right now.
async fn owned_pane(app: &AppInner, owner: &str, machine: &str, pane: &str) -> Result<Pane, Fail> {
    let panes: Value = sqlx::query_scalar("SELECT panes FROM machines WHERE id=$1 AND account_id=$2 AND NOT revoked").bind(machine).bind(owner)
        .fetch_optional(&app.db).await?.ok_or_else(|| Fail::new(404, "no such computer"))?;
    serde_json::from_value::<Vec<Pane>>(panes).unwrap_or_default().into_iter().find(|p| p.pane_id == pane && !p.agent.is_empty()).ok_or_else(|| Fail::new(404, "that agent is gone"))
}

pub async fn agent_keys(app: &AppInner, owner: &str, machine: &str, pane: &str, keys: &[String]) -> Result<(), Fail> {
    if keys.is_empty() || keys.len() > 10 || !keys.iter().all(|k| proto::AGENT_KEYS.contains(&k.as_str())) {
        return Err(Fail::new(400, "keys must be 1 to 10 of: a digit, esc, enter, up, down, left, right, tab, space, y, n"));
    }
    owned_pane(app, owner, machine, pane).await?;
    app.rpc(machine, "agent_keys", json!({"pane_id": pane, "keys": keys}), 10).await?;
    audit(app, owner, "", owner, "agent.keys", pane, &keys.join(" ")).await;
    app.changed(owner);
    Ok(())
}

/// A prompt for the agent. The relay cancels an open AskUserQuestion first.
pub async fn agent_prompt(app: &AppInner, owner: &str, machine: &str, pane: &str, text: &str) -> Result<(), Fail> {
    let text = proto::sanitize(text, 20_000);
    if text.is_empty() {
        return Err(Fail::new(400, "send 1 to 20000 characters of text"));
    }
    let p = owned_pane(app, owner, machine, pane).await?;
    if p.status == "working" {
        return Err(Fail::new(409, "the agent is working; send when it is done"));
    }
    app.rpc(machine, "agent_prompt", json!({"pane_id": pane, "text": text}), 30).await?;
    audit(app, owner, "", owner, "agent.prompt", pane, &format!("{} characters", text.chars().count())).await;
    app.changed(owner);
    Ok(())
}

pub async fn ask_file(app: &AppInner, owner: &str, machine: &str, pane: &str, n: u32) -> Result<Response, Fail> {
    owned_pane(app, owner, machine, pane).await?;
    let v = app.rpc(machine, "ask_file", json!({"pane_id": pane, "index": n}), 15).await?;
    relay_image(&v)
}

// ---------- app API (/api/v1) ----------

fn zone(q: &ListQ) -> Zone {
    Zone::parse(q.tz.as_deref().unwrap_or("0"))
}

#[derive(Deserialize, Default)]
pub struct ListQ {
    pub range: Option<String>,
    #[serde(default)]
    pub tz: Option<String>,
    pub agent: Option<String>,
    pub only: Option<String>,
    pub q: Option<String>,
}

pub async fn api_pulse(State(app): State<App>, headers: HeaderMap, Query(q): Query<ListQ>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    Ok(Json(pulse(&app, &aid, norm_range(q.range.as_deref()), zone(&q)).await?))
}

pub async fn api_repos(State(app): State<App>, headers: HeaderMap, Query(q): Query<ListQ>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    Ok(Json(repos_list(&app, &aid, norm_range(q.range.as_deref()), zone(&q)).await?))
}

pub async fn api_repo(State(app): State<App>, headers: HeaderMap, Path(id): Path<i64>, Query(q): Query<ListQ>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    Ok(Json(repo_detail(&app, &aid, id, norm_range(q.range.as_deref()), zone(&q), q.agent.as_deref(), q.only.as_deref()).await?))
}

#[derive(Deserialize)]
pub struct RepoPatch {
    name: Option<String>,
    hidden: Option<bool>,
    #[serde(default)]
    icon_reset: bool,
}

pub async fn api_repo_update(State(app): State<App>, headers: HeaderMap, Path(id): Path<i64>, Json(b): Json<RepoPatch>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    update_repo(&app, &aid, id, b.name.as_deref(), b.hidden, b.icon_reset).await?;
    Ok(Json(json!({"ok": true})))
}

/// POST /api/v1/log/repos/{id}/icon: multipart field `icon`. The icon stays until reset.
pub async fn api_repo_icon(State(app): State<App>, headers: HeaderMap, Path(id): Path<i64>, mut mp: Multipart) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    owned_repo(&app, &aid, id).await?;
    while let Ok(Some(f)) = mp.next_field().await {
        if f.name() == Some("icon") {
            let b = f.bytes().await.map_err(|_| Fail::new(400, "send an image under 1 MB as form field 'icon'"))?;
            set_icon(&app, &aid, IconTarget::Id(id), &b, true).await?;
            return Ok(Json(json!({"ok": true})));
        }
    }
    Err(Fail::new(400, "send an image under 1 MB as form field 'icon'"))
}

pub async fn api_export(State(app): State<App>, headers: HeaderMap, Path(id): Path<i64>) -> Result<Response, Fail> {
    let aid = bearer(&app, &headers).await?;
    let v = export(&app, &aid, id).await?;
    Ok(([(header::CONTENT_TYPE, "application/json".to_string()), (header::CONTENT_DISPOSITION, format!("attachment; filename=\"worklog-project-{id}.json\""))],
        serde_json::to_string_pretty(&v).unwrap()).into_response())
}

pub async fn api_turn(State(app): State<App>, headers: HeaderMap, Path(id): Path<i64>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    Ok(Json(turn_detail(&app, &aid, id, &format!("{}/api/v1/log/turns/{id}/images", app.cfg.public_url)).await?))
}

pub async fn api_turn_image(State(app): State<App>, headers: HeaderMap, Path((id, n)): Path<(i64, u32)>) -> Result<Response, Fail> {
    let aid = bearer(&app, &headers).await?;
    turn_image_bytes(&app, &aid, id, n).await
}

pub async fn api_search(State(app): State<App>, headers: HeaderMap, Query(q): Query<ListQ>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    Ok(Json(search(&app, &aid, q.q.as_deref().unwrap_or("")).await?))
}

pub async fn api_resummarize(State(app): State<App>, headers: HeaderMap, Path(id): Path<i64>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    resummarize(&app, &aid, id).await?;
    Ok(Json(json!({"ok": true})))
}

pub async fn api_watch(State(app): State<App>, headers: HeaderMap, Path((id, action)): Path<(i64, String)>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    set_watch(&app, &aid, id, &action).await?;
    Ok(Json(json!({"ok": true})))
}

pub async fn api_decision(State(app): State<App>, headers: HeaderMap, Path((id, action)): Path<(i64, String)>, body: Bytes) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    let b: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    set_decision(&app, &aid, id, &action, b["by"].as_i64()).await?;
    Ok(Json(json!({"ok": true})))
}

pub async fn api_retry_failed(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    Ok(Json(retry_failed(&app, &aid).await?))
}

pub async fn api_agents(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    Ok(Json(agents(&app, &aid, &format!("{}/api/v1/agents", app.cfg.public_url)).await?))
}

pub async fn api_agent_action(State(app): State<App>, headers: HeaderMap, Path((machine, pane, action)): Path<(String, String, String)>, Json(b): Json<Value>) -> Result<Json<Value>, Fail> {
    let aid = bearer(&app, &headers).await?;
    match action.as_str() {
        "keys" => {
            let keys: Vec<String> = serde_json::from_value(b["keys"].clone()).map_err(|_| Fail::new(400, "keys is a list"))?;
            agent_keys(&app, &aid, &machine, &pane, &keys).await?
        }
        "prompt" => agent_prompt(&app, &aid, &machine, &pane, b["text"].as_str().unwrap_or("")).await?,
        _ => return Err(Fail::new(404, "keys or prompt")),
    }
    Ok(Json(json!({"ok": true})))
}

pub async fn api_ask_file(State(app): State<App>, headers: HeaderMap, Path((machine, pane, n)): Path<(String, String, u32)>) -> Result<Response, Fail> {
    let aid = bearer(&app, &headers).await?;
    ask_file(&app, &aid, &machine, &pane, n).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert!(is_file_name("canvas.json") && !is_file_name("Plan v1.2") && !is_file_name("v1.2"));
        assert_eq!(shq("/my repo"), "'/my repo'");
        assert_eq!(shq("a/b-c.d"), "a/b-c.d");
        assert_eq!(last_question("Done.\n\nShall I deploy it?"), Some("Shall I deploy it?".into()));
        assert_eq!(last_question("Done. All tests pass."), None);
        set_prices(&json!({"claude-sonnet-4-5": {"input_cost_per_token": 0.000003, "output_cost_per_token": 0.000015}}));
        let c = cost_of("claude-sonnet-4-5-20250929", &Tokens { input: 1000, output: 1000, cache_read: 1000, cache_write: 0 }).unwrap();
        assert!((c - (0.003 + 0.015 + 0.0003)).abs() < 1e-9, "{c}");
        assert!(cost_of("unknown-model", &Tokens::default()).is_none());
        // a day starts at the reader's midnight
        let s = since_for("today", Zone::Minutes(120));
        assert_eq!(Zone::Minutes(120).at(&s).hour(), 0);
        // a named zone takes the offset of each date: Amsterdam is +1 in winter, +2 in summer
        let z = Zone::parse("Europe/Amsterdam");
        assert_eq!(z.at(&"2026-01-15T12:00:00Z".parse().unwrap()).hour(), 13);
        assert_eq!(z.at(&"2026-07-15T12:00:00Z".parse().unwrap()).hour(), 14);
    }
}
