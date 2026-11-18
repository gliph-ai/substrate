# substrate (relay and CLI)

See [docs/cli.md](../docs/cli.md). Relay endpoints it calls, all with the machine bearer:
`/relay/pair/start`, `/relay/pair/token`, `/relay/token`, `/relay/ws`, `/relay/report`,
`/relay/report/attachment`, `/relay/attachments/{id}`, `/relay/api/{whoami,logout,projects,
projects/{handle}/{bind,unbind,mode},approvals,inbox,messages/{id}/{approve,reject,retry,cancel}}`.
