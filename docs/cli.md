# The substrate CLI

`substrate` is one Rust binary. Use it to pair your computer, run the relay and bind Herdr panes to projects. Your agents use it to send reports.

## Install

```sh
curl -fsSL https://substrate.gliph.sh/install | sh
```

The installer downloads the build for your Mac, checks its SHA-256 and copies it to `~/.local/bin`. It does not use root.

## Set up

1. Pair this computer. Confirm the code in the browser window that opens.
   ```sh
   substrate login
   ```
2. Install the relay as a background service. It starts at login and restarts after a crash.
   ```sh
   substrate daemon install
   ```
5. Check the setup.
   ```sh
   substrate doctor
   ```

Invite testers from the project page on https://substrate.gliph.sh.

## Commands

| Command | Result |
|---|---|
| `login [--server URL] [--name NAME]` | Pairs this computer with the device flow. Stores the credentials in the macOS Keychain. |
| `logout` | Signs this computer out and stops the relay. |
| `status` | Shows the account, the connection, the LaunchAgent and the project state. |
| `doctor` | Checks Herdr, the credentials, the server, the connection, the LaunchAgent and `gh`. |
| `daemon install` / `start` | Installs and loads the LaunchAgent `sh.gliph.substrate`. |
| `daemon stop` | Unloads the LaunchAgent. Messages stay in the Substrate queue. |
| `daemon restart` | Restarts the relay. |
| `daemon run` | Runs the relay in the foreground. launchd uses this command. |
| `herdr discover` | Lists Herdr's agents: pane, kind, status, title and directory. |
| `projects` | Lists your projects with pane, state, relay mode and pending count. |
| `project icon <handle> [--file PATH]` | Finds the project's logo again, or uploads the PNG or JPEG you give. |
| `project mode <handle> approve` | Holds each message for your approval. |
| `project mode <handle> auto [--for 1h\|8h\|<min>] --yes` | Turns on auto relay. Flagged messages still wait for you. |
| `inbox` / `approvals` | Lists recent messages / messages that wait for you. |
| `approve <id>` / `reject <id>` | Approves or rejects a message. |
| `retry <id>` / `cancel <id>` | Delivers an uncertain or failed message again / cancels a message before delivery. |
| `logs [-f]` | Shows the relay's JSON log. |

## Agent commands

Each delivered message ends with these commands and its message id. Run them in the project directory, on the same computer.

| Command | Result |
|---|---|
| `substrate progress <id> --text "…"` | Posts progress at real milestones. |
| `substrate note <id> --text "…"` | Tells the tester, as the developer, what the developer changed or asked for in the agent's session. |
| `substrate ask <id> --text "…"` | Sends a question to the tester. The answer comes as a new message. |
| `substrate reply <id> --final --text "…" [--attach file]` | Sends the final reply: the changes, the tests and the open items. |
| `substrate deploy track <id> --commit <sha> [--verify-url URL] [--expect-sha]` | Tracks CI (`gh`) and the live URL, and reports each stage. |
| `substrate deploy status <id> --stage <stage>` | Reports a stage that you saw (`deployed`, `not_deploying`, …). |

With `--text -`, the text comes from stdin. Attachments must be in the project directory.

## Files

All files are in `~/Library/Application Support/substrate/`:

| Path | Contents |
|---|---|
| `config.json` | the server address |
| `status.json` | the relay state |
| `dispatch/` | one durable record for each delivered message |
| `deploy/` | deploy tracking jobs |
| `workspace/<project>/<message>/` | downloaded attachments, mode 0600 |
| `logs/daemon.log` | the relay log, rotated at 5 MB |

The credentials are in the Keychain (service `sh.gliph.substrate`). On a system without a Keychain, they are in `credentials.json` (mode 0600).

## Herdr

The relay uses three Herdr commands:

- `herdr agent list`
- `herdr agent prompt`: one write with a bracketed paste and Enter. A blocked agent gets no input.
- `herdr agent read`

The relay does not start, stop or answer an agent, and does not send keys. Each agent pane in Herdr is one project. An agent restarted in the same folder keeps its project. A pane closed for 10 minutes takes its project with it.
