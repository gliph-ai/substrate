# App API (`/api/v1`)

What Sidra's Substrate tab uses. JSON over HTTPS at `https://substrate.gliph.sh/api/v1`.
Every call but the two sign-in calls sends `Authorization: Bearer <token>`. A `401` means
sign in again. Errors are `{"error": "<text to show>"}`.

The approved design is in `design/sidra/` (requests, sessions, sign-in, and the HTML mockup
drawn with Sidra's mockup kit).

## Sign in: email and a 6-digit code

| Call | Body | Answer |
| --- | --- | --- |
| `POST /auth/code` | `{"email"}` | `{"ok": true, "expires_in": 600}`. Mails a 6-digit code. Rate limited (`429`). |
| `POST /auth/verify` | `{"email", "code", "device"}` | `{"token", "expires_in", "account"}`. Spaces in the code are ignored. 5 tries per 10 minutes; a code works once. |
| `POST /auth/logout` | none | `{"ok": true}`. The token stops working. |

The token lasts 180 days. Store it in the Keychain; keep the base URL and email outside it.

## Read

| Call | Answer |
| --- | --- |
| `GET /me` | `{"id", "email", "name", "avatar_url"}` |
| `GET /dashboard` | `{"counts": {"waiting", "active", "agents", "working"}, "waiting": [Message], "active": [Message], "history": [Message], "new_session_url"}` |
| `GET /conversations/{id}` | `{"id", "project", "deploy_stage", "messages": [Message], "events": [Event]}` (events newest first) |
| `GET /sessions` | `{"sessions": [Session], "new_session_url"}` |
| `GET /sessions/{id}` | Session plus `"counts": {"messages", "waiting", "active", "completed", "deployed"}` and `"history": [{"id", "title", "project", "deploy_stage", "events": [Event]}]` |
| `GET /machines` | `{"machines": [{"id", "name", "platform", "relay_version", "online", "last_seen", "agents": [{"pane_id", "workspace", "agent", "status", "title", "project"}]}]}` |
| `GET /live` | Server-sent events: `data: changed` whenever anything of this account changes. Refetch what is on screen. Keep-alive comments every ~15 s. |

The badge is `counts.waiting` from `/dashboard`, refetched on each `changed`.

## Act

| Call | Answer |
| --- | --- |
| `POST /messages/{id}/approve` `[{"text", "why"}]` | `{"id", "stage": "approved"}`. With `text`, the agent gets that instead of the tester's text, and the tester sees it from the developer, with `why` when given. To approve a reviewed request, send its `proposal.text` and `proposal.why`. 400 when the text is empty or imitates Substrate's framing; 409 when the message moved on. |
| `POST /messages/{id}/reject` | `{"id", "stage": "rejected"}` |
| `POST /messages/{id}/review` | `{"id", "stage": "reviewing"}`. Sends the request to the project's agent, review only, so the developer can go over it with the agent in its session. The agent sends the agreed version back with `substrate propose`; the message then waits again, with `proposal` set. Call again to resend. 409 when the agent is offline or its relay is older than 0.1.15. |

## Push

| Call | Answer |
| --- | --- |
| `POST /devices` `{token, bundle, sandbox, client_ref}` | `{"ok": true}`. The APNs token (hex); `sandbox` for development-signed builds; `client_ref` comes back in each push. |
| `DELETE /devices/{token}` | `{"ok": true}`. Call it before `/auth/logout`. |

Each request that starts waiting (`pending_approval` or `held`), and each reviewed request that comes back, sends an alert push to the owner's devices:
category `substrate.approval`, `apns-collapse-id` `substrate.{message id}`, and `substrateMessage` and
`substrateConnection` (the `client_ref`) beside `aps`. Tokens APNs refuses as unknown are dropped.

Creating, editing and revoking sessions stay on the web: open `new_session_url` or a session's `web_url`.

## Shapes

**Message**: `id`, `conversation_id`, `kind` (feedback, question, task, answer), `text`, `stage`
(pending_approval, held, reviewing, approved, dispatched, accepted, submitted, working, awaiting_sender,
completed, rejected, cancelled, expired, failed, interrupted, uncertain), `deploy_stage` (null or
committing, running_checks, pushing, fix_committed, ci_running, ci_passed, ci_failed, deploying,
deployed, deploy_failed, verified, ready_to_retest, not_deploying, unknown), `deploy_allow`
(none, test, production), `risk` (comma-separated flags; non-empty means held), `proposal` (null, or
`{text, why}`: the request as agreed in a review, to approve), `client`,
`sender` `{id, name, avatar_url}`, `project` (Project), `created_at`, `updated_at`, `web_url`;
`prompt` (the exact text the agent gets) on waiting messages and in a conversation.

**Project**: `id`, `name`, `handle`, `agent` (e.g. "Claude Code"), `state` (idle, working,
blocked, done, needs_rebind, offline, unbound), `icon_url` (or null: draw a letter tile).
The project's `handle` and the machine agents' `pane_id` match Herdr: one project per pane.

**Session**: `id`, `name`, `avatar_url`, `projects` [Project], `permissions` `{replies,
deploys, files, no_approval, deploy_test, deploy_production}`, `waiting`, `last_used_at`,
`expires_at`, `active`, `web_url`.

**Event**: `seq`, `time`, `actor` (human, system, relay, agent, deploy), `name`, `kind`,
`text`, `stage`, `held`, `avatar_url` (the tester for human, the developer for their own
approvals; null otherwise: draw the agent's, CI's or Substrate's mark).

Deploy track steps, in order: Commit (committing), Checks (running_checks), Push (pushing),
CI (fix_committed, ci_running, ci_passed, ci_failed), Deploy (deploying, deployed,
deploy_failed), Live (verified, ready_to_retest).
