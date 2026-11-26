//! Live agents: what a blocked agent asks, and the owner's answer back into the pane.
//! Herdr knows each pane's state; the `claude-ask` hook knows an AskUserQuestion is open.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use substrate_proto::{AskFile, AskOption, AskQuestion, LiveView, Pane, AGENT_KEYS};

use super::hook::{ask_extras, ask_path, claude_transcript, tail_lines, Ask};
use super::parse::{pending_ask, screen_options};
use super::redact;
use crate::config::{write_private, Paths};
use crate::herdr::{Herdr, PromptError};

const RASTER: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

fn load_ask(paths: &Paths, sid: &str) -> Option<Ask> {
    if sid.is_empty() {
        return None;
    }
    std::fs::read(ask_path(paths, sid)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn drop_ask(paths: &Paths, sid: &str) {
    if !sid.is_empty() {
        let _ = std::fs::remove_file(ask_path(paths, sid));
    }
}

/// Claude's and Codex's dialogs end with a key hint line that has "Esc to cancel".
pub fn dialog_open(h: &dyn Herdr, pane: &str) -> bool {
    let text = h.screen(pane).unwrap_or_default();
    let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    tail[tail.len().saturating_sub(4)..].join("\n").to_lowercase().contains("esc to cancel")
}

fn cut(s: &str, n: usize) -> String {
    substrate_proto::sanitize(s, n)
}

pub fn questions(v: &Value) -> Vec<AskQuestion> {
    v.as_array().into_iter().flatten().take(8).map(|q| AskQuestion {
        question: cut(q["question"].as_str().unwrap_or(""), 2000),
        header: cut(q["header"].as_str().unwrap_or(""), 40),
        multi_select: q["multiSelect"] == true,
        options: q["options"].as_array().into_iter().flatten().take(12).map(|o| AskOption {
            label: cut(o["label"].as_str().unwrap_or(""), 300),
            description: cut(o["description"].as_str().unwrap_or(""), 500),
            preview: cut(o["preview"].as_str().unwrap_or(""), 8000),
        }).collect(),
    }).collect()
}

/// The pane's last lines, Nerd Font icons and full-width rules cut down, secrets masked.
pub fn clean_screen(text: &str) -> String {
    let no_icons: String = text.chars().filter(|c| !matches!(*c as u32, 0xE000..=0xF8FF | 0xF0000..=0xFFFFD)).collect();
    let mut lines: Vec<String> = no_icons.split('\n').map(|l| {
        let rule = l.chars().count() >= 20 && l.chars().all(|c| c.is_whitespace() || matches!(c, '─' | '━' | '═'));
        if rule { "────".to_string() } else { l.trim_end().to_string() }
    }).collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    redact(&lines[lines.len().saturating_sub(30)..].join("\n"))
}

/// Fill `live` on each blocked pane. A question the hook saw keeps a pane blocked while its
/// dialog shows (Herdr reads some dialogs as done), and goes once the pane leaves blocked.
pub fn enrich(paths: &Paths, h: &dyn Herdr, panes: &mut [Pane], prev: &mut HashMap<String, String>) {
    for p in panes.iter_mut() {
        let sid = p.agent_session.clone();
        let mut ask = load_ask(paths, &sid);
        if ask.is_some() && !matches!(p.status.as_str(), "blocked" | "working") {
            if dialog_open(h, &p.pane_id) {
                p.status = "blocked".into();
            } else {
                drop_ask(paths, &sid);
                ask = None;
            }
        }
        if prev.get(&p.pane_id).is_some_and(|s| s == "blocked") && p.status != "blocked" {
            drop_ask(paths, &sid); // the dialog closed: answered or cancelled
            ask = None;
        }
        prev.insert(p.pane_id.clone(), p.status.clone());
        if p.status != "blocked" {
            continue;
        }
        let mut lv = LiveView::default();
        if let Some(mut a) = ask {
            // The hook runs before Claude flushes the turn's last lines; once the dialog shows they are on disk.
            if !a.reread && a.context.is_empty() && a.files.is_empty() && !a.transcript.is_empty() {
                (a.context, a.files) = ask_extras(Path::new(&a.transcript));
                a.reread = true;
                let _ = write_private(&ask_path(paths, &sid), &serde_json::to_vec(&a).unwrap_or_default());
            }
            lv.ask = questions(&a.questions);
            lv.context = redact(&a.context);
            lv.files = a.files.iter().map(|f| AskFile {
                kind: f.kind.clone(),
                name: f.name.chars().take(300).collect(),
                // Artifacts link out only to claude.ai.
                url: if f.kind == "artifact" && f.url.starts_with("https://claude.ai/") { f.url.clone() } else { String::new() },
                media_type: if !f.data.is_empty() && RASTER.contains(&f.media_type.as_str()) && f.data.len() <= 2_100_000 { f.media_type.clone() } else { String::new() },
            }).collect();
        } else if p.agent == "claude" {
            // Older Claude versions write the pending question early.
            if let Some(q) = claude_transcript(&sid).and_then(|tp| pending_ask(&tail_lines(&tp, 512_000).iter().map(String::as_str).collect::<Vec<_>>())) {
                lv.ask = questions(&q);
            }
        }
        if lv.ask.is_empty() {
            lv.screen = clean_screen(&h.screen(&p.pane_id).unwrap_or_default());
            lv.options = screen_options(&lv.screen);
        }
        p.live = Some(lv);
    }
    prev.retain(|k, _| panes.iter().any(|p| &p.pane_id == k));
}

/// op `ask_file`: an image the agent showed before it asked.
pub fn ask_file(paths: &Paths, pane: Option<&Pane>, index: usize) -> (u16, Value) {
    let Some(p) = pane else { return (404, json!({"error": "that agent is gone"})) };
    let f = load_ask(paths, &p.agent_session).and_then(|a| a.files.into_iter().nth(index));
    match f {
        Some(f) if !f.data.is_empty() && RASTER.contains(&f.media_type.as_str()) && f.data.len() <= 2_100_000 => (200, json!({"media_type": f.media_type, "data": f.data})),
        _ => (404, json!({"error": "no such file"})),
    }
}

/// op `agent_keys`.
pub fn keys(h: &dyn Herdr, pane: Option<&Pane>, args: &Value) -> (u16, Value) {
    let Some(p) = pane else { return (404, json!({"error": "that agent is gone"})) };
    let ks: Vec<String> = args["keys"].as_array().into_iter().flatten().filter_map(|k| k.as_str().map(String::from)).collect();
    if ks.is_empty() || ks.len() > 10 || ks.len() != args["keys"].as_array().map(|a| a.len()).unwrap_or(0) || !ks.iter().all(|k| AGENT_KEYS.contains(&k.as_str())) {
        return (400, json!({"error": "keys must be 1 to 10 of: a digit, esc, enter, up, down, left, right, tab, space, y, n"}));
    }
    match h.send_keys(&p.pane_id, &ks) {
        Ok(()) => (200, json!({"ok": true})),
        Err(e) => (502, json!({"error": e.to_string()})),
    }
}

/// op `agent_prompt`: a message for the agent. An open AskUserQuestion is cancelled first and
/// the answer goes in as the next prompt: key-driving that dialog breaks with each Claude Code release.
pub fn prompt(paths: &Paths, h: &dyn Herdr, pane: Option<&Pane>, args: &Value) -> (u16, Value) {
    let Some(p) = pane else { return (404, json!({"error": "that agent is gone"})) };
    let raw = args["text"].as_str().unwrap_or("");
    let text = substrate_proto::sanitize(raw, 20_000);
    if text.is_empty() || raw.chars().count() > 20_000 {
        return (400, json!({"error": "send 1 to 20000 characters of text"}));
    }
    // The pane as the last poll saw it (at most 2 s old), with the hook's question folded in.
    if p.status == "working" {
        return (409, json!({"error": "the agent is working; send when it is done"}));
    }
    if p.status == "blocked" {
        if p.live.as_ref().is_none_or(|l| l.ask.is_empty()) {
            return (409, json!({"error": "the agent shows a dialog; pick one of its choices first"}));
        }
        let _ = h.send_keys(&p.pane_id, &["esc".into()]);
        // The answer must not go into a dialog that is still open; Herdr's state alone is not enough.
        let mut ok = false;
        for _ in 0..16 {
            std::thread::sleep(Duration::from_millis(500));
            let st = h.status(&p.pane_id).unwrap_or_default();
            if matches!(st.as_str(), "idle" | "done") && !dialog_open(h, &p.pane_id) {
                ok = true;
                break;
            }
        }
        if !ok {
            return (409, json!({"error": "the dialog did not close; nothing was sent. Look at the pane and try again"}));
        }
        drop_ask(paths, &p.agent_session);
    }
    match h.prompt(&p.pane_id, &text) {
        Ok(()) => (200, json!({"ok": true})),
        Err(PromptError::Blocked) => (409, json!({"error": "the agent shows a dialog; pick one of its choices first"})),
        Err(PromptError::NotReady) => (409, json!({"error": "the agent is not ready for input; try again in a moment"})),
        Err(PromptError::Gone) => (404, json!({"error": "that agent is gone"})),
        Err(PromptError::Other(e)) => (502, json!({"error": e})),
    }
}
