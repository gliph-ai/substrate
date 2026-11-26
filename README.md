# Substrate

Give your testers a direct line to the agents that build your software.

A tester tells their own AI assistant (Claude, ChatGPT or any MCP client) about a bug. The message waits in Substrate until you approve it. Your relay then sends it to the Claude Code, Codex, Gemini or Cursor session that you have open in [Herdr]. The agent's progress, questions, CI state and deploy state go back to the tester's assistant. The tester learns when the fix is ready to retest.

Substrate also has a work log. Hooks in Claude Code and Codex record each finished turn. The relay writes the summary on your Mac with your own agent CLI. The log shows the steps, the decisions, the open items, the tokens and the cost of each turn, per git repo. Diffs, images and the tool log stay on your Mac. When an agent stops with a question, you answer it on the web or in Sidra.

Substrate is live at **https://substrate.gliph.sh** and is part of [Gliph](https://gliph.sh). You sign in with your Gliph account or an email link. In a Gliph channel, `/substrate` makes the channel an agent channel.

[Herdr]: https://herdr.dev

## Use it

You need a Mac with [Herdr] and at least one agent CLI (`claude`, `codex`, `gemini` or `cursor-agent`). The work log summaries use `claude` or `codex`. Deploy tracking uses `gh`.

1. Sign in at https://substrate.gliph.sh.
2. Install the relay. The installer puts the binary in `~/.local/bin` and does not use root. Add that folder to your `PATH`.
   ```sh
   curl -fsSL https://substrate.gliph.sh/install | sh
   export PATH="$HOME/.local/bin:$PATH"   # also in ~/.zshrc
   ```
3. Pair this Mac. Confirm the code in the browser window that opens.
   ```sh
   substrate login
   ```
4. Install the relay as a LaunchAgent (`sh.gliph.substrate`). It starts at login and restarts after a crash.
   ```sh
   substrate daemon install
   ```
5. Turn on the work log. Add `backfill --summarize` to import the turns already on this Mac.
   ```sh
   substrate log install
   substrate log backfill --summarize --since 7d
   ```
6. Check the setup.
   ```sh
   substrate doctor
   ```

Each agent pane in Herdr becomes a project. Invite testers from the project page. [Onboarding](docs/onboarding.md) has the tester setup for Claude, ChatGPT and other clients.

### The relay service

| Command | Result |
|---|---|
| `substrate status` | Shows the account, the connection, the LaunchAgent and the projects. |
| `substrate daemon restart` | Restarts the relay. |
| `substrate daemon stop` | Stops the relay. Messages stay in the Substrate queue. |
| `substrate daemon run` | Runs the relay in the foreground, as launchd does. |
| `substrate logs -f` | Follows the relay log (`~/Library/Application Support/substrate/logs/daemon.log`). |
| `substrate log status` | Shows the work log hooks, the upload queue and the summaries that wait or failed. |
| `substrate logout` | Signs this Mac out and stops the relay. |

The [CLI reference](docs/cli.md) has every command and the relay's files.

## Develop

### Prerequisites

- Rust (stable, edition 2021) with `clippy`
- PostgreSQL on `localhost`, with `createdb` on the `PATH`
- Herdr, to run the relay against real panes (the tests use a fake)

### Run the server

```sh
bin/dev
```

`bin/dev` creates the database `substrate_dev`, runs the migrations at boot and serves https://substrate.test (Caddy to port 8790). Magic links go to the server log. Files go to `./data`.

Without Caddy, use the plain port:

```sh
PUBLIC_URL=http://localhost:8790 bin/dev
```

`herdr-substrate` opens the dev workspace: a server pane, a shell and two Claude panes.

### Run the relay from source

Point a relay at your local server. The server address is stored in `config.json`, so set it back with `--server https://substrate.gliph.sh` when you are done.

```sh
cargo build -p substrate
target/debug/substrate login --server http://localhost:8790
target/debug/substrate daemon run          # foreground; Ctrl-C stops it
```

`daemon install` registers the binary that runs it. Do not install a debug build as the LaunchAgent on a Mac that also runs the production relay.

### Test

```sh
cargo test                       # proto, server unit and end-to-end tests, relay with a fake Herdr
cargo clippy --all-targets -- -D warnings
```

Each end-to-end test creates a new database on `SUBSTRATE_TEST_PG` (default `postgres://localhost/postgres`). The relay tests use a fake Herdr and touch no real pane.

### Server configuration

| Variable | Use |
|---|---|
| `DATABASE_URL` | PostgreSQL. Required. |
| `PUBLIC_URL` | The address that users open. An `https://` address makes the cookies Secure. |
| `PORT` | The listen port (8080 in production, 8790 in dev). |
| `SUBSTRATE_SECRET` | 64 hex characters. Signs the sessions and tokens. Required unless `SUBSTRATE_DEV=1`. |
| `SUBSTRATE_DEV` | `1` writes magic links to the log and allows an empty secret. |
| `SUBSTRATE_FILES` | The local file folder when no bucket is set (default `./data`). |
| `S3_ENDPOINT`, `S3_BUCKET`, `AWS_REGION`, `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY` | The attachment and release bucket. |
| `SES_KEY_ID`, `SES_SECRET`, `SES_REGION`, `MAIL_FROM` | Magic-link mail through Amazon SES. |
| `GLIPH_URL`, `GLIPH_PLUGIN_TOKEN` | Gliph sign-in and the `/substrate` channel plugin. |
| `APNS_KEY_PATH` (default `/secrets/apns.p8`, where Pushleft mounts the `APNS_KEY_P8` secret), `APNS_KEY_ID`, `APNS_TEAM_ID` | Approval push notifications to Sidra. Without them, the server sends no push. |
| `PUSHLEFT_ADMIN_JWKS`, `PUSHLEFT_ADMIN_AUDIENCE` | The Pushleft Admin page. Pushleft sets them. Without them, `/_pushleft/admin` returns 404. |

## Ship

- **Server:** push to `main`. GitHub Actions runs clippy and the tests, then `pl up --production` (Pushleft org `empire`, project `substrate`, `.pl.yaml`). Migrations run when the server starts.
- **Relay:** run `cli/release.sh`. It builds both Mac architectures and puts them in the bucket behind `/install`. To update a relay, run the installer again, then `substrate daemon restart`.

[Operations](docs/operations.md) has the manual deploy, rollback, backups, logs and the runbook.

## Repository layout

| Path | Contents |
|---|---|
| `proto/` | The wire contract for the cloud and the relay: relay frames, stages, the sanitizer and the prompt envelope. |
| `server/` | The cloud, on axum and PostgreSQL: the website, magic-link and Gliph sign-in, MCP and OAuth 2.1, approvals, the work log, live agents, the relay hub, the Gliph plugin and the Pushleft Admin page. |
| `cli/` | `substrate`, the relay and CLI in Rust: device pairing, the launchd service, the Herdr adapter, deploy tracking, agent reports, and the work log hooks and summaries. |
| `bin/dev` | The local server. |
| `docs/` | Architecture, security, the CLI, onboarding, operations and limitations. |

## Docs

- [Architecture](docs/architecture.md): the system, the protocol, and the message and deploy lifecycles.
- [Security](docs/security.md): the trust boundaries and the prompt containment layers.
- [CLI](docs/cli.md): install, pairing, bindings, the relay and the agent commands.
- [Onboarding](docs/onboarding.md): setup for developers, and for testers on Claude, ChatGPT and other clients.
- [Operations](docs/operations.md): deploy, rollback, backups, logs and the runbook.
- [App API](docs/app-api.md): the API for Sidra.
- [Limitations and roadmap](docs/limitations.md).
