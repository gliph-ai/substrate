//! The agents' hooks (`substrate hook claude|codex|claude-ask`), their install, and the
//! backfill of past transcripts. A hook runs inside the agent's turn: fast, quiet, no
//! network, always exit 0. It only parses the transcript into `worklog/queue/`.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use substrate_proto::{LogRepo, Tokens};

use super::parse::{parse_claude, parse_codex, Att, Turn, IMG_MAX};
use super::{dir, project_for, sha};
use crate::config::{write_private, Paths};

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Offset {
    pub offset: u64,
    #[serde(default)]
    pub base: Option<Tokens>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub cwd: String,
}

/// One file in `worklog/queue/`: turns of one repo, for the daemon to upload.
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Batch {
    pub summarize: bool,
    pub repo: LogRepo,
    pub turns: Vec<Turn>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}

/// Parse the transcript from the stored offset. The next offset is the start of the
/// last turn, so a turn that a Stop hook lets continue is read again in full.
pub fn read_turns(agent: &str, path: &Path, prev: Option<&Offset>, first_run_last_only: bool) -> Result<(Vec<Turn>, Offset)> {
    let mut f = std::fs::File::open(path)?;
    let size = f.metadata()?.len();
    let start = prev.map(|p| p.offset).filter(|&o| o <= size).unwrap_or(0); // shrank or replaced: start over
    f.seek(SeekFrom::Start(start))?;
    let mut buf = vec![];
    f.read_to_end(&mut buf)?;
    let mut line_start = vec![];
    let mut pos = start;
    let owned: Vec<String> = buf.split(|&b| b == b'\n').map(|l| {
        line_start.push(pos);
        pos += l.len() as u64 + 1;
        String::from_utf8_lossy(l).into_owned()
    }).collect();
    let lines: Vec<&str> = owned.iter().map(String::as_str).collect();
    let (mut sid, mut cwd) = (prev.map(|p| p.session_id.clone()).unwrap_or_default(), prev.map(|p| p.cwd.clone()).unwrap_or_default());
    if agent == "codex" && sid.is_empty() {
        if let Some(m) = lines.first().and_then(|l| serde_json::from_str::<Value>(l).ok()).filter(|m| m["type"] == "session_meta") {
            sid = m["payload"]["id"].as_str().or(m["payload"]["session_id"].as_str()).unwrap_or("").into();
            cwd = m["payload"]["cwd"].as_str().unwrap_or("").into();
        }
    }
    let mut turns = if agent == "claude" { parse_claude(&lines, "") } else { parse_codex(&lines, prev.and_then(|p| p.base), (&sid, &cwd)) };
    let next = match turns.last() {
        Some(t) if t.line.is_some() => Offset { offset: line_start[t.line.unwrap()], base: t.base, session_id: sid, cwd: if t.cwd.is_empty() { cwd } else { t.cwd.clone() } },
        _ => Offset { offset: prev.map(|p| p.offset).unwrap_or(0), base: prev.and_then(|p| p.base), session_id: sid, cwd },
    };
    if prev.is_none() && first_run_last_only && turns.len() > 1 {
        turns.drain(..turns.len() - 1); // history is for `substrate log backfill`
    }
    Ok((turns, next))
}

pub fn find_codex_transcript(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() {
        return None;
    }
    // Newest days first; rollout file names end with the session id.
    fn walk(d: &Path, depth: u32, sid: &str) -> Option<PathBuf> {
        let mut names: Vec<PathBuf> = std::fs::read_dir(d).ok()?.flatten().map(|e| e.path()).collect();
        names.sort();
        names.reverse();
        for p in names {
            if depth < 3 {
                if let Some(r) = walk(&p, depth + 1, sid) {
                    return Some(r);
                }
            } else if p.to_string_lossy().ends_with(&format!("{sid}.jsonl")) {
                return Some(p);
            }
        }
        None
    }
    walk(&home().join(".codex/sessions"), 0, session_id)
}

pub fn claude_transcript(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains('/') {
        return None;
    }
    std::fs::read_dir(home().join(".claude/projects")).ok()?.flatten().map(|e| e.path().join(format!("{session_id}.jsonl"))).find(|p| p.exists())
}

