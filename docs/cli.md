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
3. Turn on the work log. Each finished Claude Code or Codex turn then goes to Substrate with a summary.
   ```sh
   substrate log install
   substrate log backfill          # optional: the turns already on this Mac
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
| `log install` / `log uninstall` | Adds or removes the work log hooks in Claude Code (Stop, and PreToolUse on AskUserQuestion) and Codex (Stop). Your other hooks stay. |
| `log status` | Shows the hooks, the upload queue, the summaries that wait or failed, and the size of the local store. |
| `log backfill [--summarize] [--since 7d] [--include-exec]` | Imports the turns of the transcripts on this Mac. Without `--summarize` it imports only the stats. `--since` limits the summaries to recent turns. Codex `exec` sessions are skipped unless you add `--include-exec`. |

## Agent commands

Each delivered message ends with these commands and its message id. Run them in the project directory, on the same computer.

| Command | Result |
|---|---|
| `substrate progress <id> --text "…"` | Posts progress at real milestones. |
| `substrate note <id> --text "…"` | Tells the tester, as the developer, what the developer changed or asked for in the agent's session. |
| `substrate propose <id> --text "…" --why "…"` | In a review only: sends the request as the developer and the agent agreed it, and a line for the tester on why it changed. It waits for the developer's approval. |
| `substrate hold <id> --why "…"` | For a follow-up that came without its own approval: it does not fit the conversation's work, so the agent stops and sends it back to the developer as a held message. |
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
| `worklog/queue/` | turns from the hooks that wait for upload |
| `worklog/turns/` | each uploaded turn with its tool log, diffs and images (stays on this Mac) |
| `worklog/jobs/` | summary jobs, and summary states the cloud did not get yet |
| `worklog/asks/` | the open AskUserQuestion of each session, from the `claude-ask` hook |
| `worklog/offsets/` | how far each transcript was read |
| `worklog/hook-error.log` | errors of the hooks (a hook never stops the agent) |

The credentials are in the Keychain (service `sh.gliph.substrate`). On a system without a Keychain, they are in `credentials.json` (mode 0600).

## Work log

The hooks run `substrate hook claude|codex|claude-ask`. A hook reads the new turns from the agent's transcript and writes them to `worklog/queue/`. It uses no network and never stops the agent.

The relay then does these steps for each turn:

1. It masks secrets in the prompt, the reply, the diffs and the tool log.
2. It uploads the prompt, the final reply, the tokens, the time, the tools, the changed file names and the attachment names.
3. It keeps the tool log, the diffs and the images on this Mac. The cloud asks the relay for them only when you open the turn, and only while this Mac is online.
4. It writes the summary on this Mac with the CLI of the agent that did the turn: `claude -p` with Haiku, or `codex exec` with your default Codex model. Then it uploads the summary. A failed summary is tried again after 30 seconds and 2 minutes.

Turns are grouped by git repository: the normalised `origin` remote, or else the main worktree folder.

## Herdr

The relay uses these Herdr commands:

- `herdr agent list`
- `herdr agent prompt`: one write with a bracketed paste and Enter. A blocked agent gets no input.
- `herdr agent read`, also to show the dialog of a blocked agent
- `herdr agent get` and `herdr agent send-keys`, only when you answer an agent

The relay does not start or stop an agent. It sends keys or a prompt of its own only when you, the account owner, answer a blocked agent from Sidra or the web. To answer an AskUserQuestion, the relay cancels the dialog with Esc. When the dialog is closed, it sends your answer as the next prompt. While the agent works, it sends nothing. Each agent pane in Herdr is one project. An agent restarted in the same folder keeps its project. A pane closed for 10 minutes takes its project with it.
