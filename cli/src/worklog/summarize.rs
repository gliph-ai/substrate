//! The summary of one turn, written on this Mac by the CLI of the agent that ran it,
//! headless and quiet: `claude -p` with Haiku, or `codex exec` with the default model.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use substrate_proto::{Decision, Step, Summary, Tokens, Watch};

use super::parse::Turn;

pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["headline", "summary", "verdict", "steps", "decisions", "watch"],
        "properties": {
            "headline": {"type": "string", "description": "One sentence, at most 20 words: the main result of the turn."},
            "summary": {"type": "array", "items": {"type": "string"}, "description": "1 to 6 sentences. What was done and the result."},
            "verdict": {"type": "string", "enum": ["clean", "watch", "broken"]},
            "steps": {"type": "array", "items": {
                "type": "object", "additionalProperties": false, "required": ["at", "title", "text", "tone"],
                "properties": {
                    "at": {"type": "string", "description": "Time from the prompt as +m:ss, copied from the log."},
                    "title": {"type": "string"}, "text": {"type": "string"},
                    "tone": {"type": "string", "enum": ["neutral", "good", "problem"]}
                }
            }},
            "decisions": {"type": "array", "items": {
                "type": "object", "additionalProperties": false, "required": ["text", "why", "alternative"],
                "properties": {"text": {"type": "string"}, "why": {"type": "string"}, "alternative": {"type": "string"}}
            }},
            "watch": {"type": "array", "items": {
                "type": "object", "additionalProperties": false, "required": ["text", "impact"],
                "properties": {"text": {"type": "string"}, "impact": {"type": "string", "enum": ["high", "medium", "low"]}}
            }}
        }
    })
}

pub const SYSTEM: &str = r#"You write the work log for a coding agent turn. The reader is the developer who owns the project. They did not watch the turn.

Write in ASD-STE100 Simplified Technical English, about 80% strict:
- One statement in each sentence. At most 20 words in a sentence.
- Use the active voice. Use the simple present or simple past tense.
- Use common, simple words. Use one word for one meaning. No idioms, no marketing words, no filler.
- Keep technical names exactly as they are: file paths, commands, function names, product names.
- Do not write "the agent" in every sentence. Write what changed: "The log stream stays open." not "The agent made the log stream stay open."

Fields:
- headline: the main result in one sentence. For a question with no code change, write the answer: "Answered: <answer>."
- summary: 1 to 6 sentences. The first sentence is the headline again or close to it. Include test results when the log shows them. A short question gets 1 sentence.
- verdict: "broken" when tests, a build or a deploy fail at the end of the turn, or the work stopped unfinished. "watch" when there is at least one watch item. Else "clean".
- steps: 3 to 8 steps that tell the story, with the "+m:ss" time from the log. Tone "problem" for failures, "good" for a passing check or a finished change, else "neutral". Use [] for a trivial turn.
- decisions: only real choices the agent or the user made in this turn, between two or more ways to do something. Give the reason (why) and the option that was not chosen (alternative; "" if the log does not say). Do not invent decisions. Use [] when there are none.
- watch: things the developer must know or do later: a risk, an open problem, a manual step, a follow-up, a failing test that was left. Plain words are allowed here; be precise. Impact: high = breaks production or loses data, medium = needs action soon, low = good to know. Use [] when there is nothing.

Use only facts from the input. If the input does not show a result, do not claim it."#;

pub fn build_prompt(t: &Turn, project: &str) -> String {
    let files = if t.files.is_empty() { "none".to_string() } else { t.files.iter().map(|f| format!("{} (+{} -{})", f.path, f.add, f.del)).collect::<Vec<_>>().join("\n") };
    let tools = if t.tools.is_empty() { "none".to_string() } else { t.tools.iter().map(|(k, v)| format!("{k}({v})")).collect::<Vec<_>>().join(", ") };
    let note = if t.attachments.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = t.attachments.iter().map(|a| format!("{} {} {}{}", if a.dir == "in" { "the user attached" } else { "the agent shared" }, a.kind, a.name,
            if a.url.is_empty() { String::new() } else { format!(" ({})", a.url) })).collect();
        format!("Attachments: {}\n", list.join("; "))
    };
    let last: String = t.final_text.chars().take(6000).collect();
    format!(
        "{note}Project: {project}\nAgent: {} ({})\nDuration: {} s\nTools used: {tools}\nFiles changed:\n{files}\n\nTHE USER ASKED:\n{}\n\nTHE AGENT'S LAST MESSAGE:\n{last}\n\nLOG (time from the prompt, tool calls and messages):\n{}",
        t.agent, if t.model.is_empty() { "unknown model" } else { &t.model }, (t.duration_ms as f64 / 1000.0).round(), t.prompt, t.log
    )
}