pub fn tail_lines(path: &Path, bytes: u64) -> Vec<String> {
    let Ok(mut f) = std::fs::File::open(path) else { return vec![] };
    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = size.saturating_sub(bytes);
    let mut buf = vec![];
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_to_end(&mut buf).is_err() {
        return vec![];
    }
    let mut lines: Vec<String> = String::from_utf8_lossy(&buf).split('\n').map(String::from).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

fn image_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit('.').next()?.to_lowercase();
    Some(match ext.as_str() { "png" => "image/png", "jpg" | "jpeg" => "image/jpeg", "gif" => "image/gif", "webp" => "image/webp", _ => return None })
}

/// Images the agent handed to the user: the hook runs on the same machine, so it keeps a copy.
pub fn embed_sent_images(atts: &mut [Att]) {
    for a in atts.iter_mut().filter(|a| a.kind == "sent" && a.data.is_empty()) {
        let Some(mt) = image_type(&a.name) else { continue };
        if std::fs::metadata(&a.name).map(|m| m.len() as usize > IMG_MAX).unwrap_or(true) {
            continue;
        }
        if let Ok(b) = std::fs::read(&a.name) {
            a.data = base64::engine::general_purpose::STANDARD.encode(b);
            a.media_type = mt.into();
        }
    }
}

/// Group turns by repo (a session can cd into another repo, rare) and queue them.
pub fn queue(paths: &Paths, mut turns: Vec<Turn>, cwd_hint: &str, summarize: bool) -> Result<()> {
    let mut groups: Vec<Batch> = vec![];
    for mut t in turns.drain(..) {
        embed_sent_images(&mut t.attachments);
        let repo = project_for(if t.cwd.is_empty() { cwd_hint } else { &t.cwd });
        match groups.iter_mut().find(|g| g.repo.key == repo.key) {
            Some(g) => g.turns.push(t),
            None => groups.push(Batch { summarize, repo, turns: vec![t] }),
        }
    }
    for g in groups {
        for chunk in g.turns.chunks(40) {
            let b = Batch { summarize, repo: g.repo.clone(), turns: chunk.to_vec() };
            let name = format!("{}-{:08x}.json", chrono::Utc::now().timestamp_millis(), rand::random::<u32>());
            write_private(&dir(paths, "queue").join(name), &serde_json::to_vec(&b)?)?;
        }
    }
    Ok(())
}

fn read_stdin() -> Value {
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    serde_json::from_str(&s).unwrap_or_default()
}

/// `substrate hook <agent>`. Never fails the agent: errors go to worklog/hook-error.log.
pub fn run(paths: &Paths, which: &str) {
    if std::env::var("SUBSTRATE_CHILD").is_ok() {
        return; // our own summariser
    }
    let r = match which {
        "claude-ask" => ask_hook(paths),
        "codex" if std::env::var("CLAUDECODE").as_deref() == Ok("1") => Ok(()), // codex run from a Claude turn belongs to that turn
        "claude" | "codex" => stop_hook(paths, which),
        _ => Err(anyhow!("hook claude|codex|claude-ask")),
    };
    if let Err(e) = r {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir(paths, "").join("hook-error.log")) {
            let _ = writeln!(f, "{} {which} {e:#}", chrono::Utc::now().to_rfc3339());
        }
    }
}

fn stop_hook(paths: &Paths, agent: &str) -> Result<()> {
    let p = read_stdin();
    let mut transcript = p["transcript_path"].as_str().map(PathBuf::from);
    if transcript.is_none() && agent == "codex" {
        transcript = find_codex_transcript(p["session_id"].as_str().or(p["thread_id"].as_str()).unwrap_or(""));
    }
    let Some(transcript) = transcript.filter(|t| t.exists()) else { return Ok(()) };
    let off_file = dir(paths, "offsets").join(format!("{}.json", sha(&transcript.to_string_lossy())));
    let prev: Option<Offset> = std::fs::read(&off_file).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let (mut turns, next) = read_turns(agent, &transcript, prev.as_ref(), true)?;
    if let (Some(last), Some(m)) = (turns.last_mut(), p["last_assistant_message"].as_str()) {
        if last.final_text.is_empty() {
            last.final_text = super::redact(m);
        }
    }
    queue(paths, turns, p["cwd"].as_str().unwrap_or(""), true)?;
    write_private(&off_file, &serde_json::to_vec(&next)?)?;
    Ok(())
}

