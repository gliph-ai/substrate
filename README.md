# Substrate

Give your testers a direct line to the agents that build your software.

A tester tells their own AI assistant (Claude, ChatGPT or any MCP client) about a bug. The message waits in Substrate until you approve it. Your relay then sends it to the Claude Code, Codex, Gemini or Cursor session that you have open in [Herdr]. The agent's progress, questions, CI state and deploy state go back to the tester's assistant. The tester learns when the fix is ready to retest.

Substrate is live at **https://substrate.gliph.sh** and is part of [Gliph](https://gliph.sh). You sign in with your Gliph account. In a Gliph channel, `/substrate` makes the channel an agent channel.

[Herdr]: https://herdr.dev

## Repository layout

| Path | Contents |
|---|---|
| `proto/` | The wire contract for the cloud and the relay: relay frames, stages, the sanitizer and the prompt envelope. |
| `server/` | The cloud, on axum and PostgreSQL: the website, magic-link and Gliph sign-in, MCP and OAuth 2.1, approvals, the relay hub and the Gliph plugin. |
| `cli/` | `substrate`, the relay and CLI in Rust: device pairing, the launchd daemon, the Herdr adapter, deploy tracking and agent reports. |
| `docs/` | Architecture, security, the CLI, onboarding, operations and limitations. |

## Develop

```sh
bin/dev               # https://substrate.test (Caddy -> :8790); magic links go to the log
herdr-substrate       # the dev workspace: server pane, shell, two Claude panes
cargo test            # proto, server unit and end-to-end tests (local Postgres), relay with a fake Herdr
cargo clippy --all-targets
```

Each end-to-end test creates a new database (`SUBSTRATE_TEST_PG`, default `postgres://localhost/postgres`). The relay tests use a fake Herdr and touch no real pane.

## Ship

- **Server:** push to `main` on https://git.erip.me/gliph/substrate. GitLab CI runs the tests, then `pl up --production` (Pushleft project `substrate`, `.pl.yaml`). Refer to `docs/operations.md`.
- **Relay:** run `cli/release.sh`. It builds both Mac architectures and puts them in the bucket behind `/install`.

## Docs

- [Architecture](docs/architecture.md): the system, the protocol, and the message and deploy lifecycles.
- [Security](docs/security.md): the trust boundaries and the prompt containment layers.
- [CLI](docs/cli.md): install, pairing, bindings, the relay and the agent commands.
- [Onboarding](docs/onboarding.md): setup for developers, and for testers on Claude, ChatGPT and other clients.
- [Operations](docs/operations.md): deploy, rollback, backups, logs and the runbook.
- [Limitations and roadmap](docs/limitations.md).
