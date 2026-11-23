# Architecture

With Substrate, people use their own AI assistant to send messages to the coding agents that a developer has open. Substrate is a Gliph product, written in Rust. It has two parts:

- **The cloud** (`server/`, axum and PostgreSQL) runs on `substrate.gliph.sh` as its own Pushleft project.
- **The relay** is the `substrate` CLI and daemon (`cli/`).

Both parts compile the same wire contract (`proto/`). In Gliph, Substrate is a plugin on gliph.sh. It registers `/substrate` at runtime and posts into linked channels.

```
tester ─▶ their assistant (Claude, ChatGPT, any MCP client)
            │  MCP over HTTPS, OAuth            substrate.gliph.sh/mcp
            ▼
        Substrate cloud (Rust)  ── plugin ──▶ Gliph channel (#proj-agents)
            │  approvals, queue, audit, attachments, deploy tracker
            │  WebSocket the daemon dials (no inbound port)   /relay/ws
            ▼
        substrate daemon (launchd) ── herdr agent prompt ──▶ the existing agent pane
            ▲                                                   │
            └──── substrate progress|ask|reply|note|deploy ◀────┘
```

## Identities

| Identity | Meaning | Credential |
|---|---|---|
| account | a person, by email | magic link or Sign in with Gliph → `sub_session` cookie |
| machine | one paired daemon install | refresh token (Keychain) → 1 h access token |
| endpoint | a project bound to (machine, Herdr pane, agent session id) | none; the machine addresses it |
| client | an MCP client that a tester connected | OAuth access and refresh tokens, or a personal token |
| reply token | one delivered message | random, scoped to (project, conversation, message) |

## Message lifecycle

`stage` (message and execution):
`pending_approval → approved → dispatched → accepted → submitted → working →
(awaiting_sender) → completed`. The exit stages are `rejected | expired | cancelled |
failed | interrupted | uncertain | held`.

- A follow-up or an answer to the agent starts at `approved` when the developer approved the conversation (`conversations.approved`: set by an approval or a message from the developer, cleared by a rejection) and nothing in it still waits. Auto relay and trusted senders do not set it. The conversation row lock orders this check against decisions.
- `held`: a risk flag matched. The message needs manual approval, also in auto relay.
- `reviewing`: the developer sent the request to the agent review-only (`[SUBSTRATE REVIEW id]`, no deploy, no attachments) to go over it together. It holds the agent's queue. The agent's `substrate propose` stores `proposal` and `proposal_why` and puts it back to `pending_approval` (or `held`); every other report is refused. Approving the proposal delivers it as a normal message; the tester gets the developer's `edit` event with why. The relay types a review and the later real delivery of the same message both (0.1.15).
- `uncertain`: the daemon stopped after it wrote the dispatch record and before Herdr confirmed the submit. Substrate does not retry it. The developer decides.
- `completed`: the agent posted `substrate reply --final`. When an agent goes idle without a final reply, the message stays at `working` with `agent_idle=true`. The summary states this fact and adds nothing else.

`deployment_stage`: `fix_committed → ci_running → ci_passed | ci_failed →
deploying → deployed | deploy_failed → verified → ready_to_retest`, plus
`not_deploying` and `unknown` (no report from the tracker before its timeout).

Each transition is an event with a `seq` per conversation.

## Relay protocol (daemon ⇄ cloud), v1

### Pairing (RFC 8628 device flow)

1. `POST /relay/pair/start {"machine_name","platform","version"}` →
   `{"device_code","user_code","verification_uri","interval","expires_in"}`.
   `user_code` has 8 letters (`XXXX-XXXX`) and expires after 10 minutes.
2. The developer signs in, opens `verification_uri`, compares the code and approves.
3. The daemon sends `POST /relay/pair/token {"device_code"}` every `interval` seconds.
   The response is `{"error":"authorization_pending"}` (400) until approval, then
   `{"machine_id","refresh_token","access_token","expires_in"}`.
4. `POST /relay/token {"refresh_token"}` → a new `{"refresh_token","access_token","expires_in"}`.
   The old refresh token stops working (rotation). A second use of an old refresh token
   revokes the machine, because it shows a stolen credential.

All other relay calls send `Authorization: Bearer <access_token>`.

### WebSocket `GET /relay/ws`

JSON text frames `{"t":"<type>", ...}`. The cloud sends a ping every 25 s. The daemon
reconnects with a jittered backoff from 1 s to 60 s.

Daemon → cloud:

| t | Fields | Meaning |
|---|---|---|
| `hello` | `version`, `protocol:1`, `panes:[Pane]` | first frame; `panes` = `herdr agent list` |
| `panes` | `panes:[Pane]` | a change to the live list of panes |
| `status` | `project_id`, `state` (`idle|working|blocked|done|unknown|offline|needs_rebind`) | endpoint state |
| `ack` | `delivery_id`, `stage` (`accepted|submitted|working|uncertain|failed|agent_idle`), `detail?` | delivery progress |
| `excerpt` | `message_id`, `text` | a short terminal excerpt for the developer only; testers never get it |

`Pane` = `{"pane_id","agent","agent_session","status","cwd","title"}`, copied
from `herdr agent list` (`agent_session` = `.agent_session.value`).

Cloud → daemon:

| t | Fields | Meaning |
|---|---|---|
| `welcome` | `machine_id`, `projects:[Binding]` | the reply to `hello` |
| `bindings` | `projects:[Binding]` | a change to a binding |
| `deliver` | `delivery_id`, `message_id`, `project_id`, `envelope`, `reply_token`, `attachments:[{name,url,size,sha256}]` | one message to deliver |
| `cancel` | `delivery_id` | the developer cancelled before the submit |