/// An open question, kept for the daemon: from the PreToolUse hook on AskUserQuestion
/// (Claude writes a pending tool call to the transcript only after it is answered).
#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Ask {
    pub questions: Value,
    #[serde(default)]
    pub context: String,
    #[serde(default)]
    pub files: Vec<Att>,
    #[serde(default)]
    pub transcript: String,
    /// The transcript was read again once the dialog showed (the hook runs before Claude flushes).
    #[serde(default)]
    pub reread: bool,
}

pub fn ask_path(paths: &Paths, session: &str) -> PathBuf {
    dir(paths, "asks").join(format!("{}.json", substrate_proto::safe_filename(session)))
}

/// What the agent showed before it asked, in this turn: its text and the files and
/// artifacts it handed over (sent images embedded, 8 MB in all).
pub fn ask_extras(transcript: &Path) -> (String, Vec<Att>) {
    let Ok(size) = std::fs::metadata(transcript).map(|m| m.len()) else { return Default::default() };
    // Read back far enough to hold the current prompt: a window that parses to any turn holds it.
    let mut read = 512_000u64;
    let mut turns;
    loop {
        let lines = tail_lines(transcript, read);
        turns = parse_claude(&lines.iter().map(String::as_str).collect::<Vec<_>>(), "");
        if !turns.is_empty() || read >= size {
            break;
        }
        read = if read == 512_000 { 4_000_000 } else { read * 4 };
    }
    let Some(t) = turns.pop() else { return Default::default() };
    let mut att: Vec<Att> = t.attachments.into_iter().filter(|a| a.dir == "out").collect();
    let skip = att.len().saturating_sub(12);
    att.drain(..skip);
    embed_sent_images(&mut att);
    let mut budget = 8_000_000usize; // ponytail: per question; larger images go without a preview
    for a in att.iter_mut().filter(|a| !a.data.is_empty()) {
        if a.data.len() > budget { a.data.clear() } else { budget -= a.data.len() }
    }
    let ctx: Vec<char> = t.final_text.chars().collect();
    (ctx[ctx.len().saturating_sub(4000)..].iter().collect(), att)
}

fn ask_hook(paths: &Paths) -> Result<()> {
    let p = read_stdin();
    let sid = p["session_id"].as_str().unwrap_or("");
    if p["tool_name"] != "AskUserQuestion" || !p["tool_input"]["questions"].is_array() || sid.is_empty() {
        return Ok(());
    }
    let tp = p["transcript_path"].as_str().filter(|t| t.ends_with(".jsonl")).unwrap_or("");
    let (context, files) = if tp.is_empty() { Default::default() } else { ask_extras(Path::new(tp)) };
    let a = Ask { questions: p["tool_input"]["questions"].clone(), context, files, transcript: tp.into(), reread: false };
    write_private(&ask_path(paths, sid), &serde_json::to_vec(&a)?)
}

// ---------- install ----------

fn hook_cmd(exe: &str, which: &str) -> String {
    format!("\"{exe}\" hook {which}")
}

fn is_ours(h: &Value) -> bool {
    h["command"].as_str().is_some_and(|c| c.contains("substrate") && [" hook claude", " hook codex", " hook claude-ask"].iter().any(|e| c.ends_with(e)))
}

/// (event, matcher, hook argument, timeout) per agent.
fn hooks_for(agent: &str) -> &'static [(&'static str, &'static str, &'static str, u32)] {
    match agent {
        "claude" => &[("Stop", "", "claude", 15), ("PreToolUse", "AskUserQuestion", "claude-ask", 5)],
        _ => &[("Stop", "", "codex", 15)],
    }
}

/// Add our hooks to one settings file; others stay. Returns whether it changed.
pub fn add_hooks(v: &mut Value, agent: &str, exe: &str) -> Result<bool> {
    let hooks = v.as_object_mut().ok_or_else(|| anyhow!("the settings file is not an object"))?.entry("hooks").or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or_else(|| anyhow!("hooks is not an object"))?;
    let mut changed = false;
    for (event, matcher, which, timeout) in hooks_for(agent) {
        let list = hooks.entry(*event).or_insert_with(|| json!([])).as_array_mut().ok_or_else(|| anyhow!("{event} is not a list"))?;
        let want = hook_cmd(exe, which);
        let mut found = false;
        for g in list.iter_mut() {
            for h in g["hooks"].as_array_mut().into_iter().flatten().filter(|h| is_ours(h) && h["command"].as_str().is_some_and(|c| c.ends_with(&format!(" hook {which}")))) {
                found = true;
                if h["command"] != want.as_str() {
                    h["command"] = json!(want); // the binary moved
                    changed = true;
                }
            }
        }
        if !found {
            let mut g = json!({"hooks": [{"type": "command", "command": want, "timeout": timeout}]});
            if !matcher.is_empty() {
                g["matcher"] = json!(matcher);
            }
            list.push(g);
            changed = true;
        }
    }
    Ok(changed)
}

