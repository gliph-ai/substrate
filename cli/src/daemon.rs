//! The relay daemon: one outbound WebSocket to the cloud, a durable dispatch
//! record per message, one message in flight per project, and Herdr as the
//! only way anything reaches an agent (docs/architecture.md, "Delivery rules").

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use substrate_proto::{self as proto, AckStage, AttachmentRef, Binding, CloudFrame, DaemonFrame, Pane};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMsg;

use crate::config::{log, write_private, Paths, ProjectStatus, Status};
use crate::herdr::{Herdr, PromptError};

/// How the daemon downloads an attachment (the API in production, a stub in tests).
pub trait Fetch: Send + Sync {
    fn fetch<'a>(&'a self, url: &'a str) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>>;
}

impl Fetch for crate::api::Api {
    fn fetch<'a>(&'a self, url: &'a str) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        Box::pin(self.download(url))
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Record {
    pub message_id: String,
    pub delivery_id: String,
    pub project_id: String,
    pub envelope: String,
    pub reply_token: String,
    pub attachments: Vec<AttachmentRef>,
    /// received | submitting | submitted | working | done | uncertain | failed | cancelled
    pub state: String,
    #[serde(default)]
    pub pane_id: String,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub received_at: i64,
    /// What the agent may deploy while on this message (production, test or none);
    /// `substrate guard` enforces it. Empty, from an older server, means none.
    #[serde(default)]
    pub deploy: String,
    /// A review-only delivery: the real one of the same message may follow.
    #[serde(default)]
    pub review: bool,
    /// A follow-up with the fit check: if the agent holds it, the approved one may follow.
    #[serde(default)]
    pub check: bool,
    /// The agent's session when it was handed this: the guard matches hooks to it.
    #[serde(default)]
    pub agent_session: String,
    #[serde(default)]
    pub submitted_at: i64,
    #[serde(default)]
    pub saw_working: bool,
    #[serde(default)]
    pub reported_uncertain: bool,
}

#[derive(Default)]
struct Inner {
    bindings: HashMap<String, Binding>,
    panes: Vec<Pane>,
    panes_sent: Option<Vec<Pane>>,
    state_sent: HashMap<String, String>,
    queue: HashMap<String, VecDeque<String>>,
    last_poll: Option<Instant>,
}

pub struct Relay {
    pub paths: Paths,
    herdr: Arc<dyn Herdr>,
    fetch: Arc<dyn Fetch>,
    inner: Mutex<Inner>,
    out: Mutex<Option<mpsc::UnboundedSender<DaemonFrame>>>,
}

fn now_s() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Relay {
    pub fn new(paths: Paths, herdr: Arc<dyn Herdr>, fetch: Arc<dyn Fetch>) -> Arc<Relay> {
        let r = Arc::new(Relay { paths, herdr, fetch, inner: Mutex::new(Inner::default()), out: Mutex::new(None) });
        r.load_records();
        r
    }

    fn record_path(&self, mid: &str) -> PathBuf {
        self.paths.dispatch().join(format!("{}.json", proto::safe_filename(mid)))
    }

    pub fn record(&self, mid: &str) -> Option<Record> {
        std::fs::read(self.record_path(mid)).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    fn save(&self, r: &Record) {
        if let Err(e) = write_private(&self.record_path(&r.message_id), &serde_json::to_vec_pretty(r).unwrap()) {
            log(&self.paths, "error", "could not write dispatch record", json!({"message_id": r.message_id, "error": e.to_string()}));
        }
    }

    /// The agent marked this message finished (`substrate reply --final`).
    fn finalized(&self, mid: &str) -> bool {
        self.paths.dispatch().join(format!("{}.final", proto::safe_filename(mid))).exists()
    }

    /// On start: anything caught mid-submit is uncertain (it may or may not
    /// have reached the agent) and is never resubmitted; the rest resumes.
    fn load_records(&self) {
        let mut recs: Vec<Record> = std::fs::read_dir(self.paths.dispatch()).into_iter().flatten().flatten()
            .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
            .filter_map(|e| std::fs::read(e.path()).ok()).filter_map(|b| serde_json::from_slice::<Record>(&b).ok()).collect();
        recs.sort_by_key(|r| r.received_at);
        let mut inner = self.inner.lock().unwrap();
        for mut r in recs {
            match r.state.as_str() {
                "submitting" => {
                    r.state = "uncertain".into();
                    self.save(&r);
                    log(&self.paths, "warn", "message was mid-submit at restart; marked uncertain, not resubmitted", json!({"message_id": r.message_id}));
                }
                "received" | "submitted" | "working" => inner.queue.entry(r.project_id.clone()).or_default().push_back(r.message_id.clone()),
                _ => {}
            }
        }
    }

    /// Where outgoing frames go (the socket's writer; a channel in tests).
    pub fn set_outbox(&self, tx: Option<mpsc::UnboundedSender<DaemonFrame>>) {
        *self.out.lock().unwrap() = tx;
    }

    /// Make the next tick ask Herdr again instead of using the last answer.
    pub fn poll_now(&self) {
        self.inner.lock().unwrap().last_poll = None;
    }

    /// As after the server's welcome: re-report every open record.
    pub fn connected(&self) {
        self.on_connected();
    }

    fn send(&self, f: DaemonFrame) {
        if let Some(tx) = self.out.lock().unwrap().as_ref() {
            let _ = tx.send(f);
        }
    }

    fn ack(&self, r: &Record, stage: AckStage, detail: &str) {
        self.send(DaemonFrame::Ack { delivery_id: r.delivery_id.clone(), stage, detail: detail.to_string() });
    }

    /// After (re)connecting: tell the cloud where every open record stands.
    fn on_connected(&self) {
        for mut r in self.all_records() {
            match r.state.as_str() {
                "uncertain" if !r.reported_uncertain => {
                    self.ack(&r, AckStage::Uncertain, "the relay restarted while submitting");
                    r.reported_uncertain = true;
                    self.save(&r);
                }
                "received" => self.ack(&r, AckStage::Accepted, ""),
                "submitted" => self.ack(&r, AckStage::Submitted, ""),
                "working" => self.ack(&r, AckStage::Working, ""),
                _ => {}
            }
        }
        let mut inner = self.inner.lock().unwrap();
        inner.panes_sent = None;
        inner.state_sent.clear();
    }

    fn all_records(&self) -> Vec<Record> {
        std::fs::read_dir(self.paths.dispatch()).into_iter().flatten().flatten()
            .filter(|e| e.path().extension().map(|x| x == "json").unwrap_or(false))
            .filter_map(|e| std::fs::read(e.path()).ok()).filter_map(|b| serde_json::from_slice::<Record>(&b).ok()).collect()
    }

    pub fn on_frame(&self, f: CloudFrame) {
        match f {
            CloudFrame::Welcome { projects, .. } | CloudFrame::Bindings { projects } => {
                let mut inner = self.inner.lock().unwrap();
                inner.bindings = projects.into_iter().map(|b| (b.project_id.clone(), b)).collect();
                inner.state_sent.clear();
            }
            CloudFrame::Deliver { delivery_id, message_id, project_id, envelope, reply_token, attachments, deploy, review, check } => {
                // A review goes before the real delivery of the same message, and a
                // review may be asked again: either one is new text for the agent. So
                // is the approved delivery of a follow-up the agent held.
                let fresh = |e: &Record| (e.review || e.check || review) && e.delivery_id != delivery_id;
                if let Some(mut existing) = self.record(&message_id).filter(|e| !fresh(e)) {
                    // A repeat (the cloud re-sent after a reconnect). Never a
                    // second submission: adopt the new delivery id, re-ack.
                    // A review re-sent after a reconnect keeps its delivery id with a new token.
                    if existing.delivery_id != delivery_id || existing.reply_token != reply_token {
                        existing.delivery_id = delivery_id;
                        existing.reply_token = reply_token;
                        self.save(&existing);
                    }
                    let stage = match existing.state.as_str() {
                        "received" => AckStage::Accepted,
                        "submitted" => AckStage::Submitted,
                        "working" => AckStage::Working,
                        "uncertain" | "submitting" => AckStage::Uncertain,
                        "done" => AckStage::AgentIdle,
                        _ => AckStage::Failed,
                    };
                    self.ack(&existing, stage, "repeat delivery");
                    return;
                }
                let b = self.inner.lock().unwrap().bindings.get(&project_id).cloned();
                let r = Record {
                    message_id: message_id.clone(), delivery_id, project_id: project_id.clone(), envelope, reply_token, attachments,
                    state: "received".into(), pane_id: b.as_ref().map(|b| b.pane_id.clone()).unwrap_or_default(),
                    cwd: b.as_ref().map(|b| b.cwd.clone()).unwrap_or_default(), received_at: now_s(), submitted_at: 0, saw_working: false, reported_uncertain: false,
                    agent_session: b.as_ref().map(|b| b.agent_session.clone()).unwrap_or_default(), deploy, review, check,
                };
                self.save(&r);
                let mut inner = self.inner.lock().unwrap();
                let q = inner.queue.entry(project_id).or_default();
                q.retain(|m| m != &message_id);
                q.push_back(message_id.clone());
                drop(inner);
                self.ack(&r, AckStage::Accepted, "");
                log(&self.paths, "info", "message received", json!({"message_id": message_id}));
            }
            CloudFrame::Cancel { delivery_id } => {
                if let Some(mut r) = self.all_records().into_iter().find(|r| r.delivery_id == delivery_id) {
                    if r.state == "received" {
                        r.state = "cancelled".into();
                        self.save(&r);
                        if let Some(q) = self.inner.lock().unwrap().queue.get_mut(&r.project_id) {
                            q.retain(|m| m != &r.message_id);
                        }
                    }
                }
            }
            CloudFrame::Error { message } => log(&self.paths, "warn", "server said", json!({"message": message})),
        }
    }

    fn pane_state(b: &Binding, panes: &[Pane]) -> (String, Option<Pane>) {
        match panes.iter().find(|p| p.pane_id == b.pane_id) {
            Some(p) if b.agent_session.is_empty() || p.agent_session == b.agent_session => {
                let s = match p.status.as_str() { "idle" | "done" | "working" | "blocked" => p.status.clone(), _ => "unknown".into() };
                (s, Some(p.clone()))
            }
            _ => ("needs_rebind".into(), None),
        }
    }

    /// One pass: refresh the pane inventory, report state changes, and move
    /// each project's head message along.
    pub async fn tick(&self) {
        let poll_due = self.inner.lock().unwrap().last_poll.map(|t| t.elapsed() >= Duration::from_secs(2)).unwrap_or(true);
        if poll_due {
            match self.herdr.list() {
                Ok(panes) => {
                    let mut inner = self.inner.lock().unwrap();
                    inner.panes = panes.clone();
                    inner.last_poll = Some(Instant::now());
                    if inner.panes_sent.as_ref() != Some(&panes) {
                        inner.panes_sent = Some(panes.clone());
                        drop(inner);
                        self.send(DaemonFrame::Panes { panes });
                    }
                }
                Err(e) => log(&self.paths, "warn", "herdr list failed", json!({"error": e.to_string()})),
            }
        }
        let (bindings, panes) = {
            let inner = self.inner.lock().unwrap();
            (inner.bindings.values().cloned().collect::<Vec<_>>(), inner.panes.clone())
        };
        for b in &bindings {
            let (state, _) = Self::pane_state(b, &panes);
            let changed = self.inner.lock().unwrap().state_sent.get(&b.project_id) != Some(&state);
            if changed {
                self.inner.lock().unwrap().state_sent.insert(b.project_id.clone(), state.clone());
                self.send(DaemonFrame::Status { project_id: b.project_id.clone(), state });
            }
        }
        for b in &bindings {
            self.advance(b, &panes).await;
        }
        self.write_status(&bindings, &panes);
    }

    async fn advance(&self, b: &Binding, panes: &[Pane]) {
        let head = self.inner.lock().unwrap().queue.get(&b.project_id).and_then(|q| q.front().cloned());
        let Some(mid) = head else { return };
        let Some(mut r) = self.record(&mid) else { self.pop(&b.project_id, &mid); return };
        let (state, pane) = Self::pane_state(b, panes);
        match r.state.as_str() {
            "received" => {
                // Only an agent Herdr says is ready for input. Never a busy
                // agent, never a permission prompt, never an unknown pane.
                if !matches!(state.as_str(), "idle" | "done") {
                    return;
                }
                let text = match self.prepare(b, &r).await {
                    Ok(t) => t,
                    Err(e) => {
                        r.state = "failed".into();
                        self.save(&r);
                        self.ack(&r, AckStage::Failed, &e.to_string());
                        self.pop(&b.project_id, &mid);
                        return;
                    }
                };
                r.state = "submitting".into();
                r.pane_id = b.pane_id.clone();
                r.cwd = b.cwd.clone();
                r.agent_session = b.agent_session.clone();
                self.save(&r);
                match self.herdr.prompt(&b.pane_id, &text) {
                    Ok(()) => {
                        r.state = "submitted".into();
                        r.submitted_at = now_s();
                        self.save(&r);
                        self.ack(&r, AckStage::Submitted, "");
                        log(&self.paths, "info", "submitted to agent", json!({"message_id": mid, "pane": b.pane_id}));
                    }
                    Err(PromptError::Blocked) => {
                        r.state = "received".into();
                        self.save(&r);
                        self.send(DaemonFrame::Status { project_id: b.project_id.clone(), state: "blocked".into() });
                    }
                    Err(PromptError::NotReady) => {
                        r.state = "received".into();
                        self.save(&r);
                        self.send(DaemonFrame::Status { project_id: b.project_id.clone(), state: "unknown".into() });
                    }
                    Err(PromptError::Gone) => {
                        r.state = "received".into();
                        self.save(&r);
                        self.send(DaemonFrame::Status { project_id: b.project_id.clone(), state: "needs_rebind".into() });
                    }
                    Err(PromptError::Other(e)) => {
                        // Herdr answered with an error we can't classify: we
                        // cannot know whether text reached the agent.
                        r.state = "uncertain".into();
                        r.reported_uncertain = true;
                        self.save(&r);
                        self.ack(&r, AckStage::Uncertain, &e);
                        self.pop(&b.project_id, &mid);
                    }
                }
            }
            "submitted" | "working" => {
                let Some(_) = pane else {
                    r.state = "failed".into();
                    self.save(&r);
                    self.ack(&r, AckStage::Failed, "the agent's pane closed or changed session while working");
                    self.pop(&b.project_id, &mid);
                    return;
                };
                match state.as_str() {
                    "working" | "blocked" => {
                        if !r.saw_working {
                            r.saw_working = true;
                            r.state = "working".into();
                            self.save(&r);
                            self.ack(&r, AckStage::Working, "");
                        }
                    }
                    "idle" | "done" => {
                        let quiet_long = now_s() - r.submitted_at > 20;
                        if !(r.saw_working || quiet_long) {
                            return;
                        }
                        r.state = "done".into();
                        self.save(&r);
                        if !self.finalized(&mid) {
                            self.ack(&r, AckStage::AgentIdle, "");
                            if let Ok(t) = self.herdr.read(&b.pane_id, 120) {
                                let mut t = t;
                                if t.len() > 8192 {
                                    let cut = t.len() - 8192;
                                    t = t[t.char_indices().map(|(i, _)| i).find(|&i| i >= cut).unwrap_or(0)..].to_string();
                                }
                                self.send(DaemonFrame::Excerpt { message_id: mid.clone(), text: t });
                            }
                        }
                        self.pop(&b.project_id, &mid);
                    }
                    _ => {}
                }
            }
            _ => self.pop(&b.project_id, &mid),
        }
    }

    fn pop(&self, project: &str, mid: &str) {
        if let Some(q) = self.inner.lock().unwrap().queue.get_mut(project) {
            q.retain(|m| m != mid);
        }
    }

    /// Download attachments into the project's workspace and fill in the
    /// envelope's folder. Remote names and paths are never trusted.
    async fn prepare(&self, b: &Binding, r: &Record) -> Result<String> {
        if !proto::envelope_is_safe(&r.envelope) {
            bail!("refused: the envelope contains control characters");
        }
        let dir = self.paths.workspace().join(proto::safe_filename(&b.handle)).join(proto::safe_filename(&r.message_id));
        if !r.attachments.is_empty() {
            std::fs::create_dir_all(&dir)?;
            for a in &r.attachments {
                let name = proto::safe_filename(&a.name);
                let path = dir.join(&name);
                if path.parent() != Some(dir.as_path()) {
                    bail!("refused attachment name");
                }
                let bytes = self.fetch.fetch(&a.url).await?;
                if bytes.len() as u64 != a.size || hex::encode(Sha256::digest(&bytes)) != a.sha256 {
                    bail!("attachment {name} did not match its checksum");
                }
                write_new(&path, &bytes)?;
            }
        }
        Ok(r.envelope.replace(proto::ATTACH_DIR, &dir.to_string_lossy()))
    }

    fn write_status(&self, bindings: &[Binding], panes: &[Pane]) {
        let mut st = Status::load(&self.paths).unwrap_or_default();
        st.pid = std::process::id();
        st.version = env!("CARGO_PKG_VERSION").into();
        let inner = self.inner.lock().unwrap();
        st.projects = bindings.iter().map(|b| ProjectStatus {
            handle: b.handle.clone(), pane_id: b.pane_id.clone(), state: Self::pane_state(b, panes).0,
            queued: inner.queue.get(&b.project_id).map(|q| q.len()).unwrap_or(0),
        }).collect();
        drop(inner);
        st.save(&self.paths);
    }

    /// One connection's life. Returns when the socket drops or goes silent.
    pub async fn session(self: &Arc<Self>, ws_url: &str, token: &str) -> Result<()> {
        let mut req = ws_url.into_client_request()?;
        req.headers_mut().insert("authorization", format!("Bearer {token}").parse()?);
        let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| anyhow!("connect: {e}"))?;
        let (mut sink, mut stream) = ws.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<DaemonFrame>();
        *self.out.lock().unwrap() = Some(tx);
        let panes = self.herdr.list().unwrap_or_default();
        self.inner.lock().unwrap().panes = panes.clone();
        sink.send(WsMsg::Text(serde_json::to_string(&DaemonFrame::Hello { version: env!("CARGO_PKG_VERSION").into(), protocol: proto::PROTOCOL, panes })?.into())).await?;
        let mut st = Status::load(&self.paths).unwrap_or_default();
        st.connected = true;
        st.since = chrono::Utc::now().to_rfc3339();
        st.error.clear();
        st.save(&self.paths);
        log(&self.paths, "info", "connected", json!({"url": ws_url}));

        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut last_heard = Instant::now();
        let mut welcomed = false;
        let result: Result<()> = loop {
            tokio::select! {
                f = rx.recv() => {
                    let Some(f) = f else { break Ok(()) };
                    if let Err(e) = sink.send(WsMsg::Text(serde_json::to_string(&f)?.into())).await { break Err(e.into()); }
                }
                m = stream.next() => {
                    let Some(m) = m else { break Ok(()) };
                    let m = match m { Ok(m) => m, Err(e) => break Err(e.into()) };
                    last_heard = Instant::now();
                    match m {
                        WsMsg::Text(t) => match serde_json::from_str::<CloudFrame>(&t) {
                            Ok(f) => {
                                let welcome = matches!(f, CloudFrame::Welcome { .. });
                                self.on_frame(f);
                                if welcome && !welcomed { welcomed = true; self.on_connected(); }
                            }
                            Err(e) => log(&self.paths, "warn", "unreadable frame", json!({"error": e.to_string()})),
                        },
                        WsMsg::Close(_) => break Ok(()),
                        _ => {}
                    }
                }
                _ = tick.tick() => {
                    // Server pings every 25 s. Silence past 70 s (a dead NAT
                    // mapping, or the Mac waking from sleep) means reconnect.
                    if last_heard.elapsed() > Duration::from_secs(70) { break Err(anyhow!("connection went silent")); }
                    if welcomed { self.tick().await; }
                    let mut st = Status::load(&self.paths).unwrap_or_default();
                    st.last_heartbeat = chrono::Utc::now().to_rfc3339();
                    st.save(&self.paths);
                }
            }
        };
        *self.out.lock().unwrap() = None;
        let mut st = Status::load(&self.paths).unwrap_or_default();
        st.connected = false;
        if let Err(e) = &result {
            st.error = e.to_string();
        }
        st.save(&self.paths);
        result
    }
}

fn write_new(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    // create_new refuses to follow an existing symlink at that path.
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(bytes)?;
    Ok(())
}

/// The daemon forever: connect, run, back off with jitter, reconnect.
pub async fn run(relay: Arc<Relay>, api: Arc<crate::api::Api>) -> Result<()> {
    let ws_url = format!("{}/relay/ws", api.server.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1));
    let deploy = crate::deploy::Watcher::new(relay.paths.clone(), api.clone());
    tokio::spawn(async move { deploy.run().await });
    // An automatic project has no logo yet: find it in the bound folder,
    // One try per project per run.
    let (r2, a2) = (relay.clone(), api.clone());
    tokio::spawn(async move {
        let mut tried = std::collections::HashSet::new();
        loop {
            let todo: Vec<Binding> = r2.inner.lock().unwrap().bindings.values()
                .filter(|b| (!b.has_icon || !b.has_description) && !tried.contains(&b.project_id)).cloned().collect();
            for b in todo {
                tried.insert(b.project_id.clone());
                if !b.has_icon {
                    if let Some(bytes) = crate::icon::detect(&b.cwd) {
                        if let Err(e) = a2.icon(&b.handle, bytes).await {
                            log(&r2.paths, "warn", "icon upload failed", json!({"project": b.handle, "error": e.to_string()}));
                        }
                    }
                }
                if !b.has_description {
                    if let Some(text) = crate::icon::readme_summary(&b.cwd) {
                        let _ = a2.call("POST", &format!("/relay/api/projects/{}/description", b.handle), Some(json!({"text": text}))).await;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
    let mut backoff = 1u64;
    loop {
        let started = Instant::now();
        let outcome = match api.access(false).await {
            Ok((tok, _)) => relay.session(&ws_url, &tok).await,
            Err(e) => Err(e),
        };
        if let Err(e) = &outcome {
            let msg = e.to_string();
            log(&relay.paths, "warn", "disconnected", json!({"error": msg}));
            if msg.contains("signed out") || msg.contains("not signed in") {
                return Err(anyhow!(msg));
            }
            if msg.contains("401") {
                let _ = api.access(true).await;
            }
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 1;
        }
        let jitter = rand::random::<u64>() % 1000;
        tokio::time::sleep(Duration::from_millis(backoff * 1000 + jitter)).await;
        backoff = (backoff * 2).min(60);
    }
}