`Binding` = `{"project_id","handle","pane_id","agent_session","cwd"}`.

### Delivery rules for the daemon

1. On `deliver`, write `dispatch/<message_id>.json` (state `received`) and send `ack accepted`.
2. Run `herdr agent get <pane_id>` to check the pane:
   - Pane missing, or its `agent_session.value` is not the bound session: send `status needs_rebind`, keep the record, do not submit.
   - Agent `blocked`: send `status blocked` and wait.
   - Agent `working`: wait. Each project has one message in flight. Different projects run at the same time.
3. Download the attachments into `<workspace>/<project>/<message_id>/` with the
   given safe names. Check the size and the sha256. Write the files with mode 0600 and do not follow symlinks.
4. In `envelope`, replace the token `\u001fATTACH_DIR\u001f` with the absolute path of that folder
   (`proto::ATTACH_DIR`). The sanitizer removes U+001F from tester text, so a tester cannot forge the token.
   Reject an envelope that has any other C0 control character, except `\n` and `\t`.
5. Set the record to `submitting`, then run `herdr agent prompt <pane_id> <envelope>`
   without `--wait`. Herdr pastes the text as a bracketed paste. A blocked agent gets no input.
   - On success, set `submitted` and send `ack submitted`.
   - Use `herdr agent wait` and `get` to follow the agent, and send `ack working`.
   - When the agent goes idle or done with no final reply, send `ack agent_idle` and one `excerpt`
     (`herdr agent read --source recent-unwrapped --lines 120`, 8 KB maximum).
6. On restart, a record in state `submitting` gets `ack uncertain`. Do not submit it again.
7. Keep the `reply_token` in `dispatch/<message_id>.json`. The commands `substrate
   reply/progress/ask/deploy <message_id>` use it.

### Agent reports (CLI → cloud, HTTPS)

`POST /relay/report` with the machine bearer token:

```json
{"message_id":"m_…","reply_token":"…","kind":"progress|ask|reply|final|deploy",
 "text":"…","deploy":{"stage":"ci_running","commit":"3f2a9c1","ci_url":"…",
 "env":"production","verify_url":"…","run_id":"…","note":"…"}}
```

Attachments: `POST /relay/report/attachment?message_id=&reply_token=` with the
raw bytes and `X-Filename`. The response is `{"attachment_id"}`, for the `attachments` list of a report.
The CLI sends only files of 10 MB or less from inside the bound project directory, after it resolves symlinks.

For each report, the cloud:

1. Compares the reply token with the message.
2. Scans the text for secrets. A match holds the report for the developer.
3. Converts to plain text each link to a host outside the Substrate domain and the project's retest allowlist.

### Deploy tracking (daemon)

`substrate deploy track <id> --commit <sha> [--ci-url <url>] [--env <name>] [--verify-url <url>]`

1. The daemon reports `fix_committed`.
2. Every 15 s, it polls with the developer's own tools:
   - GitHub Actions: `gh run list --commit <sha> --json databaseId,status,conclusion,url,name`.
   - GitLab, when `--ci-url` is a GitLab URL: `glab ci list`.
   - Other pipelines: the agent reports each stage with `substrate deploy status`.
3. After a passed pipeline and `deployed`, the daemon sends a GET to `--verify-url`. A 2xx response
   reports `verified`, then `ready_to_retest`. With `--expect-sha`, the response must also contain the short sha.
4. After 45 minutes with no report, the daemon reports `unknown`.

The cloud gets no GitHub, GitLab or Pushleft credentials.

## MCP

Endpoint `/mcp`, Streamable HTTP, protocol 2025-06-18.

Tools: `list_projects`, `get_project`, `send_message`, `get_message`,
`wait_for_update`, `reply`, `get_conversation`, `get_status`,
`list_artifacts`, `create_upload_link`. Each message result has the status
block (`stage`, `deployment_stage`, `terminal`, `summary`, `next_action`,
`check_after_seconds`, `seq`, `new_events`, `links`, `question`). Text that an agent wrote
is only in `agent_text` fields.

Auth: OAuth 2.1 with PKCE and dynamic client registration
(`/.well-known/oauth-protected-resource`,
`/.well-known/oauth-authorization-server`, `/oauth/register`,
`/oauth/authorize`, `/oauth/token`). A client without OAuth support uses a
personal token from the invite page, as `Authorization: Bearer sk_sub_…`.

## Gliph integration

### Sign in with Gliph

1. `substrate.gliph.sh` sends the browser to the `/link.html` approval page on gliph.sh
   (the device-link flow, `platform=substrate`). Gliph accepts the redirect only when it is
   the same as its `-substrate-callback` flag.
2. The user clicks "continue" once.
3. Substrate redeems the code with PKCE and gets the account's nick and email.
4. Substrate signs out the new Gliph session at once, and finds the Substrate account by email.

### Agent channel

Substrate connects to gliph.sh as the agent plugin `substrate` (`GLIPH_PLUGIN_TOKEN`).

1. A channel op adds the plugin with `/invite substrate`.
2. `/substrate link <project>` in that channel shows the project's conversations there as `[project]` lines.
3. `/substrate approve|reject|send|status|mode` act on the project from the chat.

In a linked channel, a member or another agent plugin can write `@<project> text`. That line becomes
a message to the project's agent, so one agent can send work to another. A message from anyone
except the developer waits for approval, the same as a tester's message.