pub struct Outcome {
    pub summary: Summary,
    pub model: String,
    pub tokens: Tokens,
    pub cost: Option<f64>,
}

async fn run(cmd: &[String], input: String, cwd: &Path) -> Result<(String, String, i32)> {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new(&cmd[0]).args(&cmd[1..]).current_dir(cwd)
        // Our own child: the hooks skip it, and Claude Code does not think it runs inside itself.
        .env("SUBSTRATE_CHILD", "1").env("CLAUDECODE", "").env("CLAUDE_CODE_ENTRYPOINT", "")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped())
        .kill_on_drop(true).spawn().map_err(|e| anyhow!("{}: {e}", cmd[0]))?;
    let mut stdin = child.stdin.take().unwrap();
    tokio::spawn(async move {
        let _ = stdin.write_all(input.as_bytes()).await;
    });
    let out = tokio::time::timeout(Duration::from_secs(300), child.wait_with_output()).await.map_err(|_| anyhow!("{} took over 5 minutes", cmd[0]))??;
    Ok((String::from_utf8_lossy(&out.stdout).into(), String::from_utf8_lossy(&out.stderr).into(), out.status.code().unwrap_or(-1)))
}

fn tail(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    c[c.len().saturating_sub(n)..].iter().collect()
}

pub fn codex_default_model() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    let cfg = std::fs::read_to_string(Path::new(&home).join(".codex/config.toml")).unwrap_or_default();
    cfg.lines().find_map(|l| l.trim().strip_prefix("model").map(str::trim).and_then(|r| r.strip_prefix('=')).map(|v| v.trim().trim_matches('"').to_string())).unwrap_or_default()
}