pub fn remove_hooks(v: &mut Value) -> bool {
    let mut changed = false;
    let Some(hooks) = v["hooks"].as_object_mut() else { return false };
    for list in hooks.values_mut() {
        let Some(groups) = list.as_array_mut() else { continue };
        for g in groups.iter_mut() {
            if let Some(hs) = g["hooks"].as_array_mut() {
                let n = hs.len();
                hs.retain(|h| !is_ours(h));
                changed |= hs.len() != n;
            }
        }
        groups.retain(|g| g["hooks"].as_array().is_none_or(|h| !h.is_empty()));
    }
    hooks.retain(|_, l| l.as_array().is_none_or(|a| !a.is_empty()));
    if hooks.is_empty() {
        v.as_object_mut().map(|o| o.remove("hooks"));
    }
    changed
}

/// The settings files and their agent. Codex only when it is installed.
pub fn targets() -> Vec<(&'static str, PathBuf)> {
    let mut t = vec![("claude", home().join(".claude/settings.json"))];
    if home().join(".codex").exists() {
        t.push(("codex", home().join(".codex/hooks.json")));
    }
    t
}

fn edit(file: &Path, f: impl FnOnce(&mut Value) -> Result<bool>) -> Result<bool> {
    let raw = std::fs::read_to_string(file).unwrap_or_else(|_| "{}".into());
    let mut v: Value = serde_json::from_str(&raw)?;
    if !f(&mut v)? {
        return Ok(false);
    }
    if file.exists() && !file.with_extension("json.before-substrate-log").exists() {
        std::fs::copy(file, file.with_extension("json.before-substrate-log"))?;
    }
    std::fs::create_dir_all(file.parent().unwrap())?;
    std::fs::write(file, serde_json::to_string_pretty(&v)? + "\n")?;
    Ok(true)
}

pub fn install(exe: &str) -> Result<Vec<String>> {
    let mut out = vec![];
    for (agent, file) in targets() {
        let changed = edit(&file, |v| add_hooks(v, agent, exe))?;
        out.push(format!("{agent}: {} ({})", if changed { "hooks added" } else { "hooks already there" }, file.display()));
        if changed && agent == "codex" {
            out.push("codex: approve the new hook once in Codex under /hooks.".into());
        }
    }
    Ok(out)
}

pub fn uninstall() -> Result<Vec<String>> {
    targets().into_iter().map(|(agent, file)| {
        let changed = file.exists() && edit(&file, |v| Ok(remove_hooks(v)))?;
        Ok(format!("{agent}: {}", if changed { "hooks removed" } else { "no Substrate log hooks" }))
    }).collect()
}

