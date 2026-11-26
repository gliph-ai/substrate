//! Turns out of agent transcripts. Pure functions: lines in, turns out.
//! Claude Code: ~/.claude/projects/<dir>/<session>.jsonl
//! Codex:       ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use substrate_proto::{FileChange, Tokens};

use super::redact;

/// A file or image that came with a turn. `data` (base64) stays on the Mac.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Att {
    pub dir: String,
    pub kind: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub media_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub data: String,
    #[serde(default = "one", skip_serializing_if = "is_one")]
    pub count: u32,
}

fn one() -> u32 {
    1
}
fn is_one(n: &u32) -> bool {
    *n == 1
}

fn att(dir: &str, kind: &str, name: &str) -> Att {
    Att { dir: dir.into(), kind: kind.into(), name: name.into(), count: 1, ..Default::default() }
}

/// One turn with everything the Mac keeps (the log, diffs and image bytes included).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Turn {
    pub agent: String,
    pub session_id: String,
    pub turn_key: String,
    pub started_at: String,
    pub ended_at: String,
    pub duration_ms: u64,
    pub tool_ms: u64,
    pub prompt: String,
    pub final_text: String,
    pub model: String,
    pub tokens: Tokens,
    pub model_calls: u32,
    pub tools: BTreeMap<String, u32>,
    pub files: Vec<FileChange>,
    /// Unified diff text per file path, capped.
    pub diffs: BTreeMap<String, String>,
    pub attachments: Vec<Att>,
    /// Chronological digest for the summariser, redacted, trimmed.
    pub log: String,
    pub cwd: String,
    /// Index of the line that starts the turn (for the hook's offset).
    #[serde(skip)]
    pub line: Option<usize>,
    /// Codex: running token total before the turn.
    #[serde(skip)]
    pub base: Option<Tokens>,
}

const LOG_LIMIT: usize = 60_000;
const DIFF_FILE_LINES: usize = 400;
const DIFF_TURN_CHARS: usize = 150_000;
pub const IMG_MAX: usize = 1_500_000;

pub fn trim_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let half = (max - 40) / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(n - half).collect();
    format!("{head}\n…[trimmed {} chars]…\n{tail}", n - 2 * half)
}

fn ts(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s).map(|d| d.timestamp_millis()).unwrap_or(0)
}
fn ms(a: &str, b: &str) -> u64 {
    (ts(b) - ts(a)).max(0) as u64
}
fn clock(start: &str, t: &str) -> String {
    let s = ms(start, t) / 1000;
    format!("+{}:{:02}", s / 60, s % 60)
}
/// Whitespace collapsed, trimmed, at most `n` chars.
pub fn one_line(s: &str, n: usize) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(n).collect()
}

fn add_diff(diffs: &mut Vec<(String, Vec<String>)>, path: &str, lines: &[String]) {
    let i = match diffs.iter().position(|d| d.0 == path) {
        Some(i) => i,
        None => {
            diffs.push((path.to_string(), vec![]));
            diffs.len() - 1
        }
    };
    let cur = &mut diffs[i].1;
    let room = DIFF_FILE_LINES.saturating_sub(cur.len());
    cur.extend(lines.iter().take(room).cloned());
    if lines.len() > room && !cur.last().is_some_and(|l| l.starts_with('…')) {
        cur.push("… more lines not shown".into());
    }
}

fn finish_diffs(diffs: Vec<(String, Vec<String>)>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut total = 0;
    let mut all = diffs;
    // Smallest first, so one huge file does not push out the rest.
    all.sort_by_key(|d| d.1.len());
    for (p, lines) in all {
        let text = redact(&lines.join("\n"));
        if total + text.len() > DIFF_TURN_CHARS {
            out.insert(p, "… diff not stored (the turn's diffs are too large)".into());
            continue;
        }
        total += text.len();
        out.insert(p, text);
    }
    out
}

/// One entry per artifact URL, counting repeat publishes.
fn finish_attachments(list: Vec<Att>) -> Vec<Att> {
    let mut out: Vec<Att> = vec![];
    for a in list {
        if a.kind == "artifact" {
            if a.url.is_empty() {
                continue; // a publish that failed or never named its page
            }
            if let Some(prev) = out.iter_mut().find(|p| p.kind == "artifact" && p.url == a.url) {
                prev.count += 1;
                if !a.name.is_empty() && prev.name.starts_with("http") {
                    prev.name = a.name;
                }
                continue;
            }
        }
        out.push(a);
    }
    out
}

fn re(cell: &'static OnceLock<Regex>, pat: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pat).unwrap())
}

fn artifact_url(s: &str) -> Option<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"https://claude\.ai/(?:code/)?artifact/[A-Za-z0-9_-]+").find(s).map(|m| m.as_str().to_string())
}

fn base_name(p: &str) -> String {
    p.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(p).to_string()
}

fn add_file(files: &mut Vec<FileChange>, path: &str, add: u64, del: u64) {
    match files.iter_mut().find(|f| f.path == path) {
        Some(f) => {
            f.add += add;
            f.del += del;
        }
        None => files.push(FileChange { path: path.into(), add, del }),
    }
}

