//! `substrate`: pair this computer with Substrate, bind Herdr panes to
//! projects, run the relay, and let agents report back.

use std::io::Read;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};
use substrate_cli::api::Api;
use substrate_cli::config::{Config, Paths, Status};
use substrate_cli::daemon::{self, Relay};
use substrate_cli::deploy::{job_path, Job};
use substrate_cli::herdr::{self, Herdr};
use substrate_cli::launchd;
use substrate_proto::{DeployReport, Report, DEPLOY_STAGES};

#[derive(Parser)]
#[command(name = "substrate", version, about = "Give your testers a direct line to the agents that build your software.")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pair this computer with your Substrate account in the browser.
    Login {
        #[arg(long)]
        server: Option<String>,
        /// The name of this computer on the dashboard.
        #[arg(long)]
        name: Option<String>,
    },
    /// Sign this computer out and delete its credentials.
    Logout,
    /// Show the account, connection and project state.
    Status,
    /// Run or manage the background relay.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Herdr commands.
    Herdr {
        #[command(subcommand)]
        cmd: HerdrCmd,
    },
    /// List your projects.
    Projects,
    /// Create, bind or configure a project.
    Project {
        #[command(subcommand)]
        cmd: ProjectCmd,
    },
    /// List recent messages in all your projects.
    Inbox,
    /// List messages that wait for your approval.
    Approvals,
    /// Approve a message. The agent gets it when it is free.
    Approve { id: String },
    /// Reject a message. Substrate does not deliver it.
    Reject { id: String },
    /// Deliver an uncertain, failed or expired message again.
    Retry { id: String },
    /// Cancel a message before the agent gets it.
    Cancel { id: String },
    /// (For agents) Report progress on a message.
    Progress { id: String, #[arg(long)] text: String },
    /// (For agents) Tell the tester, as the developer, what the developer changed or asked for in this session.
    Note { id: String, #[arg(long)] text: String },
    /// (For agents) Ask the tester a question.
    Ask { id: String, #[arg(long)] text: String },
    /// (For agents) Reply to a message. Add --final when the work is done.
    Reply {
        id: String,
        #[arg(long)]
        text: String,
        #[arg(long = "final")]
        final_: bool,
        /// A file in the project to attach. Use it more than once for more files.
        #[arg(long)]
        attach: Vec<String>,
    },
    /// (For agents) Report CI and deployment for a message.
    Deploy {
        #[command(subcommand)]
        cmd: DeployCmd,
    },
    /// Show the relay's log.
    Logs {
        #[arg(short, long)]
        follow: bool,
    },
    /// Check Herdr, the credentials, the server and the LaunchAgent.
    Doctor,
    /// (Claude Code hook) Block push and deploy commands a tester's message does not allow.
    /// `substrate guard install` adds the hook to ~/.claude/settings.json.
    Guard {
        #[command(subcommand)]
        cmd: Option<GuardCmd>,
    },
}

#[derive(Subcommand)]
enum GuardCmd {
    /// Add the hook to Claude Code's user settings (once; it keeps a backup).
    Install,
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Install and start the LaunchAgent. It starts at login and restarts after a crash.
    Install,
    /// Start the LaunchAgent (the same as install).
    Start,
    /// Stop the relay and remove the LaunchAgent.
    Stop,
    /// Restart the running relay.
    Restart,
    /// Run the relay in the foreground. launchd uses this command.
    Run,
}

#[derive(Subcommand)]
enum HerdrCmd {
    /// List the agents that Herdr runs.
    Discover,
}

#[derive(Subcommand)]
enum ProjectCmd {
    /// Upload the project's logo again, from its folder or from --file.
    Icon { handle: String, #[arg(long)] file: Option<String> },
    /// approve: each message waits for you. auto: messages go to the agent without approval.
    Mode {
        handle: String,
        mode: String,
        /// For auto: 1h, 8h or a number of minutes. Leave it out to keep auto on.
        #[arg(long = "for")]
        for_: Option<String>,
        /// Required to turn on auto relay.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum DeployCmd {
    /// Track CI and the deploy for a fix.
    Track {
        id: String,
        #[arg(long)]
        commit: String,
        #[arg(long)]
        ci_url: Option<String>,
        #[arg(long)]
        env: Option<String>,
        /// The URL for the tester to retest on.
        #[arg(long)]
        verify_url: Option<String>,
        /// Report live only when verify-url shows the commit's short sha.
        #[arg(long)]
        expect_sha: bool,
    },
    /// Report a stage that you saw.
    Status {
        id: String,
        #[arg(long)]
        stage: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
}

fn setup() -> Result<(Paths, Config, Api)> {
    let paths = Paths::new(Paths::default_root())?;
    let cfg = Config::load(&paths);
    let api = Api::new(&paths, &cfg.server);
    Ok((paths, cfg, api))
}

fn read_text(t: &str) -> Result<String> {
    if t == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        Ok(s)
    } else {
        Ok(t.to_string())
    }
}

fn record(paths: &Paths, id: &str) -> Result<daemon::Record> {
    let p = paths.dispatch().join(format!("{}.json", substrate_proto::safe_filename(id)));
    let b = std::fs::read(&p).with_context(|| format!("no message {id} was delivered to this computer"))?;
    Ok(serde_json::from_slice(&b)?)
}

async fn report(api: &Api, r: Report) -> Result<Value> {
    api.call("POST", "/relay/report", Some(serde_json::to_value(r)?)).await
}

fn table(rows: &[Vec<String>]) {
    if rows.is_empty() {
        return;
    }
    let n = rows[0].len();
    let w: Vec<usize> = (0..n).map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0)).collect();
    for r in rows {
        let line: Vec<String> = r.iter().enumerate().map(|(i, c)| format!("{c:<w$}", w = w[i])).collect();
        println!("{}", line.join("  ").trim_end());
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("substrate: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    if let Cmd::Guard { cmd: None } = cli.cmd {
        // The hook: no network, no sign-in, a few files read. Exit 2 blocks the command
        // and Claude Code shows the agent the reason.
        let mut input = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
        let v: serde_json::Value = serde_json::from_str(&input).unwrap_or_default();
        let paths = Paths::new(Paths::default_root())?;
        if let Err(why) = substrate_cli::guard::check(&paths, &v) {
            // Exit 2 with the reason on stderr blocks in Claude Code and Codex; the JSON
            // decision on stdout says the same for hosts that read that instead.
            println!("{}", serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": why}}));
            eprintln!("{why}");
            std::process::exit(2);
        }
        return Ok(());
    }
    let (paths, mut cfg, api) = setup()?;
    match cli.cmd {
        Cmd::Login { server, name } => {
            if let Some(s) = server {
                cfg.server = s.trim_end_matches('/').to_string();
                cfg.save(&paths)?;
            }
            let api = Api::new(&paths, &cfg.server);
            let name = name.unwrap_or_else(|| {
                let o = std::process::Command::new("scutil").args(["--get", "ComputerName"]).output();
                o.ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).filter(|s| !s.is_empty()).unwrap_or_else(|| "computer".into())
            });
            let c = api.pair(&name).await?;
            println!("Paired {name} ({}). Your credentials are in the {}.", c.machine_id, api.store().backend());
            println!("Next:\n  substrate daemon install   # each agent in Herdr gets a project");
        }
        Cmd::Logout => {
            let _ = api.call("POST", "/relay/api/logout", None).await;
            api.store().clear()?;
            let _ = launchd::uninstall();
            println!("Signed out. The relay stopped.");
        }
        Cmd::Status => {
            let me = api.call("GET", "/relay/api/whoami", None).await?;
            println!("{} on {} ({})", me["account"].as_str().unwrap_or(""), me["machine"].as_str().unwrap_or(""), cfg.server);
            let st = Status::load(&paths).unwrap_or_default();
            println!("relay: {}{}", if me["online"].as_bool().unwrap_or(false) { "connected" } else { "not connected" },
                if launchd::loaded() { " · LaunchAgent loaded" } else { " · LaunchAgent not installed (substrate daemon install)" });
            if !st.error.is_empty() {
                println!("last error: {}", st.error);
            }
            let ps = api.call("GET", "/relay/api/projects", None).await?;
            let mut rows = vec![vec!["PROJECT".into(), "PANE".into(), "AGENT".into(), "STATE".into(), "RELAY".into(), "PENDING".into()]];
            for p in ps["projects"].as_array().into_iter().flatten() {
                rows.push(vec![p["handle"].as_str().unwrap_or("").into(), p["pane_id"].as_str().unwrap_or("").into(), p["agent"].as_str().unwrap_or("").into(),
                    p["state"].as_str().unwrap_or("").into(), p["mode"].as_str().unwrap_or("").into(), p["pending"].to_string()]);
            }
            table(&rows);
        }
        Cmd::Daemon { cmd } => match cmd {
            DaemonCmd::Install | DaemonCmd::Start => {
                api.store().load()?.context("not signed in; run `substrate login` first")?;
                let exe = std::env::current_exe()?.to_string_lossy().to_string();
                launchd::install(&exe, &paths.logs().to_string_lossy(), &cfg.server)?;
                println!("The relay runs in the background and starts at login. Run `substrate status` to check it.");
                if substrate_cli::guard::install(&exe)? {
                    println!("The deploy guard is on: Claude Code blocks deploys a tester's message does not allow.");
                }
                if substrate_cli::guard::install_codex(&exe)? {
                    println!("The deploy guard is added for Codex. Approve it once in Codex under /hooks.");
                }
            }
            DaemonCmd::Stop => {
                launchd::uninstall()?;
                println!("Stopped. Messages stay in the queue until the relay runs again.");
            }
            DaemonCmd::Restart => launchd::restart()?,
            DaemonCmd::Run => {
                let api = Arc::new(api);
                let relay = Relay::new(paths.clone(), Arc::new(herdr::Cli::new()), api.clone());
                let stop = async {
                    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                    tokio::select! { _ = term.recv() => {}, _ = tokio::signal::ctrl_c() => {} }
                };
                tokio::select! {
                    r = daemon::run(relay, api) => r?,
                    _ = stop => {
                        let mut st = Status::load(&paths).unwrap_or_default();
                        st.connected = false;
                        st.save(&paths);
                    }
                }
            }
        },
        Cmd::Herdr { cmd: HerdrCmd::Discover } => {
            let panes = herdr::Cli::new().list().context("no answer from Herdr. Is herdr running?")?;
            let mut rows = vec![vec!["PANE".into(), "AGENT".into(), "STATUS".into(), "TITLE".into(), "DIRECTORY".into()]];
            for p in panes {
                rows.push(vec![p.pane_id, p.agent, p.status, p.title, p.cwd]);
            }
            table(&rows);
        }
        Cmd::Projects => {
            let ps = api.call("GET", "/relay/api/projects", None).await?;
            let mut rows = vec![vec!["PROJECT".into(), "NAME".into(), "PANE".into(), "STATE".into(), "RELAY".into(), "PENDING".into()]];
            for p in ps["projects"].as_array().into_iter().flatten() {
                rows.push(vec![p["handle"].as_str().unwrap_or("").into(), p["name"].as_str().unwrap_or("").into(), p["pane_id"].as_str().unwrap_or("").into(),
                    p["state"].as_str().unwrap_or("").into(), p["mode"].as_str().unwrap_or("").into(), p["pending"].to_string()]);
            }
            table(&rows);
        }
        Cmd::Project { cmd } => match cmd {
            ProjectCmd::Icon { handle, file } => {
                let cwd = api.call("GET", "/relay/api/projects", None).await?["projects"].as_array().into_iter().flatten()
                    .find(|p| p["handle"] == handle.as_str()).and_then(|p| p["cwd"].as_str().map(String::from)).unwrap_or_default();
                if !upload_icon(&api, &handle, &cwd, file.as_deref()).await {
                    bail!("no logo in the project folder. Add --file <png>.");
                }
            }
            ProjectCmd::Mode { handle, mode, for_, yes } => {
                let minutes = match for_.as_deref() {
                    None => None,
                    Some("1h") => Some(60),
                    Some("8h") => Some(480),
                    Some(m) => Some(m.parse::<i64>().context("--for is 1h, 8h or minutes")?),
                };
                if mode == "auto" && !yes {
                    bail!("in auto relay, testers' messages go to your agent without your approval. Add --yes to confirm.");
                }
                api.call("POST", &format!("/relay/api/projects/{handle}/mode"), Some(json!({"mode": mode, "minutes": minutes}))).await?;
                println!("{handle}: {}", if mode == "auto" { "auto relay on. Flagged messages still wait for you. Your agent's permissions stay the same." } else { "approval required" });
            }
        },
        Cmd::Inbox | Cmd::Approvals => {
            let (path, key) = if matches!(cli_cmd_name(), "approvals") { ("/relay/api/approvals", "approvals") } else { ("/relay/api/inbox", "messages") };
            let v = api.call("GET", path, None).await?;
            let mut rows = vec![vec!["ID".into(), "FROM".into(), "STAGE".into(), "TEXT".into()]];
            for m in v[key].as_array().into_iter().flatten() {
                let text: String = m["text"].as_str().unwrap_or("").replace('\n', " ").chars().take(60).collect();
                let stage = format!("{}{}", m["stage"].as_str().unwrap_or(""), if m["risk"].as_str().unwrap_or("").is_empty() { String::new() } else { format!(" ({})", m["risk"].as_str().unwrap()) });
                rows.push(vec![m["id"].as_str().unwrap_or("").into(), format!("{} via {}", m["from"].as_str().unwrap_or(""), m["client"].as_str().unwrap_or("")), stage, text]);
            }
            if rows.len() == 1 {
                println!("No messages.");
            } else {
                table(&rows);
            }
        }
        Cmd::Approve { id } => decide(&api, &id, "approve").await?,
        Cmd::Reject { id } => decide(&api, &id, "reject").await?,
        Cmd::Retry { id } => decide(&api, &id, "retry").await?,
        Cmd::Cancel { id } => decide(&api, &id, "cancel").await?,
        Cmd::Progress { id, text } => {
            let r = record(&paths, &id)?;
            report(&api, Report { message_id: id, reply_token: r.reply_token, kind: "progress".into(), text: read_text(&text)?, ..Default::default() }).await?;
            println!("Progress sent.");
        }
        Cmd::Note { id, text } => {
            let r = record(&paths, &id)?;
            report(&api, Report { message_id: id, reply_token: r.reply_token, kind: "note".into(), text: read_text(&text)?, ..Default::default() }).await?;
            println!("Note sent to the tester as the developer.");
        }
        Cmd::Ask { id, text } => {
            let r = record(&paths, &id)?;
            report(&api, Report { message_id: id, reply_token: r.reply_token, kind: "ask".into(), text: read_text(&text)?, ..Default::default() }).await?;
            println!("Question sent to the tester. Their answer arrives as a new message.");
        }
        Cmd::Reply { id, text, final_, attach } => {
            let r = record(&paths, &id)?;
            let mut ids = vec![];
            for a in attach {
                ids.push(upload_inside(&api, &r, &a).await?);
            }
            let v = report(&api, Report { message_id: id.clone(), reply_token: r.reply_token.clone(), kind: if final_ { "final".into() } else { "reply".into() },
                text: read_text(&text)?, deploy: None, attachments: ids }).await?;
            if final_ {
                let _ = std::fs::write(paths.dispatch().join(format!("{}.final", substrate_proto::safe_filename(&id))), b"");
            }
            if v["held"].as_bool().unwrap_or(false) {
                println!("Held for the developer. Possible secret: {}.", v["secret_hits"]);
            } else {
                println!("Reply sent.");
            }
        }
        Cmd::Deploy { cmd } => match cmd {
            DeployCmd::Track { id, commit, ci_url, env, verify_url, expect_sha } => {
                let r = record(&paths, &id)?;
                let job = Job { message_id: id.clone(), reply_token: r.reply_token, commit, ci_url: ci_url.unwrap_or_default(), env: env.unwrap_or_default(),
                    verify_url: verify_url.unwrap_or_default(), expect_sha, cwd: r.cwd, changed_at: chrono::Utc::now().timestamp(), ..Default::default() };
                substrate_cli::config::write_private(&job_path(&paths, &id), &serde_json::to_vec_pretty(&job)?)?;
                println!("Tracking the pipeline for {id}. The tester gets each stage, and \"ready to retest\" when the fix is live.");
            }
            DeployCmd::Status { id, stage, url, note } => {
                if !DEPLOY_STAGES.contains(&stage.as_str()) {
                    bail!("stage is one of: {}", DEPLOY_STAGES.join(", "));
                }
                let r = record(&paths, &id)?;
                report(&api, Report { message_id: id, reply_token: r.reply_token, kind: "deploy".into(), text: String::new(), attachments: vec![],
                    deploy: Some(DeployReport { stage, verify_url: url.unwrap_or_default(), note: note.unwrap_or_default(), ..Default::default() }) }).await?;
                println!("Deploy stage sent.");
            }
        },
        Cmd::Logs { follow } => {
            let p = paths.logs().join("daemon.log");
            let mut args = vec!["-n", "200"];
            if follow {
                args.push("-f");
            }
            std::process::Command::new("tail").args(&args).arg(&p).status()?;
        }
        Cmd::Doctor => doctor(&paths, &cfg, &api).await,
        Cmd::Guard { cmd: Some(GuardCmd::Install) } => {
            let exe = std::env::current_exe()?.to_string_lossy().to_string();
            if substrate_cli::guard::install(&exe)? { println!("The deploy guard is on for Claude Code (~/.claude/settings.json; the old file is settings.json.before-substrate)."); }
            else { println!("The deploy guard is already on for Claude Code."); }
            if substrate_cli::guard::install_codex(&exe)? { println!("It is added for Codex too (~/.codex/hooks.json). Open Codex and approve the new hook under /hooks once: Codex runs a hook only after you review it."); }
            else if substrate_cli::guard::installed_codex() { println!("It is already added for Codex."); }
        }
        Cmd::Guard { cmd: None } => unreachable!("handled before sign-in"),
    }
    Ok(())
}

/// Find (or read) the project's logo and upload it. Never fails the command
/// that called it: a project without a logo gets a lettered tile.
async fn upload_icon(api: &Api, handle: &str, cwd: &str, file: Option<&str>) -> bool {
    let bytes = match file {
        Some(f) => std::fs::read(f).ok(),
        None => substrate_cli::icon::detect(cwd),
    };
    let Some(bytes) = bytes else { return false };
    match api.icon(handle, bytes).await {
        Ok(()) => { println!("Project icon set from its logo."); true }
        Err(e) => { eprintln!("substrate: icon not set: {e}"); false }
    }
}

fn cli_cmd_name() -> &'static str {
    if std::env::args().nth(1).as_deref() == Some("approvals") { "approvals" } else { "inbox" }
}

async fn decide(api: &Api, id: &str, action: &str) -> Result<()> {
    let v = api.call("POST", &format!("/relay/api/messages/{id}/{action}"), None).await?;
    println!("{id}: {}", v["stage"].as_str().unwrap_or("").replace('_', " "));
    Ok(())
}

/// Only files inside the bound project, after following symlinks.
async fn upload_inside(api: &Api, r: &daemon::Record, path: &str) -> Result<String> {
    if r.cwd.is_empty() {
        bail!("this project has no directory, so it cannot send attachments");
    }
    let root = std::fs::canonicalize(&r.cwd)?;
    let file = std::fs::canonicalize(path).with_context(|| format!("no such file: {path}"))?;
    if !file.starts_with(&root) || !file.is_file() {
        bail!("{path} is outside the project ({})", root.display());
    }
    let bytes = std::fs::read(&file)?;
    if bytes.len() > 10 * 1024 * 1024 {
        bail!("{path} is over 10 MB");
    }
    let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
    api.upload(&r.message_id, &r.reply_token, &name, bytes).await
}

async fn doctor(paths: &Paths, cfg: &Config, api: &Api) {
    let ok = |b: bool| if b { "ok  " } else { "FAIL" };
    let h = herdr::Cli::new().list();
    println!("{} herdr: {}", ok(h.is_ok()), match &h { Ok(p) => format!("{} agents", p.len()), Err(e) => e.to_string() });
    let creds = api.store().load().ok().flatten();
    println!("{} credentials: {}", ok(creds.is_some()), if creds.is_some() { api.store().backend().to_string() } else { "not signed in (substrate login)".into() });
    let reach = api.http.get(format!("{}/healthz", cfg.server)).send().await.map(|r| r.status().is_success()).unwrap_or(false);
    println!("{} server: {}", ok(reach), cfg.server);
    println!("{} deploy guard: {}", ok(substrate_cli::guard::installed()), if substrate_cli::guard::installed() { "on in Claude Code" } else { "off (substrate guard install)" });
    println!("{} deploy guard for Codex: {}", ok(substrate_cli::guard::installed_codex()), if substrate_cli::guard::installed_codex() { "added (approve it once in Codex under /hooks)" } else { "not added (substrate guard install)" });
    if creds.is_some() {
        let me = api.call("GET", "/relay/api/whoami", None).await;
        println!("{} account: {}", ok(me.is_ok()), match &me { Ok(v) => format!("{} on {}", v["account"].as_str().unwrap_or(""), v["machine"].as_str().unwrap_or("")), Err(e) => e.to_string() });
        if let Ok(v) = &me {
            println!("{} relay connection: {}", ok(v["online"].as_bool().unwrap_or(false)), if v["online"].as_bool().unwrap_or(false) { "connected" } else { "not connected" });
        }
    }
    println!("{} LaunchAgent: {}", ok(launchd::loaded()), if launchd::loaded() { "loaded" } else { "not installed (substrate daemon install)" });
    let gh = std::process::Command::new("gh").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    println!("{} gh (for CI watching): {}", ok(gh), if gh { "found" } else { "not found. Deploy track uses the stages your agent reports." });
    if let Some(st) = Status::load(paths) {
        if !st.error.is_empty() {
            println!("     last relay error: {}", st.error);
        }
    }
}
