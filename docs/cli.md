# The substrate CLI

One Rust binary: it pairs your computer, runs the relay, binds Herdr panes to projects, and gives agents a way to report back.

## Install

```sh
curl -fsSL https://substrate.gliph.sh/install | sh
```

The installer downloads the build for your Mac, checks its SHA-256 and puts it in `~/.local/bin`. Nothing runs as root.

## Set up

1. Pair this computer. The command opens your browser; confirm the code there.
   ```sh
   substrate login
   ```
2. See the agents Herdr is running.
   ```sh
   substrate herdr discover
   ```
3. Create a project bound to one pane.
   ```sh
   substrate project add verna --pane w9P:p1
   ```
4. Keep the relay running in the background. It starts at login and restarts after a crash.
   ```sh
   substrate daemon install
   ```
5. Check everything.
   ```sh
   substrate doctor
   ```

Invite testers from the project page on https://substrate.gliph.sh.

## Commands

| command | does |
|---|---|
| `login [--server URL] [--name NAME]` | Pair with the device flow. Credentials go to the macOS Keychain. |
| `logout` | Sign this computer out and stop the relay. |
| `status` | Account, connection, LaunchAgent and project state. |
| `doctor` | Check Herdr, credentials, the server, the connection, the LaunchAgent and `gh`. |
| `daemon install` / `start` | Install and load the LaunchAgent `sh.gliph.substrate`. |
| `daemon stop` | Unload it. Messages stay queued in Substrate. |
| `daemon restart` | Restart the running relay. |
| `daemon run` | Run in the foreground (what launchd runs). |
| `herdr discover` | List Herdr's agents: pane, kind, status, title, directory. |
| `projects` | Your projects with pane, state, relay mode and pending count. |
| `project add <handle> --pane <id> [--name]` | Create a project bound to a pane here. |
| `project bind <handle> <pane>` | Bind or move a project. The only way its target changes. |
| `project unbind <handle>` | Deliver nowhere; messages stay queued. |
| `project mode <handle> approve` | Every message waits for you. |
| `project mode <handle> auto [--for 1h\|8h\|<min>] --yes` | Auto relay. Flagged messages still wait. |
| `inbox` / `approvals` | Recent messages / messages waiting for you. |
| `approve <id>` / `reject <id>` | Decide on a message. |
| `retry <id>` / `cancel <id>` | Deliver an uncertain or failed message again / cancel before delivery. |
| `logs [-f]` | The relay's JSON log. |

## For agents

Every delivered message ends with these commands and its message id. Run them in the project directory, on the same computer.

| command | does |
|---|---|
| `substrate progress <id> --text "…"` | Post progress at real milestones. |
| `substrate ask <id> --text "…"` | Ask the tester. Their answer arrives as a new message. |
| `substrate reply <id> --final --text "…" [--attach file]` | Finish: what changed, what was tested, what was not done. |
| `substrate deploy track <id> --commit <sha> [--verify-url URL] [--expect-sha]` | Watch CI (`gh`) and the live URL; report each stage. |
| `substrate deploy status <id> --stage <stage>` | Report a stage you saw yourself (`deployed`, `not_deploying`, …). |

`--text -` reads the text from stdin. Attachments must be inside the project directory.

## Files

`~/Library/Application Support/substrate/`: `config.json` (server), `status.json` (the relay's state), `dispatch/` (one durable record per delivered message), `deploy/` (deploy watch jobs), `workspace/<project>/<message>/` (downloaded attachments, 0600), `logs/daemon.log` (rotated at 5 MB). Credentials live in the Keychain (service `sh.gliph.substrate`), or `credentials.json` (0600) where no Keychain exists.

## Herdr

The relay uses `herdr agent list`, `herdr agent prompt` (bracketed paste plus Enter as one write; refuses a blocked agent) and `herdr agent read`. It never starts, stops or answers an agent, and never sends keys. A project is bound to a pane id and the agent's session id; if either changes, delivery stops until you run `project bind`.
