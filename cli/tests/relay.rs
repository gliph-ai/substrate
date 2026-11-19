//! The relay against a fake Herdr: what reaches an agent, when, and what is
//! reported. No real herdr pane is ever touched by these tests.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use sha2::{Digest, Sha256};
use substrate_cli::config::Paths;
use substrate_cli::daemon::{Fetch, Relay};
use substrate_cli::herdr::{Herdr, PromptError};
use substrate_proto::{AckStage, AttachmentRef, Binding, CloudFrame, DaemonFrame, Pane, ATTACH_DIR};
use tokio::sync::mpsc;

#[derive(Default)]
struct FakeHerdr {
    panes: Mutex<Vec<Pane>>,
    prompts: Mutex<Vec<(String, String)>>,
}

impl FakeHerdr {
    fn set(&self, pane: &str, session: &str, status: &str) {
        let mut ps = self.panes.lock().unwrap();
        ps.retain(|p| p.pane_id != pane);
        ps.push(Pane { pane_id: pane.into(), agent: "claude".into(), agent_session: session.into(), status: status.into(), cwd: "/w".into(), title: "t".into(), ..Default::default() });
    }
    fn remove(&self, pane: &str) {
        self.panes.lock().unwrap().retain(|p| p.pane_id != pane);
    }
    fn prompts(&self) -> Vec<(String, String)> {
        self.prompts.lock().unwrap().clone()
    }
}

impl Herdr for FakeHerdr {
    fn list(&self) -> Result<Vec<Pane>> {
        Ok(self.panes.lock().unwrap().clone())
    }
    fn prompt(&self, pane: &str, text: &str) -> std::result::Result<(), PromptError> {
        let p = self.panes.lock().unwrap().iter().find(|p| p.pane_id == pane).cloned();
        match p {
            None => Err(PromptError::Gone),
            Some(p) if p.status == "blocked" => Err(PromptError::Blocked),
            Some(_) => {
                self.prompts.lock().unwrap().push((pane.into(), text.into()));
                Ok(())
            }
        }
    }
    fn read(&self, _pane: &str, _lines: u32) -> Result<String> {
        Ok("…terminal tail…".into())
    }
}

#[derive(Default)]
struct FakeFetch(Mutex<HashMap<String, Vec<u8>>>);

impl Fetch for FakeFetch {
    fn fetch<'a>(&'a self, url: &'a str) -> Pin<Box<dyn Future<Output = Result<Vec<u8>>> + Send + 'a>> {
        let b = self.0.lock().unwrap().get(url).cloned();
        Box::pin(async move { b.ok_or_else(|| anyhow::anyhow!("404")) })
    }
}

struct H {
    _dir: tempfile::TempDir,
    paths: Paths,
    herdr: Arc<FakeHerdr>,
    fetch: Arc<FakeFetch>,
    relay: Arc<Relay>,
    rx: mpsc::UnboundedReceiver<DaemonFrame>,
}

fn harness() -> H {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::new(dir.path().to_path_buf()).unwrap();
    reopen(dir, paths, Arc::new(FakeHerdr::default()))
}

fn reopen(dir: tempfile::TempDir, paths: Paths, herdr: Arc<FakeHerdr>) -> H {
    let fetch = Arc::new(FakeFetch::default());
    let relay = Relay::new(paths.clone(), herdr.clone(), fetch.clone());
    let (tx, rx) = mpsc::unbounded_channel();
    relay.set_outbox(Some(tx));
    H { _dir: dir, paths, herdr, fetch, relay, rx }
}

fn bind(h: &H, projects: &[(&str, &str, &str)]) {
    h.relay.on_frame(CloudFrame::Welcome { machine_id: "mc".into(), projects: projects.iter().map(|(pid, pane, sess)| Binding {
        project_id: (*pid).into(), handle: pid.to_string(), pane_id: (*pane).into(), agent_session: (*sess).into(), cwd: "/w".into(), has_icon: true, has_description: true }).collect() });
}

fn deliver(h: &H, mid: &str, pid: &str, env: &str, atts: Vec<AttachmentRef>) {
    h.relay.on_frame(CloudFrame::Deliver { delivery_id: format!("d_{mid}"), message_id: mid.into(), project_id: pid.into(), envelope: env.into(), reply_token: "rt".into(), attachments: atts, deploy: "none".into() });
}

fn deliver_allowing(h: &H, mid: &str, pid: &str, allow: &str) {
    h.relay.on_frame(CloudFrame::Deliver { delivery_id: format!("d_{mid}"), message_id: mid.into(), project_id: pid.into(), envelope: "fix it".into(), reply_token: "rt".into(), attachments: vec![], deploy: allow.into() });
}

