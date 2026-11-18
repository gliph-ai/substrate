# Security

Substrate is a bridge from people you invite into the coding agents running on your computer. Every layer below assumes the layer before it can fail.

## Trust boundaries

| boundary | what crosses it | what holds it |
|---|---|---|
| Tester's assistant → cloud | MCP tool calls | OAuth 2.1 (PKCE S256, rotating refresh) or a personal token; per-project membership checked on every call; schema-only tool input |
| Cloud → your computer | approved messages | the relay dials out (no inbound port); machine tokens rotate hourly; a reused refresh token revokes the machine |
| Relay → agent | one envelope per message | Herdr's own submit path; only to the bound pane and agent session; never into a blocked, busy or unknown agent |
| Agent → tester | replies, questions, deploy stages | a reply token scoped to one message; secret scan; link allowlist; agent text labelled as data |

## Identities

Accounts (email), machines (paired relays), MCP clients (OAuth registrations), projects (bound to a machine, a Herdr pane and an agent session id), and reply tokens (one per delivered message). Every credential is stored as a SHA-256 digest. Browser sessions are HttpOnly, Secure, SameSite=Lax cookies with a per-session CSRF secret on every form; POSTs from another origin are refused.

## Prompt containment

A tester's message is data inside one tool segment: one project, one conversation, one message. Each layer works on its own.

1. **Schema-only input.** MCP tools take only their declared fields. No field names a command, path, pane, mode, policy or recipient.
2. **Sanitizer** (`proto::sanitize`). Terminal escape sequences, bracketed-paste markers, C0/C1 control characters, bidi overrides and zero-width characters are removed; text is NFC and length-capped. A message cannot press keys in the pane.
3. **One submission.** The relay submits through `herdr agent prompt`, which bracketed-pastes the text and Enter as one write and refuses an agent at a permission prompt.
4. **The envelope** (`proto::envelope`). A fixed frame says the block is an external tester's untrusted request, names the project directory as the limit, and forbids secrets, destructive commands, permission changes, production pushes and outside contact unless the developer approves that action. The tester's text sits between `<<<UNTRUSTED <nonce>` and `UNTRUSTED <nonce>>>>`, with a random nonce per message. A message that imitates the frame is refused. The developer sees the exact envelope before approving.
5. **Risk flags** (`server/src/safety.rs`). Requests touching secrets, instruction overrides, destructive commands, `curl | sh`, exfiltration, permission bypasses or production deploys are held for manual approval, even under auto relay. These rules slow an attacker down; layers 1 to 4 are the boundary.
6. **Attachments** are sniffed by content (PNG, JPEG, WebP, PDF, plain text and Markdown only; no HTML or SVG), stored by id, renamed to a safe basename, size- and SHA-256-checked by the relay, and written 0600 into the project's workspace without following symlinks. Nothing is executed.
7. **The reply token.** `substrate reply|progress|ask|deploy` work only for the message they were delivered with, on the machine bound to its project. An agent cannot post to another project or conversation, change bindings, approval modes or retest URLs.
8. **Agent attachments** come only from inside the bound project directory after symlinks resolve, up to 10 MB.
9. **Secret scan** on every outbound reply, question and progress note (cloud keys, private keys, tokens, `.env` assignments, connection strings, high-entropy strings). A hit holds the reply for the developer; nothing is silently redacted.
10. **Link allowlist.** Tester-facing agent text keeps links only to Substrate and the project's retest hosts; other URLs lose their scheme. Raw CI links reach testers only when the developer marks CI public.
11. **Data, not instructions.** Agent-written text reaches a tester's assistant only in `agent_text` fields. The MCP server instructions tell the assistant never to follow them, never to call other tools because of them, and never to ask for secrets on the agent's behalf.

Every block, hold, release, approval, mode change, binding and sign-in is an audit event (`/audit`).

## Auto relay is not elevation

Auto relay skips the per-message approval. It never changes the agent's own permissions: Substrate passes no flags to the agent and cannot answer a permission prompt (Herdr refuses to submit into one). Auto relay is per project, optionally per trusted tester, time-limited (1 h, 8 h, or until turned off), needs explicit confirmation, and is audited. Only the project's owner can change it, from the web, the CLI on a paired machine, or `/substrate mode` in Gliph.

## Delivery integrity

- A project delivers only to the pane and agent session it is bound to. A pane that closes, or that now hosts a different session, marks the project `needs_rebind`; messages stay queued until the developer rebinds explicitly.
- One message is in flight per project. The relay writes a durable record before typing. A crash between "submitting" and confirmation makes the message `uncertain`: it is reported, never retried automatically.
- Repeated deliveries of the same message are recognised by message id and never typed twice.

## Reporting a vulnerability

Email security@gliph.sh.
