//! The Herdr adapter. Herdr owns the panes and knows what each agent is doing
//! (idle, working, blocked at a permission prompt, done); `herdr agent prompt`
//! bracketed-pastes text plus Enter as one submission and refuses an agent
//! that is blocked. Substrate never starts, stops or answers an agent.

use std::process::Command;

use anyhow::{bail, Result};
use serde_json::Value;
use substrate_proto::Pane;

pub trait Herdr: Send + Sync {
    fn list(&self) -> Result<Vec<Pane>>;
    /// Submit text to the agent in a pane. `Err(Blocked)` when the agent sits
    /// at an approval or question dialog: nothing was typed.
    fn prompt(&self, pane: &str, text: &str) -> std::result::Result<(), PromptError>;
    fn read(&self, pane: &str, lines: u32) -> Result<String>;
}

#[derive(Debug)]
pub enum PromptError {
    Blocked,
    Gone,
    Other(String),
}

pub struct Cli {
    pub bin: String,
}

impl Cli {
    pub fn new() -> Cli {
        Cli { bin: std::env::var("HERDR_BIN").unwrap_or_else(|_| "herdr".into()) }
    }
    fn run(&self, args: &[&str]) -> Result<Value> {
        let out = Command::new(&self.bin).args(args).output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(serde_json::from_slice(&out.stdout)?)
    }
}

impl Default for Cli {
    fn default() -> Self {
        Self::new()
    }
}

pub fn pane_from(a: &Value) -> Pane {
    Pane {
        pane_id: a["pane_id"].as_str().unwrap_or_default().into(),
        agent: a["agent"].as_str().unwrap_or_default().into(),
        agent_session: a["agent_session"]["value"].as_str().unwrap_or_default().into(),
        status: a["agent_status"].as_str().unwrap_or("unknown").into(),
        cwd: a["cwd"].as_str().unwrap_or_default().into(),
        title: a["terminal_title_stripped"].as_str().unwrap_or_default().into(),
    }
}

impl Herdr for Cli {
    fn list(&self) -> Result<Vec<Pane>> {
        let v = self.run(&["agent", "list"])?;
        Ok(v["result"]["agents"].as_array().map(|l| l.iter().map(pane_from).collect()).unwrap_or_default())
    }

    fn prompt(&self, pane: &str, text: &str) -> std::result::Result<(), PromptError> {
        let out = Command::new(&self.bin).args(["agent", "prompt", pane, text]).output().map_err(|e| PromptError::Other(e.to_string()))?;
        if out.status.success() {
            return Ok(());
        }
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        if err.contains("agent_blocked") {
            Err(PromptError::Blocked)
        } else if err.contains("not_found") || err.contains("no agent") || err.contains("unknown target") {
            Err(PromptError::Gone)
        } else {
            Err(PromptError::Other(err.trim().chars().take(300).collect()))
        }
    }

    fn read(&self, pane: &str, lines: u32) -> Result<String> {
        let out = Command::new(&self.bin).args(["agent", "read", pane, "--source", "recent-unwrapped", "--lines", &lines.to_string()]).output()?;
        if !out.status.success() {
            bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
        }
        // `agent read` prints JSON or plain text depending on version: take either.
        let s = String::from_utf8_lossy(&out.stdout).to_string();
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            if let Some(t) = v["result"]["text"].as_str().or(v["result"]["read"]["text"].as_str()) {
                return Ok(t.to_string());
            }
        }
        Ok(s)
    }
}