/// (agent, installed) per settings file.
pub fn state() -> Vec<(&'static str, bool)> {
    targets().into_iter().map(|(agent, file)| {
        let v: Value = std::fs::read(file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let on = v["hooks"]["Stop"].as_array().into_iter().flatten().any(|g| g["hooks"].as_array().into_iter().flatten().any(is_ours));
        (agent, on)
    }).collect()
}

// ---------- backfill ----------

pub fn parse_since(s: &str) -> Result<i64> {
    let (n, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = n.parse().map_err(|_| anyhow!("--since takes a value like 7d, 12h or 2w, not \"{s}\""))?;
    let secs = match unit { "h" => 3600, "d" => 86400, "w" => 7 * 86400, _ => return Err(anyhow!("--since takes a value like 7d, 12h or 2w, not \"{s}\"")) };
    Ok(super::now_s() - n * secs)
}

fn files(root: &Path, depth: u32, pick: &dyn Fn(&str) -> bool, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        match e.file_type() {
            Ok(t) if t.is_dir() && depth > 0 => files(&p, depth - 1, pick, out),
            Ok(t) if t.is_file() && pick(&name) => out.push(p),
            _ => {}
        }
    }
}

/// Queue the turns of every transcript still on disk. Stats only, unless `summarize`
/// (then only the turns that ended after `since`, if given).
pub fn backfill(paths: &Paths, summarize: bool, since: Option<i64>, include_exec: bool) -> Result<String> {
    let mut srcs = vec![];
    files(&home().join(".claude/projects"), 1, &|n| n.ends_with(".jsonl"), &mut srcs);
    let claude_n = srcs.len();
    files(&home().join(".codex/sessions"), 3, &|n| n.starts_with("rollout-") && n.ends_with(".jsonl"), &mut srcs);
    let work = dir(paths, "work").to_string_lossy().to_string();
    let (mut seen, mut skipped, mut failed, mut queued) = (0, 0, 0, 0);
    for (i, src) in srcs.iter().enumerate() {
        let agent = if i < claude_n { "claude" } else { "codex" };
        if agent == "codex" && !include_exec {
            let head = tail_or_head(src);
            if serde_json::from_str::<Value>(&head).ok().is_some_and(|m| m["payload"]["originator"] == "codex_exec") {
                skipped += 1; // hook-driven reviews, not your work
                continue;
            }
        }
        let Ok((turns, next)) = read_turns(agent, src, None, false) else { failed += 1; continue };
        let turns: Vec<Turn> = turns.into_iter().filter(|t| !t.cwd.is_empty() && !t.cwd.starts_with(&work)).collect();
        seen += turns.len();
        let ended = |t: &Turn| chrono::DateTime::parse_from_rfc3339(&t.ended_at).map(|d| d.timestamp()).unwrap_or(0);
        let (sum, rest): (Vec<Turn>, Vec<Turn>) = turns.into_iter().partition(|t| summarize && since.is_none_or(|s| ended(t) >= s));
        queued += sum.len();
        queue(paths, rest, "", false)?;
        queue(paths, sum, "", true)?;
        // The live hook continues from the last turn.
        write_private(&dir(paths, "offsets").join(format!("{}.json", sha(&src.to_string_lossy()))), &serde_json::to_vec(&next)?)?;
    }
    Ok(format!("Backfill: {} transcripts, {seen} turns queued for upload, {queued} of them for a summary, {skipped} codex exec sessions skipped{}. The relay uploads them; check with `substrate log status`.",
        srcs.len(), if failed > 0 { format!(", {failed} unreadable") } else { String::new() }))
}

fn tail_or_head(p: &Path) -> String {
    let mut s = String::new();
    let _ = std::fs::File::open(p).and_then(|f| f.take(20_000).read_to_string(&mut s));
    s.lines().next().unwrap_or("").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_keeps_other_hooks_and_is_idempotent() {
        let mut v = json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "bash ~/.claude/hooks/codex-review.sh"}]}]}, "model": "opus"});
        assert!(add_hooks(&mut v, "claude", "/bin/substrate").unwrap());
        let once = v.clone();
        assert!(!add_hooks(&mut v, "claude", "/bin/substrate").unwrap());
        assert_eq!(v, once);
        assert_eq!(v["hooks"]["Stop"].as_array().unwrap().len(), 2);
        assert_eq!(v["hooks"]["PreToolUse"][0]["matcher"], "AskUserQuestion");
        assert!(add_hooks(&mut v, "claude", "/new/substrate").unwrap(), "a moved binary updates the command");
        assert!(v["hooks"]["Stop"][1]["hooks"][0]["command"].as_str().unwrap().starts_with("\"/new/substrate\""));
        assert!(remove_hooks(&mut v));
        assert_eq!(v, json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": "bash ~/.claude/hooks/codex-review.sh"}]}]}, "model": "opus"}));
    }

    #[test]
    fn offsets_reread_the_last_turn() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("t.jsonl");
        let lines = super::super::parse::tests::claude_lines();
        std::fs::write(&f, lines[..6].join("\n") + "\n").unwrap();
        let (t1, n1) = read_turns("claude", &f, None, true).unwrap();
        assert_eq!((t1.len(), n1.offset), (1, 0));
        std::fs::write(&f, lines.join("\n") + "\n").unwrap();
        let (t2, n2) = read_turns("claude", &f, Some(&n1), true).unwrap();
        assert_eq!(t2.iter().map(|t| t.turn_key.as_str()).collect::<Vec<_>>(), ["u1", "u2"]);
        assert_eq!(n2.offset as usize, lines[..7].join("\n").len() + 1);
    }
}
