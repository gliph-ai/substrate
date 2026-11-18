# Substrate

Substrate lets people reach a developer's running coding agents through their
own AI assistant. It is a Gliph product, built in Rust: the cloud
(`server/`, axum + PostgreSQL) runs on `substrate.gliph.sh` as its own Pushleft
project, and the local relay is the `substrate` CLI and daemon (`cli/`). Both
compile the same wire contract (`proto/`). Inside Gliph, Substrate is a plugin
on gliph.sh: it registers `/substrate` at runtime and posts into linked
channels.

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
            └──── substrate progress|ask|reply|deploy ◀─────────┘
```

## Identities

| identity | what it is | credential |
|---|---|---|
| account | a person, by email | magic-link or Sign in with Gliph → `sub_session` cookie |
| machine | one paired daemon install | refresh token (Keychain) → 1 h access token |
| endpoint | a project bound to (machine, herdr pane, agent session id) | none; addressed by the machine |
| client | an MCP client a tester connected | OAuth access/refresh tokens, or a personal token |
| reply token | one delivered message | random, scoped to (project, conversation, message) |

## Message lifecycle

`stage` (message + execution):
`pending_approval → approved → dispatched → accepted → submitted → working →
(awaiting_sender) → completed` and the exits `rejected | expired | cancelled |
failed | interrupted | uncertain | held`.

- `held` — a risk flag fired; manual approval is required even in auto relay.
- `uncertain` — the daemon crashed between writing the dispatch record and
  confirming the submit. Never retried automatically; the developer decides.
- `completed` — the agent posted `substrate reply --final`. An agent that goes
  idle without one leaves the message at `working` with `agent_idle=true`; the
  summary says so and invents nothing.

`deployment_stage`: `fix_committed → ci_running → ci_passed | ci_failed →
deploying → deployed | deploy_failed → verified → ready_to_retest`, plus
`not_deploying` and `unknown` (watcher went silent past its timeout).

Every transition is an event with a per-conversation `seq`.

## Relay protocol (daemon ⇄ cloud), v1

### Pairing (RFC 8628 device flow)

1. `POST /relay/pair/start {"machine_name","platform","version"}` →
   `{"device_code","user_code","verification_uri","interval","expires_in"}`.
   `user_code` is 8 letters `XXXX-XXXX`; it lives 10 minutes.
2. The developer opens `verification_uri` signed in, checks the code, approves.
3. Daemon polls `POST /relay/pair/token {"device_code"}` every `interval` s →
   `{"error":"authorization_pending"}` (400) until approved, then
   `{"machine_id","refresh_token","access_token","expires_in"}`.
4. `POST /relay/token {"refresh_token"}` → new `{"refresh_token","access_token","expires_in"}`.
   The old refresh token stops working (rotation). A reused old refresh token
   revokes the machine (theft signal).

All other relay calls carry `Authorization: Bearer <access_token>`.

### WebSocket `GET /relay/ws`

JSON text frames `{"t":"<type>", ...}`. The cloud pings every 25 s; the daemon
reconnects with jittered backoff 1 s → 60 s.

daemon → cloud:

| t | fields | meaning |
|---|---|---|
| `hello` | `version`, `protocol:1`, `panes:[Pane]` | first frame; `panes` = `herdr agent list` |
| `panes` | `panes:[Pane]` | the live pane inventory changed |
| `status` | `project_id`, `state` (`idle|working|blocked|done|unknown|offline|needs_rebind`) | endpoint state |
| `ack` | `delivery_id`, `stage` (`accepted|submitted|working|uncertain|failed|agent_idle`), `detail?` | delivery progress |
| `excerpt` | `message_id`, `text` | bounded terminal excerpt (developer-only, never sent to the tester) |

`Pane` = `{"pane_id","agent","agent_session","status","cwd","title"}` straight
from `herdr agent list` (`agent_session` = `.agent_session.value`).

cloud → daemon:

| t | fields | meaning |
|---|---|---|
| `welcome` | `machine_id`, `projects:[Binding]` | after `hello` |
| `bindings` | `projects:[Binding]` | a binding changed |
| `deliver` | `delivery_id`, `message_id`, `project_id`, `envelope`, `reply_token`, `attachments:[{name,url,size,sha256}]` | deliver one message |
| `cancel` | `delivery_id` | developer cancelled before submit |

`Binding` = `{"project_id","handle","pane_id","agent_session","cwd"}`.

### Delivery rules (the daemon's contract)

1. On `deliver`: write `dispatch/<message_id>.json` (state `received`), `ack accepted`.
2. Check the pane: `herdr agent get <pane_id>`. If missing, or its
   `agent_session.value` differs from the binding → `status needs_rebind`, keep
   the record, do not submit. If `blocked` → `status blocked`, wait. If
   `working` → wait (one in flight per project; projects run concurrently).
3. Download attachments into `<workspace>/<project>/<message_id>/` with the
   given safe names; verify size and sha256; files 0600, no symlinks followed.
4. Replace the U+001F-delimited token `\u001fATTACH_DIR\u001f` in `envelope` with
   that folder's absolute path (`proto::ATTACH_DIR`; the sanitizer strips
   U+001F from tester text, so it cannot be forged). Reject an envelope
   containing any other C0 control character except `\n` and `\t`.
5. Mark the record `submitting`, then `herdr agent prompt <pane_id> <envelope>`
   (no `--wait`; herdr bracketed-pastes and refuses a blocked agent). On success
   mark `submitted`, `ack submitted`; then watch `herdr agent wait` / `get` and
   `ack working`, and when it settles idle/done with no final reply, `ack
   agent_idle` plus one `excerpt` (`herdr agent read --source recent-unwrapped --lines 120`, capped 8 KB).
6. On restart, a record still in `submitting` → `ack uncertain`. Never resubmit.
7. Write `dispatch/<message_id>.json` with the `reply_token` so `substrate
   reply/progress/ask/deploy <message_id>` can find it.

### Agent reports (CLI → cloud, HTTPS)

`POST /relay/report` with the machine bearer:

```json
{"message_id":"m_…","reply_token":"…","kind":"progress|ask|reply|final|deploy",
 "text":"…","deploy":{"stage":"ci_running","commit":"3f2a9c1","ci_url":"…",
 "env":"production","verify_url":"…","run_id":"…","note":"…"}}
