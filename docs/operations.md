# Operations

## Production

| Item | Value |
|---|---|
| URL | https://substrate.gliph.sh (TLS 1.3 only) |
| Platform | Pushleft, org `empire`, project `substrate`, role `web` (one instance, port 8080) |
| Manifest | `.pl.yaml` at the repo root |
| Database | Pushleft PostgreSQL (`data.postgres`, database `substrate`). Migrations (`server/migrations`) run at boot. |
| Files | bucket `files`: `attachments/<id>`, and relay releases in `downloads/substrate/` |
| DNS | Porkbun CNAME `substrate.gliph.sh` → `substrate--205eb939.apps.pushleft.dev` |
| Secrets | `SUBSTRATE_SECRET`, `GLIPH_PLUGIN_TOKEN`, `SES_KEY_ID`, `SES_SECRET`, `S3_*`, `AWS_*` (in Pushleft secrets) |
| Mail | Amazon SES eu-west-1, from `noreply@gliph.sh` (the shared account) |
| Gliph | plugin `substrate` (kind agent) on gliph.sh. Gliph runs with `-substrate-callback https://substrate.gliph.sh/auth/gliph/callback`. |

Run one web instance only. The relay hub and the long-poll wakeups are in process memory.

## Deploy

Push to `main` on https://github.com/gliph-ai/substrate. `.github/workflows/ci.yml` runs clippy and the tests, then deploys with the repository secret `PUSHLEFT_TOKEN`. CI deploys only the commit at the tip of `main`.

To deploy by hand:

```sh
pl --org empire up --production      # from the repo root
curl -s https://substrate.gliph.sh/healthz
```

When the database is available, `/healthz` returns 200 with the version and the number of connected relays.

After a deploy, the relays reconnect with a jittered backoff from 1 s to 60 s. Messages are durable in Postgres. The cloud sends each dispatched message without an ack again, and the relay discards duplicates by message id.

## Rollback

Deploy the last good commit again:

```sh
pl --org empire list                                  # recent deployments
git worktree add /tmp/substrate-rb <good-sha>
pl --org empire up --production --path /tmp/substrate-rb
git worktree remove /tmp/substrate-rb
```

All migrations only add. After a rollback across a migration, the new columns stay, and the older binary ignores them.

## Relay release

```sh
cli/release.sh        # builds arm64 and x86_64, publishes to files:downloads/substrate/{<version>,latest}/
```

## Backups

Pushleft backs up the managed Postgres. To restore into a new generation, use `pl --org empire db` (refer to docs/managed-databases.md in Pushleft). Attachments are in the bucket. When you delete a project, Substrate deletes its rows. You can delete its attachment objects by prefix.

## Logs

```sh
pl --org empire logs --production --role web -f
```

The log has JSON lines. Useful fields: `msg`, `error`, `machine`. On each developer's Mac, the relay log is `~/Library/Application Support/substrate/logs/daemon.log`.

## Runbook

| Symptom | Check | Fix |
|---|---|---|
| Testers' messages stay "approved" | project page: relay offline or `needs_rebind` | The developer runs `substrate doctor`, or uses Move here on the project page after a pane change. |
| A message is "uncertain" | the relay restarted during a submit | The developer reads the agent's pane, then clicks Deliver again or Cancel. |
| Magic links do not arrive | logs for `ses` errors | Check the SES key in Pushleft secrets and the SES sending quota. |
| `/substrate` is missing in Gliph | logs for `gliph plugin` | Check that `GLIPH_PLUGIN_TOKEN` is valid on gliph.sh. Restart the web role. |
| Sign in with Gliph fails | gliph.sh logs for device-link | Make `-substrate-callback` on Gliph the same as `PUBLIC_URL/auth/gliph/callback`. |
| A machine was signed out with no action from its owner | audit: `machine.revoked`, "refresh token reused" | Someone used a copied credential. Pair again with `substrate login`. |