async fn tick(h: &H) {
    h.relay.poll_now();
    h.relay.tick().await;
}

fn drain(h: &mut H) -> Vec<DaemonFrame> {
    let mut v = vec![];
    while let Ok(f) = h.rx.try_recv() {
        v.push(f);
    }
    v
}

fn acks(frames: &[DaemonFrame], did: &str) -> Vec<AckStage> {
    frames.iter().filter_map(|f| match f { DaemonFrame::Ack { delivery_id, stage, .. } if delivery_id == did => Some(*stage), _ => None }).collect()
}

fn statuses(frames: &[DaemonFrame], pid: &str) -> Vec<String> {
    frames.iter().filter_map(|f| match f { DaemonFrame::Status { project_id, state } if project_id == pid => Some(state.clone()), _ => None }).collect()
}

#[tokio::test]
async fn happy_path_with_attachment() {
    let mut h = harness();
    h.herdr.set("w1:p1", "s1", "idle");
    bind(&h, &[("p1", "w1:p1", "s1")]);
    let bytes = b"\x89PNG\r\n\x1a\nimg".to_vec();
    h.fetch.0.lock().unwrap().insert("https://x/a".into(), bytes.clone());
    let att = AttachmentRef { name: "../../evil/shot.png".into(), url: "https://x/a".into(), size: bytes.len() as u64, sha256: hex::encode(Sha256::digest(&bytes)) };
    deliver(&h, "m1", "p1", &format!("hello\nAttachment: {ATTACH_DIR}/shot.png"), vec![att]);
    tick(&h).await;
    let f = drain(&mut h);
    assert_eq!(acks(&f, "d_m1"), vec![AckStage::Accepted, AckStage::Submitted]);
    let prompts = h.herdr.prompts();
    assert_eq!(prompts.len(), 1);
    let ws = h.paths.workspace().join("p1").join("m1");
    assert!(prompts[0].1.contains(&format!("{}/shot.png", ws.display())), "{}", prompts[0].1);
    assert!(!prompts[0].1.contains('\u{1f}'));
    let saved = ws.join("shot.png");
    assert_eq!(std::fs::read(&saved).unwrap(), bytes);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&saved).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!h.paths.workspace().join("evil").exists());

    h.herdr.set("w1:p1", "s1", "working");
    tick(&h).await;
    assert_eq!(acks(&drain(&mut h), "d_m1"), vec![AckStage::Working]);
    h.herdr.set("w1:p1", "s1", "idle");
    tick(&h).await;
    let f = drain(&mut h);
    assert_eq!(acks(&f, "d_m1"), vec![AckStage::AgentIdle]);
    assert!(f.iter().any(|f| matches!(f, DaemonFrame::Excerpt { .. })));
    assert_eq!(h.herdr.prompts().len(), 1, "never typed twice");
}

#[tokio::test]
async fn final_reply_skips_agent_idle() {
    let mut h = harness();
    h.herdr.set("w1:p1", "s1", "idle");
    bind(&h, &[("p1", "w1:p1", "s1")]);
    deliver(&h, "m1", "p1", "hi", vec![]);
    tick(&h).await;
    h.herdr.set("w1:p1", "s1", "working");
    tick(&h).await;
    std::fs::write(h.paths.dispatch().join("m1.final"), b"").unwrap();
    h.herdr.set("w1:p1", "s1", "idle");
    tick(&h).await;
    let f = drain(&mut h);
    assert!(!acks(&f, "d_m1").contains(&AckStage::AgentIdle));
}

#[tokio::test]
async fn missing_pane_or_new_session_never_delivers() {
    let mut h = harness();
    bind(&h, &[("p1", "w1:p1", "s1")]);
    deliver(&h, "m1", "p1", "hi", vec![]);
    tick(&h).await;
    assert!(h.herdr.prompts().is_empty());
    assert!(statuses(&drain(&mut h), "p1").contains(&"needs_rebind".to_string()));
    // The same pane id now hosts a different agent session: still nothing.
    h.herdr.set("w1:p1", "other-session", "idle");
    tick(&h).await;
    assert!(h.herdr.prompts().is_empty());
    // The developer rebinds: delivery goes ahead.
    bind(&h, &[("p1", "w1:p1", "other-session")]);
    tick(&h).await;
    assert_eq!(h.herdr.prompts().len(), 1);
}

