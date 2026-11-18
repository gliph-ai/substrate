# Substrate

Give people a direct line to the agents building your software.

A tester tells their own AI assistant (Claude, ChatGPT, any MCP client) what they found. Substrate holds the message until you approve it. Your relay hands it to the Claude Code (or Codex, Gemini, Cursor) session you already have open in [Herdr]. The agent's progress, questions, CI and deploy state go back the same way. The tester's assistant says when the fix is ready to retest.

Live at **https://substrate.gliph.sh**. Part of [Gliph](https://gliph.sh): sign in with your Gliph account, and run `/substrate` in a Gliph channel to use it as an agent channel there.

[Herdr]: https://herdr.dev

## Repository layout

| path | what it is |
|---|---|
| `proto/` | The wire contract both ends compile: relay frames, stages, the sanitizer and the prompt envelope. |
| `server/` | The cloud: axum + PostgreSQL. Website, magic-link and Gliph sign-in, MCP + OAuth 2.1, approvals, the relay hub, the Gliph plugin. |
| `cli/` | `substrate`, the Rust relay and CLI: device pairing, the launchd daemon, the Herdr adapter, deploy watching, agent reports. |
| `docs/` | Architecture, security, CLI, onboarding, operations, limitations. |

## Develop

```sh
createdb substrate_dev
SUBSTRATE_DEV=1 PUBLIC_URL=http://localhost:8790 PORT=8790 DATABASE_URL=postgres://localhost/substrate_dev cargo run -p substrate-server
# magic links are logged to stdout in dev
cargo test            # proto, server unit + end-to-end (needs local Postgres), relay with a fake Herdr
cargo clippy --all-targets
```

The end-to-end suite creates a fresh database per test (`SUBSTRATE_TEST_PG`, default `postgres://localhost/postgres`). The relay tests never touch a real Herdr pane.

## Ship

- Server: `pl --org empire up --production` from the repo root (Pushleft project `substrate`, `.pl.yaml`). See `docs/operations.md`.
- Relay: `cli/release.sh` builds both Mac architectures and publishes them to the bucket behind `/install`.

## Docs

- [Architecture](docs/architecture.md): the system, the protocol, message and deploy lifecycles.
- [Security](docs/security.md): the trust boundaries and the prompt-containment layers.
- [CLI](docs/cli.md): install, pair, bind, run the relay; the agent-side commands.
- [Onboarding](docs/onboarding.md): developers, and testers on Claude, ChatGPT and other clients.
- [Operations](docs/operations.md): deploy, rollback, backups, logs, runbook.
- [Limitations and roadmap](docs/limitations.md).