/// Each JSON line with its index; a partial last line (the agent still writing) is skipped.
fn parse_lines(lines: &[&str]) -> Vec<(usize, Value)> {
    lines.iter().enumerate().filter(|(_, l)| !l.trim().is_empty())
        .filter_map(|(i, l)| serde_json::from_str::<Value>(l).ok().filter(Value::is_object).map(|v| (i, v))).collect()
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

// ---------- Claude Code ----------

fn user_text(d: &Value) -> Option<String> {
    let c = &d["message"]["content"];
    if let Some(t) = c.as_str() {
        return Some(t.to_string());
    }
    let a = c.as_array()?;
    if a.iter().any(|x| x["type"] == "tool_result") {
        return None;
    }
    Some(a.iter().filter(|x| x["type"] == "text").map(|x| s(&x["text"])).collect::<Vec<_>>().join("\n"))
}

fn is_claude_prompt(d: &Value, cur_prompt: Option<&str>) -> bool {
    if d["type"] != "user" || d["isSidechain"] == true || d["isCompactSummary"] == true {
        return false;
    }
    match user_text(d) {
        None => return false,
        Some(t) if t.starts_with("<local-command") => return false,
        _ => {}
    }
    // Meta entries (peer messages, wake-ups) start a turn only when they open a new prompt.
    if d["isMeta"] == true {
        let p = s(&d["promptId"]);
        return !p.is_empty() && Some(p) != cur_prompt;
    }
    true
}

pub fn clean_prompt(t: &str, origin: &Value) -> String {
    static SUMMARY: OnceLock<Regex> = OnceLock::new();
    static CMD: OnceLock<Regex> = OnceLock::new();
    static ARGS: OnceLock<Regex> = OnceLock::new();
    static REMINDER: OnceLock<Regex> = OnceLock::new();
    let kind = s(&origin["kind"]);
    if kind == "peer" && !s(&origin["body"]).is_empty() {
        let name = origin["name"].as_str().unwrap_or("another session");
        return format!("Message from {name}: {}", s(&origin["body"]));
    }
    if kind == "task-notification" || t.starts_with("<task-notification") {
        return match re(&SUMMARY, r"<summary>([\s\S]*?)</summary>").captures(t) {
            Some(c) => format!("Background task finished: {}", one_line(&c[1], 300)),
            None => "Background task finished.".into(),
        };
    }
    if let Some(c) = re(&CMD, r"<command-name>([\s\S]*?)</command-name>").captures(t) {
        let args = re(&ARGS, r"<command-args>([\s\S]*?)</command-args>").captures(t).map(|a| a[1].trim().to_string()).unwrap_or_default();
        return format!("{} {}", c[1].trim(), args).trim().to_string();
    }
    let out = re(&REMINDER, r"<system-reminder>[\s\S]*?</system-reminder>").replace_all(t, "").trim().to_string();
    if out.is_empty() { "(continued without a new prompt)".into() } else { out }
}

fn prompt_attachments(d: &Value) -> Vec<Att> {
    let mut out = vec![];
    for b in d["message"]["content"].as_array().into_iter().flatten() {
        if b["type"] == "image" {
            let mut a = att("in", "image", "Pasted image");
            a.media_type = s(&b["source"]["media_type"]).into();
            let data = s(&b["source"]["data"]);
            if b["source"]["type"] == "base64" && (data.len() as f64) <= IMG_MAX as f64 * 1.37 {
                a.data = data.into();
            }
            out.push(a);
        } else if b["type"] == "document" {
            let name = b["title"].as_str().or(b["source"]["media_type"].as_str()).unwrap_or("Document");
            let mut a = att("in", "document", name);
            a.media_type = s(&b["source"]["media_type"]).into();
            out.push(a);
        }
    }
    out
}

struct Cur {
    attachments: Vec<Att>,
    diffs: Vec<(String, Vec<String>)>,
    /// tool_use id -> index in `attachments`, for the URL in its result.
    artifacts: HashMap<String, usize>,
    key: String,
    session_id: String,
    prompt_id: String,
    line: usize,
    start: String,
    end: String,
    prompt: String,
    cwd: String,
    closed: bool,
    model: String,
    last_text: String,
    calls: HashMap<String, Value>,
    tools: BTreeMap<String, u32>,
    files: Vec<FileChange>,
    log: Vec<String>,
    pending: HashMap<String, String>,
    tool_ms: u64,
}

fn new_turn(i: usize, d: &Value, prompt: String) -> Cur {
    let key = [&d["uuid"], &d["promptId"], &d["timestamp"]].iter().find_map(|v| v.as_str().filter(|x| !x.is_empty())).unwrap_or("").to_string();
    Cur {
        attachments: prompt_attachments(d), diffs: vec![], artifacts: HashMap::new(), key,
        session_id: s(&d["sessionId"]).into(), prompt_id: s(&d["promptId"]).into(), line: i,
        start: s(&d["timestamp"]).into(), end: s(&d["timestamp"]).into(), cwd: s(&d["cwd"]).into(), closed: false,
        model: String::new(), last_text: String::new(), calls: HashMap::new(), tools: BTreeMap::new(), files: vec![],
        log: vec![format!("USER: {}", one_line(&prompt, 2000))], prompt, pending: HashMap::new(), tool_ms: 0,
    }
}

fn new_file_diff(content: &str) -> Vec<String> {
    std::iter::once("@@ new file @@".to_string()).chain(content.split('\n').map(|l| format!("+{l}"))).collect()
}

pub fn parse_claude(lines: &[&str], session_id: &str) -> Vec<Turn> {
    let mut turns = vec![];
    let mut cur: Option<Cur> = None;
    let finish = |cur: &mut Option<Cur>, turns: &mut Vec<Turn>| {
        let Some(c) = cur.take() else { return };
        if c.calls.is_empty() {
            return;
        }
        let mut tokens = Tokens::default();
        for u in c.calls.values() {
            tokens.input += u["input_tokens"].as_u64().unwrap_or(0);
            tokens.output += u["output_tokens"].as_u64().unwrap_or(0);
            tokens.cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
            tokens.cache_write += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        }
        turns.push(Turn {
            agent: "claude".into(),
            session_id: if c.session_id.is_empty() { session_id.into() } else { c.session_id },
            turn_key: c.key,
            duration_ms: ms(&c.start, &c.end),
            started_at: c.start,
            ended_at: c.end,
            tool_ms: c.tool_ms,
            prompt: redact(&c.prompt),
            final_text: redact(&c.last_text),
            model: c.model,
            tokens,
            model_calls: c.calls.len() as u32,
            tools: c.tools,
            files: c.files,
            diffs: finish_diffs(c.diffs),
            attachments: finish_attachments(c.attachments),
            log: trim_middle(&redact(&c.log.join("\n")), LOG_LIMIT),
            cwd: c.cwd,
            line: Some(c.line),
            base: None,
        });
    };

    for (i, d) in parse_lines(lines) {
        if d["isSidechain"] == true {
            continue;
        }
        if d["type"] == "user" && d["isCompactSummary"] == true && cur.as_ref().is_none_or(|c| c.closed) {
            // Auto-compaction mid-work: the work goes on, so it is one turn.
            finish(&mut cur, &mut turns);
            cur = Some(new_turn(i, &d, "Continued after the context was compacted.".into()));
            continue;
        }
        if is_claude_prompt(&d, cur.as_ref().map(|c| c.prompt_id.as_str()).filter(|p| !p.is_empty())) {
            finish(&mut cur, &mut turns);
            let p = clean_prompt(&user_text(&d).unwrap_or_default(), &d["origin"]);
            cur = Some(new_turn(i, &d, p));
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        let t = s(&d["timestamp"]).to_string();
        if c.closed || t.is_empty() {
            continue;
        }
        let fname = s(&d["attachment"]["filename"]);
        if d["type"] == "attachment" && d["attachment"]["type"] == "file" && !fname.is_empty() && ts(&t) - ts(&c.start) < 5000 {
            if !c.attachments.iter().any(|a| a.name == fname) {
                c.attachments.push(att("in", "file", fname));
            }
            continue;
        }
        if d["type"] == "system" && d["subtype"] == "turn_duration" {
            c.end = t;
            c.closed = true;
            continue;
        }
        if d["type"] == "assistant" {
            let m = &d["message"];
            let model = s(&m["model"]);
            if !model.is_empty() && model != "<synthetic>" {
                c.model = model.into();
            }
            if let (Some(id), true) = (m["id"].as_str(), m["usage"].is_object()) {
                c.calls.insert(id.into(), m["usage"].clone()); // the same usage repeats on every content block of one call
            }
            c.end = t.clone();
            for b in m["content"].as_array().into_iter().flatten() {
                if b["type"] == "text" && !s(&b["text"]).trim().is_empty() {
                    c.last_text = s(&b["text"]).into();
                    c.log.push(format!("{} SAID: {}", clock(&c.start, &t), one_line(&c.last_text, 600)));
                } else if b["type"] == "tool_use" {
                    let name = s(&b["name"]).to_string();
                    *c.tools.entry(name.clone()).or_default() += 1;
                    c.pending.insert(s(&b["id"]).into(), t.clone());
                    let inp = &b["input"];
                    let what = ["command", "file_path", "pattern", "description", "url", "query"].iter()
                        .map(|k| &inp[*k]).find(|v| !v.is_null()).map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).unwrap_or_default();
                    c.log.push(format!("{} {name}: {}", clock(&c.start, &t), one_line(&what, 200)));
                    if name == "SendUserFile" {
                        for f in inp["files"].as_array().into_iter().flatten() {
                            c.attachments.push(att("out", "sent", &f.as_str().map(String::from).unwrap_or_else(|| f.to_string())));
                        }
                    } else if name == "Artifact" && (inp["action"].is_null() || inp["action"] == "publish") && inp["asset"].as_bool() != Some(true) {
                        let title = inp["title"].as_str().map(String::from).or_else(|| inp["file_path"].as_str().map(base_name)).unwrap_or_default();
                        let mut a = att("out", "artifact", &title);
                        a.url = s(&inp["url"]).into();
                        c.attachments.push(a);
                        c.artifacts.insert(s(&b["id"]).into(), c.attachments.len() - 1);
                    }
                }
            }
        } else if d["type"] == "user" {
            let content = d["message"]["content"].as_array();
            if content.is_some_and(|a| a.iter().any(|b| b["type"] == "tool_result")) {
                c.end = t.clone();
            }
            for b in content.into_iter().flatten().filter(|b| b["type"] == "tool_result") {
                let id = s(&b["tool_use_id"]);
                if let Some(t0) = c.pending.get(id) {
                    c.tool_ms += ms(t0, &t);
                }
                let text = b["content"].as_str().map(String::from).unwrap_or_else(|| if b["content"].is_null() { "\"\"".into() } else { b["content"].to_string() });
                if let Some(&ai) = c.artifacts.get(id) {
                    if c.attachments[ai].url.is_empty() {
                        c.attachments[ai].url = artifact_url(&text).unwrap_or_default();
                    }
                }
                if b["is_error"] == true {
                    c.log.push(format!("{} ERROR: {}", clock(&c.start, &t), one_line(&text, 300)));
                }
            }
            // ponytail: files written through Bash (echo >, sed -i) are not counted; only Edit/Write/MultiEdit results are.
            // Upgrade path: snapshot `git status` at UserPromptSubmit and diff it at Stop.
            let r = &d["toolUseResult"];
            let fp = s(&r["filePath"]);
            if r.is_object() && !fp.is_empty() {
                let created = r["type"] == "create" && r["content"].is_string();
                match r["structuredPatch"].as_array() {
                    Some(hunks) if !hunks.is_empty() => {
                        let (mut a, mut x) = (0, 0);
                        let mut text = vec![];
                        for h in hunks {
                            let n = |k: &str| h[k].as_u64().unwrap_or(0);
                            text.push(format!("@@ -{},{} +{},{} @@", n("oldStart"), n("oldLines"), n("newStart"), n("newLines")));
                            for line in h["lines"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                                text.push(line.to_string());
                                if line.starts_with('+') { a += 1 } else if line.starts_with('-') { x += 1 }
                            }
                        }
                        add_file(&mut c.files, fp, a, x);
                        add_diff(&mut c.diffs, fp, &text);
                    }
                    Some(_) if !created => add_file(&mut c.files, fp, 0, 0),
                    _ if created => {
                        let content = s(&r["content"]);
                        add_file(&mut c.files, fp, content.split('\n').count() as u64, 0);
                        add_diff(&mut c.diffs, fp, &new_file_diff(content));
                    }
                    _ => {}
                }
            }
        }
    }
    finish(&mut cur, &mut turns);
    turns
}

// ---------- Codex ----------

fn codex_tool_names(p: &Value) -> Vec<String> {
    static R: OnceLock<Regex> = OnceLock::new();
    let input = p["input"].as_str().or(p["arguments"].as_str()).unwrap_or("");
    let name = s(&p["name"]);
    if name == "exec" || name == "js" {
        let inner: Vec<String> = re(&R, r"tools\.(\w+)\(").captures_iter(input).map(|c| c[1].to_string()).collect();
        if !inner.is_empty() {
            return inner;
        }
    }
    vec![name.to_string()]
}

pub fn patch_changes(text: &str, mut diffs: Option<&mut Vec<(String, Vec<String>)>>) -> Vec<FileChange> {
    static H: OnceLock<Regex> = OnceLock::new();
    let h = re(&H, r#"\*\*\* (Update|Add|Delete) File: (.+?)["'`]?\)?;?$"#);
    let mut files = vec![];
    let mut path = String::new();
    for raw in text.replace("\\n", "\n").split('\n') {
        let line = raw.replace("\\\"", "\"").replace("\\t", "\t");
        if let Some(c) = h.captures(&line) {
            path = c[2].trim().to_string();
            add_file(&mut files, &path, 0, 0);
            let head = match &c[1] { "Add" => "@@ new file @@", "Delete" => "@@ deleted @@", _ => "" };
            if let Some(d) = diffs.as_deref_mut() {
                let l: Vec<String> = [head].iter().filter(|x| !x.is_empty()).map(|x| x.to_string()).collect();
                add_diff(d, &path, &l);
            }
            continue;
        }
        if path.is_empty() || line.starts_with("+++") || line.starts_with("---") || line.starts_with("***") {
            if line.starts_with("*** End") {
                path.clear();
            }
            continue;
        }
        if line.starts_with('+') {
            add_file(&mut files, &path, 1, 0);
        } else if line.starts_with('-') {
            add_file(&mut files, &path, 0, 1);
        }
        if let Some(d) = diffs.as_deref_mut() {
            if line.starts_with('+') || line.starts_with('-') || line.starts_with(' ') || line.starts_with("@@") {
                add_diff(d, &path, std::slice::from_ref(&line));
            }
        }
    }
    files
}

struct CxCur {
    key: String,
    line: usize,
    start: String,
    last_ts: String,
    prompt: String,
    user_msgs: Vec<String>,
    model: String,
    tools: BTreeMap<String, u32>,
    files: Vec<FileChange>,
    log: Vec<String>,
    pending: HashMap<String, String>,
    tool_ms: u64,
    calls: u32,
    last_text: String,
    diffs: Vec<(String, Vec<String>)>,
    attachments: Vec<Att>,
    base: Tokens,
    total: Option<Tokens>,
}

fn usage_tokens(u: &Value) -> Tokens {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Tokens { input: n("input_tokens"), output: n("output_tokens"), cache_read: n("cached_input_tokens"), cache_write: n("cache_write_input_tokens") }
}

pub fn parse_codex(lines: &[&str], prev_total: Option<Tokens>, meta: (&str, &str)) -> Vec<Turn> {
    let mut turns = vec![];
    let (mut session_id, mut cwd) = (meta.0.to_string(), meta.1.to_string());
    let mut last_total = prev_total.unwrap_or_default();
    let mut cur: Option<CxCur> = None;

    let finish = |cur: &mut Option<CxCur>, end: &str, final_text: &str, turns: &mut Vec<Turn>, last_total: &mut Tokens, session_id: &str, cwd: &str| {
        let Some(c) = cur.take() else { return };
        let t = c.total.unwrap_or(*last_total);
        let b = c.base;
        let sub = |x: u64, y: u64| x.saturating_sub(y);
        let tokens = Tokens {
            input: sub(t.input.saturating_sub(t.cache_read), b.input.saturating_sub(b.cache_read)),
            output: sub(t.output, b.output),
            cache_read: sub(t.cache_read, b.cache_read),
            cache_write: sub(t.cache_write, b.cache_write),
        };
        *last_total = t;
        let mut files = vec![];
        for f in &c.files {
            add_file(&mut files, &f.path, f.add, f.del);
        }
        let mut log = vec![format!("USER: {}", one_line(&c.prompt, 2000))];
        log.extend(c.log);
        turns.push(Turn {
            agent: "codex".into(), session_id: session_id.into(), turn_key: c.key,
            started_at: c.start.clone(), ended_at: end.into(), duration_ms: ms(&c.start, end), tool_ms: c.tool_ms,
            prompt: redact(&c.prompt), final_text: redact(if final_text.is_empty() { &c.last_text } else { final_text }), model: c.model,
            tokens, model_calls: c.calls, tools: c.tools, files,
            diffs: finish_diffs(c.diffs), attachments: finish_attachments(c.attachments),
            log: trim_middle(&redact(&log.join("\n")), LOG_LIMIT), cwd: cwd.into(),
            line: Some(c.line), base: Some(c.base),
        });
    };

    for (i, d) in parse_lines(lines) {
        let p = &d["payload"];
        let dts = s(&d["timestamp"]).to_string();
        if let Some(c) = cur.as_mut() {
            if !dts.is_empty() {
                c.last_ts = dts.clone();
            }
        }
        let dt = s(&d["type"]);
        if dt == "session_meta" {
            if let Some(id) = p["id"].as_str().or(p["session_id"].as_str()) {
                session_id = id.into();
            }
            if let Some(x) = p["cwd"].as_str() {
                cwd = x.into();
            }
            continue;
        }
        if dt == "turn_context" {
            if let (Some(c), Some(m)) = (cur.as_mut(), p["model"].as_str()) {
                c.model = m.into();
            }
            if let Some(x) = p["cwd"].as_str().filter(|x| !x.is_empty()) {
                cwd = x.into();
            }
            continue;
        }
        let t = s(&p["type"]);
        if dt == "event_msg" && t == "task_started" {
            if cur.is_some() {
                finish(&mut cur, &dts, "", &mut turns, &mut last_total, &session_id, &cwd);
            }
            cur = Some(CxCur {
                key: s(&p["turn_id"]).into(), line: i, start: dts.clone(), last_ts: dts.clone(), prompt: String::new(), user_msgs: vec![],
                model: String::new(), tools: BTreeMap::new(), files: vec![], log: vec![], pending: HashMap::new(), tool_ms: 0, calls: 0,
                last_text: String::new(), diffs: vec![], attachments: vec![], base: last_total, total: None,
            });
            continue;
        }
        let Some(c) = cur.as_mut() else { continue };
        if dt == "event_msg" && t == "user_message" && !p["message"].is_null() {
            c.prompt = p["message"].as_str().map(String::from).unwrap_or_else(|| p["message"].to_string());
            continue;
        }
        let content = p["content"].as_array();
        if dt == "response_item" && t == "message" && p["role"] == "user" {
            for x in content.into_iter().flatten().filter(|x| x["type"] == "input_image") {
                let url = s(&x["image_url"]);
                let mut a = att("in", "image", "Attached image");
                if let Some((head, data)) = url.strip_prefix("data:").and_then(|r| r.split_once(";base64,")) {
                    a.media_type = head.into();
                    if (data.len() as f64) <= IMG_MAX as f64 * 1.37 {
                        a.data = data.into();
                    }
                }
                c.attachments.push(a);
            }
            if c.prompt.is_empty() {
                let txt = content.into_iter().flatten().map(|x| s(&x["text"])).collect::<Vec<_>>().join("\n").trim().to_string();
                if !txt.is_empty() && !txt.starts_with('<') {
                    c.user_msgs.push(txt);
                }
                continue;
            }
        }
        if dt == "response_item" && t == "message" && p["role"] == "assistant" {
            let txt = content.into_iter().flatten().map(|x| s(&x["text"])).collect::<Vec<_>>().join("\n").trim().to_string();
            if !txt.is_empty() {
                c.log.push(format!("{} SAID: {}", clock(&c.start, &dts), one_line(&txt, 600)));
                c.last_text = txt;
            }
            continue;
        }
        if dt == "response_item" && matches!(t, "function_call" | "custom_tool_call" | "local_shell_call") {
            static CMD: OnceLock<Regex> = OnceLock::new();
            let names = codex_tool_names(p);
            for n in &names {
                *c.tools.entry(n.clone()).or_default() += 1;
            }
            if let Some(id) = p["call_id"].as_str() {
                c.pending.insert(id.into(), dts.clone());
            }
            let input = p["input"].as_str().or(p["arguments"].as_str()).unwrap_or("").to_string();
            let changes = patch_changes(&input, Some(&mut c.diffs));
            c.files.extend(changes);
            let cmd = re(&CMD, r#"cmd:\s*["'`]([^"'`]{1,200})"#).captures(&input).map(|m| m[1].to_string()).unwrap_or(input);
            c.log.push(format!("{} {}: {}", clock(&c.start, &dts), names.join(","), one_line(&cmd, 200)));
            continue;
        }
        if dt == "response_item" && matches!(t, "function_call_output" | "custom_tool_call_output") {
            if let Some(t0) = c.pending.get(s(&p["call_id"])) {
                c.tool_ms += ms(t0, &dts);
            }
            continue;
        }
        if dt == "event_msg" && t == "token_count" && p["info"]["total_token_usage"].is_object() {
            c.total = Some(usage_tokens(&p["info"]["total_token_usage"]));
            c.calls += 1;
            continue;
        }
        if dt == "event_msg" && (t == "task_complete" || t == "turn_aborted") {
            if c.prompt.is_empty() {
                c.prompt = c.user_msgs.last().cloned().unwrap_or_default();
            }
            if t == "turn_aborted" {
                c.log.push(format!("{} ABORTED by the user", clock(&c.start, &dts)));
            }
            let last = s(&p["last_agent_message"]).to_string();
            finish(&mut cur, &dts, &last, &mut turns, &mut last_total, &session_id, &cwd);
        }
    }
    // Codex runs the Stop hook before it writes task_complete: close the open turn at its last event.
    if let Some(c) = cur.as_mut() {
        if c.prompt.is_empty() {
            c.prompt = c.user_msgs.last().cloned().unwrap_or_default();
        }
        let end = c.last_ts.clone();
        finish(&mut cur, &end, "", &mut turns, &mut last_total, &session_id, &cwd);
    }
    turns
}

// ---------- Live: questions and dialogs ----------

/// The last AskUserQuestion in a transcript that has no answer yet.
pub fn pending_ask(lines: &[&str]) -> Option<Value> {
    let mut pending: Option<(String, Value)> = None;
    for l in lines {
        if !l.contains("AskUserQuestion") && !pending.as_ref().is_some_and(|p| l.contains(&p.0)) {
            continue;
        }
        let Ok(o) = serde_json::from_str::<Value>(l) else { continue };
        for b in o["message"]["content"].as_array().into_iter().flatten() {
            if b["type"] == "tool_use" && b["name"] == "AskUserQuestion" && b["input"]["questions"].is_array() {
                pending = Some((s(&b["id"]).into(), b["input"]["questions"].clone()));
            } else if b["type"] == "tool_result" && pending.as_ref().is_some_and(|p| p.0 == s(&b["tool_use_id"])) {
                pending = None;
            }
        }
    }
    pending.map(|p| p.1)
}

/// The choices on a dialog screen, with the keys that pick each one: numbered ("❯ 1. Yes" /
/// "  2. No") take the digit; a bare cursor list ("❯ No, exit" / "  Yes, I trust") takes arrows and Enter.
pub fn screen_options(text: &str) -> Vec<substrate_proto::ScreenOption> {
    use substrate_proto::ScreenOption;
    static NUM: OnceLock<Regex> = OnceLock::new();
    static CURSOR: OnceLock<Regex> = OnceLock::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut num: Vec<ScreenOption> = vec![];
    for l in &lines {
        if let Some(m) = re(&NUM, r"^\s*([❯›>])?\s*(\d{1,2})\.\s+(.+?)\s*$").captures(l) {
            if !num.iter().any(|o| o.keys[0] == m[2]) {
                num.push(ScreenOption { label: m[3].into(), keys: vec![m[2].into()], selected: m.get(1).is_some() });
            }
        }
    }
    if num.len() >= 2 {
        return num;
    }
    let Some(at) = lines.iter().rposition(|l| re(&CURSOR, r"^\s*❯\s+\S").is_match(l)) else { return vec![] };
    // Columns in chars: the cursor and the space after it.
    let indent = lines[at].chars().position(|c| c == '❯').unwrap_or(0) + 2;
    let item = |l: &str| {
        let cs: Vec<char> = l.chars().collect();
        cs.len() > indent && cs[..indent].iter().all(|c| c.is_whitespace()) && cs[indent] != ' '
    };
    let (mut a, mut b) = (at, at);
    while a > 0 && item(lines[a - 1]) {
        a -= 1;
    }
    while b + 1 < lines.len() && item(lines[b + 1]) {
        b += 1;
    }
    if a == b {
        return vec![];
    }
    (a..=b).map(|i| {
        let d = i as i64 - at as i64;
        let mut keys: Vec<String> = std::iter::repeat_n(if d < 0 { "up" } else { "down" }, d.unsigned_abs() as usize).map(String::from).collect();
        keys.push("enter".into());
        keys.truncate(10);
        ScreenOption { label: lines[i].trim_start().trim_start_matches('❯').trim().to_string(), keys, selected: d == 0 }
    }).collect()
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use serde_json::json;

    fn l(v: &[Value]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }
    fn refs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }
    fn t(s: u32) -> String {
        format!("2026-11-13T10:{:02}:{:02}.000Z", s / 60, s % 60)
    }

    pub fn claude_lines() -> Vec<String> {
        l(&[
            json!({"type": "user", "uuid": "u1", "promptId": "p1", "sessionId": "S", "cwd": "/r", "timestamp": t(0), "message": {"content": "fix the bug"}, "origin": {"kind": "human"}}),
            json!({"type": "assistant", "timestamp": t(2), "message": {"id": "m1", "model": "claude-opus-5-5", "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 20}, "content": [{"type": "thinking"}]}}),
            json!({"type": "assistant", "timestamp": t(3), "message": {"id": "m1", "model": "claude-opus-5-5", "usage": {"input_tokens": 10, "output_tokens": 5, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 20}, "content": [{"type": "tool_use", "id": "t1", "name": "Edit", "input": {"file_path": "/r/a.ts"}}]}}),
            json!({"type": "user", "timestamp": t(5), "message": {"content": [{"type": "tool_result", "tool_use_id": "t1"}]}, "toolUseResult": {"filePath": "/r/a.ts", "structuredPatch": [{"lines": [" x", "+a", "+b", "-c"]}]}}),
            json!({"type": "assistant", "timestamp": t(8), "message": {"id": "m2", "model": "claude-opus-5-5", "usage": {"input_tokens": 1, "output_tokens": 7, "cache_read_input_tokens": 200, "cache_creation_input_tokens": 0}, "content": [{"type": "text", "text": "Fixed. The key is sk-ant-abcdefghijklmnop123."}]}}),
            json!({"type": "system", "subtype": "turn_duration", "durationMs": 8000, "timestamp": t(8)}),
            json!({"type": "user", "isMeta": true, "timestamp": t(500), "message": {"content": "<local-command-caveat>x</local-command-caveat>"}}),
            json!({"type": "user", "uuid": "u2", "promptId": "p2", "isMeta": true, "sessionId": "S", "cwd": "/r", "timestamp": t(600), "message": {"content": "Another Claude session sent a message: ..."}, "origin": {"kind": "peer", "name": "Reviewer", "body": "prod is down"}}),
            json!({"type": "assistant", "timestamp": t(605), "message": {"id": "m3", "model": "claude-opus-5-5", "usage": {"input_tokens": 1, "output_tokens": 1}, "content": [{"type": "text", "text": "Looking."}]}}),
        ])
    }

    pub fn codex_lines() -> Vec<String> {
        l(&[
            json!({"type": "session_meta", "timestamp": t(0), "payload": {"id": "CX", "cwd": "/c", "originator": "codex_cli_rs"}}),
            json!({"type": "event_msg", "timestamp": t(1), "payload": {"type": "task_started", "turn_id": "T1"}}),
            json!({"type": "turn_context", "timestamp": t(1), "payload": {"model": "gpt-6-sol", "cwd": "/c"}}),
            json!({"type": "event_msg", "timestamp": t(1), "payload": {"type": "user_message", "message": "add retries"}}),
            json!({"type": "response_item", "timestamp": t(2), "payload": {"type": "custom_tool_call", "name": "exec", "call_id": "c1", "input": "await tools.apply_patch(\"*** Begin Patch\\n*** Update File: src/x.ts\\n@@\\n-old\\n+new\\n+more\\n*** End Patch\")"}}),
            json!({"type": "response_item", "timestamp": t(4), "payload": {"type": "custom_tool_call_output", "call_id": "c1"}}),
            json!({"type": "event_msg", "timestamp": t(4), "payload": {"type": "token_count", "info": {"total_token_usage": {"input_tokens": 1000, "cached_input_tokens": 600, "output_tokens": 50}}}}),
            json!({"type": "event_msg", "timestamp": t(6), "payload": {"type": "task_complete", "turn_id": "T1", "last_agent_message": "Done."}}),
            json!({"type": "event_msg", "timestamp": t(10), "payload": {"type": "task_started", "turn_id": "T2"}}),
            json!({"type": "event_msg", "timestamp": t(10), "payload": {"type": "user_message", "message": "and tests"}}),
            json!({"type": "event_msg", "timestamp": t(12), "payload": {"type": "token_count", "info": {"total_token_usage": {"input_tokens": 3000, "cached_input_tokens": 2000, "output_tokens": 80}}}}),
            json!({"type": "event_msg", "timestamp": t(13), "payload": {"type": "task_complete", "turn_id": "T2", "last_agent_message": "Tests added."}}),
        ])
    }

    fn tok(i: u64, o: u64, r: u64, w: u64) -> Tokens {
        Tokens { input: i, output: o, cache_read: r, cache_write: w }
    }

    #[test]
    fn claude_turns() {
        let lines = claude_lines();
        let turns = parse_claude(&refs(&lines), "");
        assert_eq!(turns.len(), 2);
        let (a, b) = (&turns[0], &turns[1]);
        assert_eq!(a.tokens, tok(11, 12, 300, 20));
        assert_eq!(a.model_calls, 2);
        assert_eq!(a.ended_at, t(8), "the meta line at +500 s does not stretch it");
        assert_eq!(a.duration_ms, 8000);
        assert_eq!(a.tool_ms, 2000);
        assert_eq!(a.files, vec![FileChange { path: "/r/a.ts".into(), add: 2, del: 1 }]);
        assert_eq!(a.tools.get("Edit"), Some(&1));
        assert!(a.final_text.contains("sk-ant-[masked]") && !a.final_text.contains("abcdefghijklmnop"));
        assert_eq!(b.prompt, "Message from Reviewer: prod is down");
        assert_eq!((a.line, b.line), (Some(0), Some(7)));
        assert_eq!(a.session_id, "S");
    }

    #[test]
    fn codex_turns() {
        let lines = codex_lines();
        let turns = parse_codex(&refs(&lines), None, ("", ""));
        let (a, b) = (&turns[0], &turns[1]);
        assert_eq!(a.session_id, "CX");
        assert_eq!(a.prompt, "add retries");
        assert_eq!(a.tokens, tok(400, 50, 600, 0));
        assert_eq!(b.tokens, tok(600, 30, 1400, 0));
        assert_eq!(a.files, vec![FileChange { path: "src/x.ts".into(), add: 2, del: 1 }]);
        assert_eq!(a.tools.get("apply_patch"), Some(&1));
        assert_eq!(a.tool_ms, 2000);
        assert_eq!(b.final_text, "Tests added.");
        assert_eq!(a.diffs["src/x.ts"], "@@\n-old\n+new\n+more");
        // Stop fires before task_complete: the open turn still comes back.
        let open = refs(&lines[..7]);
        let turns = parse_codex(&open, None, ("", ""));
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].ended_at, t(4));
        assert_eq!(turns[0].tokens.output, 50);
    }

    #[test]
    fn patches_and_trim() {
        assert_eq!(patch_changes("*** Add File: a\n+1\n+2\n*** Delete File: b\n", None),
            vec![FileChange { path: "a".into(), add: 2, del: 0 }, FileChange { path: "b".into(), add: 0, del: 0 }]);
        let mut d = vec![];
        assert_eq!(patch_changes("*** Begin Patch\\n*** Update File: src/x.ts\\n@@ fn\\n-old\\n+new\\n*** End Patch", Some(&mut d)),
            vec![FileChange { path: "src/x.ts".into(), add: 1, del: 1 }]);
        let x = trim_middle(&"x".repeat(100), 60);
        assert!(x.chars().count() <= 80 && x.contains("[trimmed"));
    }

    #[test]
    fn diffs_and_attachments() {
        let lines = l(&[
            json!({"type": "user", "uuid": "x1", "promptId": "q1", "sessionId": "S2", "cwd": "/r", "timestamp": t(0), "message": {"content": [{"type": "text", "text": "look at this"}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}]}}),
            json!({"type": "attachment", "timestamp": t(0), "attachment": {"type": "file", "filename": "/r/notes.md"}}),
            json!({"type": "assistant", "timestamp": t(1), "message": {"id": "n1", "model": "m", "usage": {"output_tokens": 1}, "content": [
                {"type": "tool_use", "id": "e1", "name": "Edit", "input": {"file_path": "/r/a.ts"}},
                {"type": "tool_use", "id": "s1", "name": "SendUserFile", "input": {"files": ["/tmp/shot.png"]}},
                {"type": "tool_use", "id": "a1", "name": "Artifact", "input": {"file_path": "/tmp/page.html", "title": "Plan"}},
                {"type": "tool_use", "id": "a2", "name": "Artifact", "input": {"action": "read", "url": "https://claude.ai/artifact/READ"}},
            ]}}),
            json!({"type": "user", "timestamp": t(2), "message": {"content": [{"type": "tool_result", "tool_use_id": "e1"}]}, "toolUseResult": {"filePath": "/r/a.ts", "structuredPatch": [{"oldStart": 3, "oldLines": 1, "newStart": 3, "newLines": 2, "lines": [" ctx", "-old", "+new", "+more"]}]}}),
            json!({"type": "user", "timestamp": t(3), "message": {"content": [{"type": "tool_result", "tool_use_id": "a1", "content": "Updated the Artifact at https://claude.ai/artifact/AbC123 (Version 2)"}]}}),
        ]);
        let turns = parse_claude(&refs(&lines), "");
        let tt = &turns[0];
        assert_eq!(tt.diffs["/r/a.ts"], "@@ -3,1 +3,2 @@\n ctx\n-old\n+new\n+more");
        let mut img = att("in", "image", "Pasted image");
        img.media_type = "image/png".into();
        img.data = "iVBORw0KGgo=".into();
        let mut art = att("out", "artifact", "Plan");
        art.url = "https://claude.ai/artifact/AbC123".into();
        assert_eq!(tt.attachments, vec![img, att("in", "file", "/r/notes.md"), att("out", "sent", "/tmp/shot.png"), art]);
    }

    #[test]
    fn live_questions_and_dialogs() {
        let q = json!([{"question": "Which DB?", "header": "DB", "multiSelect": false, "options": [{"label": "SQLite"}, {"label": "Postgres"}]}]);
        let use_ = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "a1", "name": "AskUserQuestion", "input": {"questions": q}}]}}).to_string();
        let res = json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "a1", "content": "answered"}]}}).to_string();
        assert_eq!(pending_ask(&[&use_]), Some(q));
        assert_eq!(pending_ask(&[&use_, &res]), None);
        let o: Vec<_> = screen_options(" Allow?\n ❯ 1. Yes\n   2. Yes, and don't ask again\n   3. No\n").into_iter().map(|o| (o.label, o.keys, o.selected)).collect();
        assert_eq!(o, vec![("Yes".into(), vec!["1".to_string()], true), ("Yes, and don't ask again".into(), vec!["2".into()], false), ("No".into(), vec!["3".into()], false)]);
        let k: Vec<_> = screen_options(" Trust it?\n\n ❯ No, exit\n   Yes, I trust this folder\n\n Enter to confirm").into_iter().map(|o| o.keys).collect();
        assert_eq!(k, vec![vec!["enter".to_string()], vec!["down".into(), "enter".into()]]);
        assert!(screen_options("❯ \n just output").is_empty());
    }
}