#[tokio::test]
async fn busy_and_blocked_agents_wait_and_order_holds() {
    let mut h = harness();
    h.herdr.set("w1:p1", "s1", "blocked");
    h.herdr.set("w2:p1", "s2", "idle");
    bind(&h, &[("p1", "w1:p1", "s1"), ("p2", "w2:p1", "s2")]);
    deliver(&h, "a", "p1", "first", vec![]);
    deliver(&h, "b", "p1", "second", vec![]);
    deliver(&h, "c", "p2", "other project", vec![]);
    tick(&h).await;
    // p1 is at a permission prompt: nothing typed there. p2 goes ahead.
    let ps = h.herdr.prompts();
    assert_eq!(ps, vec![("w2:p1".to_string(), "other project".to_string())]);
    assert!(statuses(&drain(&mut h), "p1").contains(&"blocked".to_string()));

    h.herdr.set("w1:p1", "s1", "idle");
    tick(&h).await;
    assert_eq!(h.herdr.prompts().last().unwrap().1, "first");
    h.herdr.set("w1:p1", "s1", "working");
    tick(&h).await;
    tick(&h).await;
    assert_eq!(h.herdr.prompts().iter().filter(|p| p.0 == "w1:p1").count(), 1, "second waits while the first works");
    h.herdr.set("w1:p1", "s1", "idle");
    tick(&h).await; // first finishes
    tick(&h).await; // second goes
    let p1: Vec<String> = h.herdr.prompts().into_iter().filter(|p| p.0 == "w1:p1").map(|p| p.1).collect();
    assert_eq!(p1, vec!["first", "second"]);
}

#[tokio::test]
async fn duplicate_delivery_is_typed_once() {
    let mut h = harness();
    h.herdr.set("w1:p1", "s1", "idle");
    bind(&h, &[("p1", "w1:p1", "s1")]);
    deliver(&h, "m1", "p1", "hi", vec![]);
    tick(&h).await;
    // The cloud re-sends after a reconnect, with a new delivery id.
    h.relay.on_frame(CloudFrame::Deliver { delivery_id: "d_new".into(), message_id: "m1".into(), project_id: "p1".into(), envelope: "hi".into(), reply_token: "rt2".into(), attachments: vec![], deploy: "none".into() });
    tick(&h).await;
    assert_eq!(h.herdr.prompts().len(), 1);
    assert_eq!(acks(&drain(&mut h), "d_new"), vec![AckStage::Submitted]);
}

#[tokio::test]
async fn restart_mid_submit_is_uncertain_and_not_resubmitted() {
    let h = harness();
    h.herdr.set("w1:p1", "s1", "idle");
    bind(&h, &[("p1", "w1:p1", "s1")]);
    deliver(&h, "m1", "p1", "hi", vec![]);
    // Simulate a crash between "submitting" and the herdr call returning.
    let mut r = h.relay.record("m1").unwrap();
    r.state = "submitting".into();
    std::fs::write(h.paths.dispatch().join("m1.json"), serde_json::to_vec(&r).unwrap()).unwrap();
    let (dir, paths, herdr) = (h._dir, h.paths, h.herdr);
    let mut h = reopen(dir, paths, herdr);
    bind(&h, &[("p1", "w1:p1", "s1")]);
    h.relay.connected();
    tick(&h).await;
    tick(&h).await;
    assert!(h.herdr.prompts().is_empty(), "never resubmitted");
    assert_eq!(acks(&drain(&mut h), "d_m1"), vec![AckStage::Uncertain]);
    assert_eq!(h.relay.record("m1").unwrap().state, "uncertain");
}

#[tokio::test]
async fn bad_attachment_or_envelope_fails_without_typing() {
    let mut h = harness();
    h.herdr.set("w1:p1", "s1", "idle");
    bind(&h, &[("p1", "w1:p1", "s1")]);
    h.fetch.0.lock().unwrap().insert("https://x/a".into(), b"tampered".to_vec());
    deliver(&h, "m1", "p1", "hi", vec![AttachmentRef { name: "a.png".into(), url: "https://x/a".into(), size: 8, sha256: "00".repeat(32) }]);
    deliver(&h, "m2", "p1", "clear\u{1b}[2J", vec![]);
    tick(&h).await;
    tick(&h).await;
    let f = drain(&mut h);
    assert_eq!(acks(&f, "d_m1").last(), Some(&AckStage::Failed));
    assert_eq!(acks(&f, "d_m2").last(), Some(&AckStage::Failed));
    assert!(h.herdr.prompts().is_empty());
}

