//! Keeping the daemon alive on macOS: a per-user LaunchAgent that starts at
//! login, restarts after a crash, and needs no open terminal.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Result};

pub const LABEL: &str = "sh.gliph.substrate";

pub fn plist_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn uid() -> String {
    String::from_utf8_lossy(&Command::new("id").arg("-u").output().map(|o| o.stdout).unwrap_or_default()).trim().to_string()
}

pub fn plist(exe: &str, logs: &str, path_env: &str, server: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>daemon</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Background</string>
  <key>EnvironmentVariables</key><dict>
    <key>PATH</key><string>{path}</string>
    <key>SUBSTRATE_SERVER</key><string>{server}</string>
  </dict>
  <key>StandardOutPath</key><string>{logs}/launchd.out.log</string>
  <key>StandardErrorPath</key><string>{logs}/launchd.err.log</string>
</dict>
</plist>
"#, exe = esc(exe), logs = esc(logs), path = esc(path_env), server = esc(server))
}

pub fn install(exe: &str, logs: &str, server: &str) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("`daemon install` works on macOS only. Run `substrate daemon run` from your service manager.");
    }
    // herdr and gh must be reachable from launchd's minimal environment.
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let p = plist_path();
    std::fs::create_dir_all(p.parent().unwrap())?;
    let _ = uninstall();
    std::fs::write(&p, plist(exe, logs, &path, server))?;
    let st = Command::new("launchctl").args(["bootstrap", &format!("gui/{}", uid()), &p.to_string_lossy()]).status()?;
    if !st.success() {
        bail!("launchctl bootstrap failed");
    }
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = Command::new("launchctl").args(["bootout", &format!("gui/{}/{LABEL}", uid())]).stderr(std::process::Stdio::null()).status();
    let _ = std::fs::remove_file(plist_path());
    Ok(())
}

pub fn loaded() -> bool {
    Command::new("launchctl").args(["print", &format!("gui/{}/{LABEL}", uid())]).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

pub fn restart() -> Result<()> {
    Command::new("launchctl").args(["kickstart", "-k", &format!("gui/{}/{LABEL}", uid())]).status()?;
    Ok(())
}