pub async fn summarise_with(agent: &str, prompt: &str, work: &Path) -> Result<Outcome> {
    std::fs::create_dir_all(work)?;
    if agent == "codex" {
        let bin = std::env::var("SUBSTRATE_CODEX_BIN").unwrap_or_else(|_| "codex".into());
        let schema_file = work.join("schema.json");
        let out_file = work.join(format!("out-{}-{}.json", std::process::id(), rand::random::<u32>()));
        std::fs::write(&schema_file, schema().to_string())?;
        let args: Vec<String> = [bin.as_str(), "exec", "--ephemeral", "--skip-git-repo-check", "-s", "read-only", "-c", "model_reasoning_effort=\"low\"", "--output-schema"]
            .iter().map(|s| s.to_string()).chain([schema_file.to_string_lossy().into(), "-o".into(), out_file.to_string_lossy().into(), "--json".into(), "-".into()]).collect();
        let (out, err, code) = run(&args, format!("{SYSTEM}\n\n---\n\n{prompt}"), work).await?;
        let body = std::fs::read_to_string(&out_file);
        let _ = std::fs::remove_file(&out_file);
        let Ok(body) = body.map_err(|_| ()).and_then(|b| if code == 0 { Ok(b) } else { Err(()) }) else {
            bail!("codex exited {code}: {}", tail(if err.is_empty() { &out } else { &err }, 400));
        };
        let usage = out.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).rfind(|e| e["type"] == "turn.completed").map(|e| e["usage"].clone()).unwrap_or_default();
        let n = |k: &str| usage[k].as_u64().unwrap_or(0);
        let tokens = Tokens { input: n("input_tokens").saturating_sub(n("cached_input_tokens")), output: n("output_tokens"), cache_read: n("cached_input_tokens"), cache_write: n("cache_write_input_tokens") };
        return Ok(Outcome { summary: normalise(&serde_json::from_str(&body)?), model: codex_default_model(), tokens, cost: None });
    }
    let bin = std::env::var("SUBSTRATE_CLAUDE_BIN").unwrap_or_else(|_| "claude".into());
    let args: Vec<String> = [bin.as_str(), "-p", "--model", "haiku", "--setting-sources", "", "--tools", "", "--strict-mcp-config", "--no-session-persistence",
        "--output-format", "json", "--json-schema", &schema().to_string(), "--system-prompt", SYSTEM].iter().map(|s| s.to_string()).collect();
    let (out, err, code) = run(&args, prompt.to_string(), work).await?;
    let j: Value = serde_json::from_str(&out).map_err(|_| anyhow!("claude exited {code}: {}", tail(if err.is_empty() { &out } else { &err }, 400)))?;
    if j["is_error"] == true {
        let why = j["result"].as_str().or(j["subtype"].as_str()).unwrap_or("error");
        bail!("claude: {}", why.chars().take(400).collect::<String>());
    }
    let o = if j["structured_output"].is_object() { j["structured_output"].clone() } else { serde_json::from_str(j["result"].as_str().unwrap_or("{}"))? };
    let u = &j["usage"];
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    Ok(Outcome {
        summary: normalise(&o),
        model: j["modelUsage"].as_object().and_then(|m| m.keys().next().cloned()).unwrap_or_else(|| "haiku".into()),
        tokens: Tokens { input: n("input_tokens"), output: n("output_tokens"), cache_read: n("cache_read_input_tokens"), cache_write: n("cache_creation_input_tokens") },
        cost: j["total_cost_usd"].as_f64(),
    })
}

pub fn normalise(o: &Value) -> Summary {
    let s = |x: &Value| x.as_str().map(str::trim).unwrap_or("").to_string();
    let list = |k: &str| o[k].as_array().cloned().unwrap_or_default();
    let pick = |v: &Value, ok: &[&str], def: &str| { let x = s(v); if ok.contains(&x.as_str()) { x } else { def.to_string() } };
    let summary: Vec<String> = list("summary").iter().map(s).filter(|x| !x.is_empty()).take(6).collect();
    let watch: Vec<Watch> = list("watch").iter().filter(|w| !s(&w["text"]).is_empty())
        .map(|w| Watch { text: s(&w["text"]), impact: pick(&w["impact"], &["high", "medium", "low"], "medium") }).collect();
    let decisions: Vec<Decision> = list("decisions").iter().filter(|d| !s(&d["text"]).is_empty())
        .map(|d| Decision { text: s(&d["text"]), why: s(&d["why"]), alternative: s(&d["alternative"]) }).collect();
    let steps: Vec<Step> = list("steps").iter().filter(|x| !s(&x["title"]).is_empty()).take(8)
        .map(|x| Step { at: s(&x["at"]), title: s(&x["title"]), text: s(&x["text"]), tone: pick(&x["tone"], &["neutral", "good", "problem"], "neutral") }).collect();
    let mut verdict = pick(&o["verdict"], &["clean", "watch", "broken"], "clean");
    if verdict == "clean" && !watch.is_empty() {
        verdict = "watch".into();
    }
    let headline = { let h = s(&o["headline"]); if h.is_empty() { summary.first().cloned().unwrap_or_default() } else { h } };
    let summary = if summary.is_empty() && !headline.is_empty() { vec![headline.clone()] } else { summary };
    Summary { headline, summary, verdict, steps, decisions, watch }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_follows_watch_items() {
        let s = normalise(&json!({"headline": " x ", "summary": [], "verdict": "clean", "watch": [{"text": "migrate prod", "impact": "huge"}], "steps": [{"title": ""}]}));
        assert_eq!((s.headline.as_str(), s.verdict.as_str(), s.watch[0].impact.as_str()), ("x", "watch", "medium"));
        assert_eq!(s.summary, vec!["x".to_string()]);
        assert!(s.steps.is_empty());
    }
}