```

Attachments: `POST /relay/report/attachment?message_id=&reply_token=` with the
raw bytes and `X-Filename`; returns `{"attachment_id"}` to list in a report's
`attachments`. The CLI only sends files inside the bound project's cwd after
resolving symlinks, ≤ 10 MB.

The cloud checks the reply token against the message, scans the text for
secrets (a hit holds the report for the developer), and turns links outside
the Substrate domain and the project's retest allowlist into plain text.

### Deploy watching (daemon)

`substrate deploy track <id> --commit <sha> [--ci-url <url>] [--env <name>]
[--verify-url <url>]`: the daemon reports `fix_committed`, then polls every
15 s with the developer's own tools — `gh run list --commit <sha> --json
databaseId,status,conclusion,url,name` for GitHub Actions, `glab ci list`
style for GitLab when `--ci-url` is a GitLab URL, and otherwise waits for the
agent's own `substrate deploy status` calls. After a passing pipeline and
`deployed`, a `--verify-url` GET that returns 2xx (and contains the short sha
when `--expect-sha` is given) reports `verified` then `ready_to_retest`.
Silence for 45 minutes reports `unknown`. The cloud never receives GitHub,
GitLab or Pushleft credentials.

## MCP (`/mcp`, Streamable HTTP, protocol 2025-06-18)

Tools: `list_projects`, `get_project`, `send_message`, `get_message`,
`wait_for_update`, `reply`, `get_conversation`, `get_status`,
`list_artifacts`, `create_upload_link`. Every message result carries the status
block (`stage`, `deployment_stage`, `terminal`, `summary`, `next_action`,
`check_after_seconds`, `seq`, `new_events`, `links`, `question`). Agent-written
text is only ever in an `agent_text` field.

Auth: OAuth 2.1 with PKCE and dynamic client registration
(`/.well-known/oauth-protected-resource`,
`/.well-known/oauth-authorization-server`, `/oauth/register`,
`/oauth/authorize`, `/oauth/token`). Clients that cannot do OAuth use a
personal token from the invite page as `Authorization: Bearer sk_sub_…`.

## Gliph integration

- **Sign in with Gliph**: `substrate.gliph.sh` sends the browser to gliph.sh's
  own `/link.html` approval (the device-link flow, `platform=substrate`; gliph
  accepts that redirect only when it equals its `-substrate-callback` flag).
  One click on "continue"; Substrate redeems the code with PKCE, reads the
  account's nick and email, signs the minted Gliph session straight out, and
  matches the Substrate account by email.
- **Agent channel**: Substrate connects to gliph.sh as the agent plugin
  `substrate` (`GLIPH_PLUGIN_TOKEN`). A channel op adds it with `/invite substrate`;
  then `/substrate link <project>` in that channel
  mirrors that project's conversations into it as `[project]` lines, and
  `/substrate approve|reject|send|status|mode` act on it from chat. A line
  `@<project> text` in a linked channel, from a member or another agent plugin,
  becomes a message to that project's agent (agent to agent); unless its
  author is the developer, it waits for approval like any tester's.
