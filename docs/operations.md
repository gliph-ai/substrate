# Operations

## Production

| item | value |
|---|---|
| URL | https://substrate.gliph.sh (TLS 1.3 only) |
| Platform | Pushleft, org `empire`, project `substrate`, role `web` (one instance, port 8080) |
| Manifest | `.pl.yaml` at the repo root |
| Database | Pushleft PostgreSQL (`data.postgres`, database `substrate`); migrations run at boot (`server/migrations`) |
| Files | bucket `files`: `attachments/<id>`, relay releases under `downloads/substrate/` |
| DNS | Porkbun CNAME `substrate.gliph.sh` → `substrate--205eb939.apps.pushleft.dev` |
| Secrets | `SUBSTRATE_SECRET`, `GLIPH_PLUGIN_TOKEN`, `SES_KEY_ID`, `SES_SECRET`, `S3_*`, `AWS_*` (Pushleft secrets; never in the repo) |
| Mail | Amazon SES eu-west-1, as `noreply@gliph.sh` (the shared account) |
| Gliph | plugin `substrate` (kind agent) on gliph.sh; gliph runs with `-substrate-callback https://substrate.gliph.sh/auth/gliph/callback` |

Keep one web instance: the relay hub and the long-poll wakeups live in process memory.

## Deploy

```sh
pl --org empire up --production      # from the repo root
curl -s https://substrate.gliph.sh/healthz
```

`/healthz` answers 200 with the version and the number of connected relays when the database answers.

Relays reconnect by themselves after a deploy (jittered backoff, 1 s to 60 s). Messages are durable in Postgres; anything dispatched but unacknowledged is re-sent with the relay deduplicating by message id.

## Rollback

Deploy the last good commit again:

```sh
pl --org empire list                                  # recent deployments
git worktree add /tmp/substrate-rb <good-sha>
pl --org empire up --production --path /tmp/substrate-rb
git worktree remove /tmp/substrate-rb
```

Migrations are additive; a rollback across a migration keeps the new columns, which the older binary ignores.

## Relay release

```sh
cli/release.sh        # builds arm64 + x86_64, publishes to files:downloads/substrate/{<version>,latest}/
```

## Backups

Pushleft backs up the managed Postgres; `pl --org empire db --help` restores into a new generation (docs/managed-databases.md in Pushleft). Attachments are in the bucket; deleting a project deletes its rows, and attachment objects can be swept by prefix.

## Logs

```sh
pl --org empire logs --production --role web -f
```

JSON lines. Useful fields: `msg`, `error`, `machine`. The relay logs to `~/Library/Application Support/substrate/logs/daemon.log` on each developer's Mac.

## Runbook

| symptom | check | fix |
|---|---|---|
| Testers' messages stay "approved" | project page: relay offline or `needs_rebind` | developer runs `substrate doctor`; `substrate project bind` after a pane change |
| A message is "uncertain" | the relay restarted mid-submit | developer reads the agent's pane, then Deliver again or Cancel |
| Magic links don't arrive | logs for `ses` errors | SES key in Pushleft secrets; SES sending quota |
| `/substrate` missing in Gliph | logs for `gliph plugin` | `GLIPH_PLUGIN_TOKEN` valid on gliph.sh; restart the web role |
| Sign in with Gliph fails | gliph.sh logs for device-link | gliph's `-substrate-callback` matches `PUBLIC_URL/auth/gliph/callback` |
| A machine was signed out on its own | audit: `machine.revoked`, "refresh token reused" | a copied credential was used; re-pair with `substrate login` |