#[tokio::test]
async fn pane_closing_mid_work_fails_the_message() {
    let mut h = harness();
    h.herdr.set("w1:p1", "s1", "idle");
    bind(&h, &[("p1", "w1:p1", "s1")]);
    deliver(&h, "m1", "p1", "hi", vec![]);
    tick(&h).await;
    h.herdr.remove("w1:p1");
    tick(&h).await;
    assert_eq!(acks(&drain(&mut h), "d_m1").last(), Some(&AckStage::Failed));
}

#[tokio::test]
async fn reconnect_over_a_real_socket() {
    use futures_util::{SinkExt, StreamExt};
    let h = harness();
    h.herdr.set("w1:p1", "s1", "idle");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/relay/ws", listener.local_addr().unwrap());
    let relay = h.relay.clone();
    let server = tokio::spawn(async move {
        let mut seen = vec![];
        for round in 0..2 {
            let (s, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(s).await.unwrap();
            let hello = ws.next().await.unwrap().unwrap();
            assert!(hello.to_text().unwrap().contains("\"t\":\"hello\""));
            let welcome = CloudFrame::Welcome { machine_id: "mc".into(), projects: vec![Binding { project_id: "p1".into(), handle: "p1".into(), pane_id: "w1:p1".into(), agent_session: "s1".into(), cwd: "/w".into(), has_icon: true, has_description: true }] };
            ws.send(serde_json::to_string(&welcome).unwrap().into()).await.unwrap();
            if round == 0 {
                let d = CloudFrame::Deliver { delivery_id: "d1".into(), message_id: "m1".into(), project_id: "p1".into(), envelope: "hi".into(), reply_token: "rt".into(), attachments: vec![], deploy: "none".into() };
                ws.send(serde_json::to_string(&d).unwrap().into()).await.unwrap();
            }
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(2500);
            while let Ok(Some(Ok(m))) = tokio::time::timeout_at(deadline, ws.next()).await {
                if let Ok(t) = m.to_text() { seen.push((round, t.to_string())); }
            }
            drop(ws); // the server goes away; the relay must come back
        }
        seen
    });
    let r2 = relay.clone();
    let u2 = url.clone();
    tokio::spawn(async move {
        for _ in 0..2 {
            let _ = r2.session(&u2, "tok").await;
        }
    });
    let seen = server.await.unwrap();
    assert!(seen.iter().any(|(r, t)| *r == 0 && t.contains("\"stage\":\"submitted\"")), "{seen:?}");
    // After reconnecting the relay re-reports the open record.
    assert!(seen.iter().any(|(r, t)| *r == 1 && t.contains("\"delivery_id\":\"d1\"")), "{seen:?}");
    assert_eq!(h.herdr.prompts().len(), 1);
}

#[test]
fn herdr_refusals_are_not_uncertain() {
    use substrate_cli::herdr::classify;
    assert!(matches!(classify(r#"{"error":{"code":"agent_blocked","message":"x"}}"#), Err(PromptError::Blocked)));
    assert!(matches!(classify(r#"{"error":{"code":"agent_not_ready","message":"agent w1:p1 is no longer the pane foreground process"}}"#), Err(PromptError::NotReady)));
    assert!(matches!(classify(r#"{"error":{"code":"agent_not_found","message":"x"}}"#), Err(PromptError::Gone)));
    assert!(matches!(classify("socket closed"), Err(PromptError::Other(_))));
}

/// The guard reads the relay's own records: while a tester's message is with the
/// agent, a deploy it does not allow is blocked; after the reply, nothing is.
#[tokio::test]
async fn guard_blocks_deploys_the_tester_may_not_make() {
    let mut h = harness();
    h.herdr.set("w1:p1", "s1", "idle");
    bind(&h, &[("p1", "w1:p1", "s1")]);
    deliver_allowing(&h, "m1", "p1", "test");
    tick(&h).await;
    drain(&mut h);
    let hook = |cmd: &str, session: &str, cwd: &str| substrate_cli::guard::check(&h.paths, &serde_json::json!({
        "tool_name": "Bash", "tool_input": {"command": cmd}, "session_id": session, "cwd": cwd}));
    assert!(hook("git push origin HEAD:main", "s1", "/elsewhere").is_err(), "production deploy blocked");
    assert!(hook("git push origin staging", "s1", "/elsewhere").is_ok(), "test deploy allowed");
    assert!(hook("cargo test", "s1", "/elsewhere").is_ok());
    assert!(hook("git push", "other", "/w/sub").is_err(), "same project folder is covered too");
    assert!(hook("git push", "other", "/elsewhere").is_ok(), "other sessions are not affected");
    std::fs::write(h.paths.dispatch().join("m1.final"), b"").unwrap();
    assert!(hook("git push", "s1", "/w").is_ok(), "after the final reply the developer is free again");
}
