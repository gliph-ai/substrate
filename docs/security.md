# Security

Substrate connects the people you invite to the coding agents on your computer. Each layer below is designed for a failure of the layer before it.

## Trust boundaries

| Boundary | Traffic | Protection |
|---|---|---|
| Tester's assistant → cloud | MCP tool calls | OAuth 2.1 (PKCE S256, rotating refresh) or a personal token; a project membership check on each call; schema-only tool input |
| Cloud → your computer | approved messages | the relay dials out (no inbound port); machine tokens rotate each hour; a second use of a refresh token revokes the machine |
| Relay → agent | one envelope for each message | Herdr's own submit path; only the bound pane and agent session; no submit into a blocked, busy or unknown agent |
| Agent → tester | replies, questions, deploy stages | a reply token scoped to one message; secret scan; link allowlist; agent text marked as data |

## Identities

Substrate has six identities:

- accounts (email)
- machines (paired relays)
- MCP clients (OAuth registrations)
- projects (bound to a machine, a Herdr pane and an agent session id)
- reply tokens (one for each delivered message)
- session links (one for each person a producer gives access)

A session link (`/mcp/<secret>`) is a credential in the URL. Anyone with the
link has its access, so each link is limited:

- It reaches only the projects its producer picked.
- Its permissions narrow the tools and the status it gets: no replies, no
  deploy status or no files when those are off.
- It expires (7 to 365 days), and revoking it stops it at once.
- Messages wait for approval unless the producer turned that off for the link.

Credentials and sessions:

- Substrate stores each credential as a SHA-256 digest.
- Browser sessions use HttpOnly, Secure, SameSite=Lax cookies.
- Each form has a CSRF secret for its session. A POST from another origin fails.

## Prompt containment

A tester's message is data inside one tool segment: one project, one conversation, one message. Each layer works alone.

1. **Schema-only input.** MCP tools accept only their declared fields. A tool call cannot set a command, path, pane, mode, policy or recipient.
2. **Sanitizer** (`proto::sanitize`). It removes terminal escape sequences, bracketed-paste markers, C0 and C1 control characters, bidi overrides and zero-width characters. Text is NFC, with a length limit. A message cannot press keys in the pane.
3. **One submission.** The relay submits through `herdr agent prompt`. Herdr pastes the text and Enter as one bracketed write. An agent at a permission prompt gets no input.
4. **The envelope** (`proto::envelope`).
   - A fixed frame marks the block as an untrusted request from an external tester.
   - The frame limits the work to the project directory. It forbids secrets, destructive commands, permission changes, production pushes and outside contact without the developer's approval of that action.
   - The tester's text is between `<<<UNTRUSTED <nonce>` and `UNTRUSTED <nonce>>>>`, with a random nonce for each message.
   - Substrate stops a message that copies the frame.
   - The developer sees the exact envelope before approval.
5. **Risk flags** (`server/src/safety.rs`). Substrate holds these requests for manual approval, also in auto relay: secrets, instruction overrides, destructive commands, `curl | sh`, exfiltration, permission bypasses and production deploys. These rules slow down an attacker. Layers 1 to 4 are the boundary.
6. **Attachments.**
   - Substrate accepts only PNG, JPEG, WebP, PDF, plain text and Markdown, by content. HTML and SVG fail.
   - Substrate stores each file by id and gives it a safe name.
   - The relay compares the size and the SHA-256, and writes the file with mode 0600 into the project's workspace. It does not follow symlinks.
   - Nothing runs an attachment.
7. **The reply token.** `substrate reply|progress|ask|deploy` work only for the message that came with the token, on the machine bound to its project. The token gives no access to other projects or conversations, and no access to bindings, approval modes or retest URLs.
8. **Agent attachments.** The CLI sends only files from inside the bound project directory, after it resolves symlinks, 10 MB maximum.
9. **Secret scan.** Substrate scans each outbound reply, question and progress note for cloud keys, private keys, tokens, `.env` assignments, connection strings and high-entropy strings. A match holds the reply for the developer. Substrate does not redact text.
10. **Link allowlist.** Agent text for testers shows links only to Substrate and the project's retest hosts. Other URLs lose their scheme. Testers get raw CI links only when the developer marks CI as public.
11. **Agent text is data.** A tester's assistant gets agent text only in `agent_text` fields. The MCP server instructions forbid the assistant to follow that text or to call other tools because of it. They also forbid it to ask for secrets for the agent.

The audit log (`/audit`) records each block, hold, release, approval, mode change, binding and sign-in.

## Auto relay and agent permissions

Auto relay removes the approval step for each message. It does not change the agent's own permissions. Substrate passes no flags to the agent and cannot answer a permission prompt, because Herdr does not submit into one.

- Auto relay is set for each project, and optionally for each trusted tester.
- It has a time limit: 1 h, 8 h, or until you turn it off.
- It needs explicit confirmation, and the audit log records it.
- Only the project's owner can change it: on the web, with the CLI on a paired machine, or with `/substrate mode` in Gliph.

## Delivery integrity

- A project delivers only to its bound pane and agent session. When the pane closes or has a different session, the project becomes `needs_rebind`. Messages stay in the queue until the developer binds the project again.
- Each project has one message in flight. The relay writes a durable record before it types.
- A crash after "submitting" and before the confirmation makes the message `uncertain`. Substrate reports it and does not retry it.
- The relay finds repeated deliveries of a message by its id, and types each message one time only.

## Vulnerability reports

Email security@gliph.sh.
