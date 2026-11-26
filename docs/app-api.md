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
| `GET /sessions/{id}` | Session plus `"counts": {"messages", "waiting", "active", "completed", "deployed"}`, `"history": [{"id", "title", "project", "deploy_stage", "events": [Event]}]` and `"available_projects": [Project]` (all of the developer's projects, to add) |
| `GET /machines` | `{"machines": [{"id", "name", "platform", "relay_version", "online", "last_seen", "agents": [{"pane_id", "workspace", "agent", "status", "title", "project"}]}]}` |
| `GET /live` | Server-sent events: `data: changed` whenever anything of this account changes. Refetch what is on screen. Keep-alive comments every ~15 s. |

The badge is `counts.waiting` from `/dashboard`, refetched on each `changed`.

## Act

| Call | Answer |
| --- | --- |
| `POST /messages/{id}/approve` `[{"text", "why"}]` | `{"id", "stage": "approved"}`. With `text`, the agent gets that instead of the tester's text, and the tester sees it from the developer, with `why` when given. To approve a reviewed request, send its `proposal.text` and `proposal.why`. 400 when the text is empty or imitates Substrate's framing; 409 when the message moved on. |
| `POST /messages/{id}/reject` | `{"id", "stage": "rejected"}` |
| `PATCH /sessions/{id}` `{"permissions": {...}, "projects": [id]}` | The session, as `GET /sessions/{id}`. Each permission given (`replies`, `deploys`, `files`, `no_approval`, `deploy_test`, `deploy_production`) is set in every project of the session; one left out keeps each project's value. `projects`, when given, is the full list of project ids the session reaches (pick from `available_projects`); an added project gets the session's permissions, off where its projects differ. 400 for an empty `projects` list or an unknown permission; 404 when the session is revoked or expired. |
| `POST /messages/{id}/review` | `{"id", "stage": "reviewing"}`. Sends the request to the project's agent, review only, so the developer can go over it with the agent in its session. The agent sends the agreed version back with `substrate propose`; the message then waits again, with `proposal` set. Call again to resend. 409 when the agent is offline or its relay is older than 0.1.15. |

## Push

| Call | Answer |
| --- | --- |
| `POST /devices` `{token, bundle, sandbox, client_ref}` | `{"ok": true}`. The APNs token (hex); `sandbox` for development-signed builds; `client_ref` comes back in each push. |
| `DELETE /devices/{token}` | `{"ok": true}`. Call it before `/auth/logout`. |

Each request that starts waiting (`pending_approval` or `held`), and each reviewed request that comes back, sends an alert push to the owner's devices:
category `substrate.approval`, `apns-collapse-id` `substrate.{message id}`, and `substrateMessage` and
`substrateConnection` (the `client_ref`) beside `aps`. Tokens APNs refuses as unknown are dropped.

Creating and revoking sessions stay on the web: open `new_session_url` or a session's `web_url`. Change a session's projects and permissions with `PATCH /sessions/{id}`.

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
deploys, files, no_approval, deploy_test, deploy_production}` (the last three true when any project
has them; each project in `projects` also has its own `permissions` `{no_approval, deploy_test,
deploy_production}`, so a permission that differs between projects can show as mixed), `waiting`, `last_used_at`,
`expires_at`, `active`, `web_url`.

**Event**: `seq`, `time`, `actor` (human, system, relay, agent, deploy), `name`, `kind`,
`text`, `stage`, `held`, `avatar_url` (the tester for human, the developer for their own
approvals; null otherwise: draw the agent's, CI's or Substrate's mark).

Deploy track steps, in order: Commit (committing), Checks (running_checks), Push (pushing),
CI (fix_committed, ci_running, ci_passed, ci_failed), Deploy (deploying, deployed,
deploy_failed), Live (verified, ready_to_retest).

## Work log (`/api/v1/log`)

The turns of Claude Code and Codex on your Macs, ported from Theasa. The shapes follow Theasa's API, so a screen
built for it reads these. A **project** here is a git repo (`log_repos`), not a Herdr pane; ids are integers.
Ranges are `range=today|week|month|all` (default week). Days and hours are the reader's: send `tz`, the minutes
east of UTC (`-getTimezoneOffset()`). Each write ends with a `changed` on `/live`.

The cloud keeps no diffs, images or tool log. A turn's `local` comes from the relay on the Mac that recorded it
(`machine_id`); when that Mac is offline or its relay is older than 0.1.17, `local` is null and `local_error` says why.

| Call | Answer |
| --- | --- |
| `GET /log/pulse?range&tz` | `{range, since, totals: {turns, agent_ms, tokens, cache_share, cost, priced, by_agent}, bars: [{label, day?, claude, codex}] (tokens; hours for today), days: [{day, items: [{project, turns, agents, last_at, health, pending, bullets: [{turn_id, text, verdict}], decisions, watch, files}]}], attention: [{id, text, impact, created_at, turn_id, project_id, project}], open_watch, decisions: [{id, text, created_at, turn_id, project_id, project}], decisions_count, worker: {written, tokens, cost, queued, running, failed, median_delay_s}, active: [{agent, session_id, machine, last_at, project_id, project}]}` |
| `GET /log/repos?range&tz` | `[Project + {last_at, turns, tokens, cost, agent_ms, open_watch, machines}]` |
| `GET /log/repos/{id}?range&tz&agent=claude\|codex&only=notes` | `{project, first_at, last_at, total_turns, machines: [{machine, n}], totals: {turns, agent_ms, tokens, cost}, hist: [minutes], turns: [Turn + {decisions, watch, notes: [{kind: decision\|watch, id, text, status, impact?}]}], decisions: [Decision], watch: [Watch], sessions: [{agent, session_id, machine, last_at, active}]}` |
| `GET /log/repos/{id}/export` | the project, every turn with `final_text`, its decisions and `watch_items`, as a JSON download |
| `PATCH /log/repos/{id}` `{name?, hidden?, icon_reset?}` | `{"ok": true}`. `name` "" goes back to the repo name; `icon_reset` lets the relay's logo replace an uploaded one. |
| `POST /log/repos/{id}/icon` | multipart field `icon` (PNG, JPEG or WebP, under 1 MB); kept until `icon_reset` |
| `GET /log/turns/{id}` | `{turn: Turn + {final_text, diffs: {path: text}}, attachments: [{id, dir, kind, name, url, media_type, count, image?, has_data, src}], project, decisions, watch, job: {status, model, tokens, cost, finished_at, error}, session: {index, count, first_at}, prev, next, resume, local: {log, diffs, images} \| null, local_error}` |
| `GET /log/turns/{id}/images/{n}` | the image bytes (raster only), from the relay |
| `POST /log/turns/{id}/resummarize` | `{"ok": true}`: the recording Mac writes the summary again. 409/504 when it is offline or too old. |
| `POST /log/watch/{id}/done\|open` | `{"ok": true}` |
| `POST /log/decisions/{id}/dismiss\|restore\|supersede` `{by?}` | `{"ok": true}`; `supersede` needs `by`, the id of the newer decision |
| `POST /log/jobs/retry-failed` | `{retried, offline}`: failed summaries queued again on each online Mac |
| `GET /log/search?q=` | `[{id, project_id, agent, started_at, headline, prompt, verdict, snippet, project}]`; `snippet` marks hits as `[[word]]`. Words match as prefixes. |

**Project**: `id`, `key` (the normalised git remote, else the path), `name` (the shown name), `repo_name`, `root`,
`remote`, `hidden`, `has_icon`, `icon_locked`, `icon_url` (public, or null: draw `initial`), `initial`.

**Turn**: `id`, `agent` (claude, codex), `session_id`, `project_id`, `machine`, `machine_id`, `cwd`, `branch`, `head`,
`started_at`, `ended_at`, `duration_ms`, `tool_ms`, `prompt`, `model`, `tokens` `{input, output, cache_read, cache_write}`,
`model_calls`, `cost` (USD at API list prices, or null for an unknown model), `tools` `{name: count}`,
`files` `[{path, add, del}]`, `headline`, `summary` [sentences], `verdict` (clean, watch, broken, or null),
`steps` `[{at, title, text, tone}]`, `summary_status` (none, queued, running, done, failed, skipped), `summary_error`, `att_in`, `att_out`.

**Decision**: `id`, `turn_id`, `text`, `why`, `alternative`, `status` (active, dismissed, superseded), `superseded_by`, `created_at`, `agent`.
**Watch**: `id`, `turn_id`, `text`, `impact` (high, medium, low), `status` (open, done), `done_at`, `created_at`, `agent`.

## Live agents (`/api/v1/agents`)

Every agent on your online Macs, and what a blocked one asks. Only the account's owner can answer; each answer is in the audit log.

| Call | Answer |
| --- | --- |
| `GET /agents` | `{available, error, agents: [Agent]}` |
| `POST /agents/{machine_id}/{pane_id}/keys` `{keys}` | `{"ok": true}`. 1 to 10 keys: a digit, esc, enter, up, down, left, right, tab, space, y, n. |
| `POST /agents/{machine_id}/{pane_id}/prompt` `{text}` | `{"ok": true}`. 1 to 20000 characters. The relay cancels an open AskUserQuestion first, so send the answers as the prompt. 409 while the agent works. |
| `GET /agents/{machine_id}/{pane_id}/ask-files/{n}` | the image of `ask_extras.attachments[n]`, from the relay |

**Agent**: `machine_id`, `machine`, `pane_id`, `agent`, `status` (blocked, done, idle, working, unknown), `title`, `cwd`,
`workspace`, `session_id`, `since` (when it entered this status), `project` (the Substrate project of the pane, or null),
`repo` (the work-log Project of its folder, or null), `ask` ([{question, header, multiSelect, options: [{label, description, preview}]}] or null),
`ask_extras` ({context, attachments: [{id, kind, name, url, media_type, src}]} or null), `screen` ({text, options: [{label, keys, selected}]}
when blocked at another dialog, or null), `last` ({turn_id, headline, text, question, ended_at}: its last turn's reply, or null).
