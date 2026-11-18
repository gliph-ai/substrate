# Onboarding

## Developers

1. Sign in at https://substrate.gliph.sh with an email link, or with your Gliph account.
2. Install the relay on the Mac where your agents run, and pair it: see [cli.md](cli.md).
3. Bind a project to its Herdr pane: `substrate project add <handle> --pane <id>`.
4. On the project page, add a description and the hosts testers may retest on (for example `verna.app`).
5. Create an invitation link and send it to your tester. It is shown once.
6. Approve messages on the project page, with `substrate approve <id>`, or with `/substrate approve <id>` in a linked Gliph channel.

### In Gliph

Sign in to Substrate with Gliph once, then in any channel:

| command | does |
|---|---|
| `/substrate link <project>` | Show the project's conversations in this channel. |
| `@<project> <text>` | Send a message to the project's agent. A channel member's message waits for your approval unless auto relay is on. |
| `/substrate approve <id>` / `reject <id>` | Decide on a message. |
| `/substrate send <project> <text>` | Send a message as yourself. |
| `/substrate status [project]` | Agent, relay and queue state. |
| `/substrate mode <project> approve\|auto [1h\|8h]` | Change the relay mode. |
| `/substrate unlink` | Stop showing the project here. |

Agent plugins in a linked channel can message a project the same way, with `@<project>`, so one agent can hand work to another.

## Testers

Open the invitation link and accept it with your email or Gliph account. Then add Substrate to your assistant once.

The address is `https://substrate.gliph.sh/mcp`.

### Claude (claude.ai and the desktop app)

1. Open Settings, then Connectors, then Add custom connector.
2. Paste the address and save.
3. Press Connect and sign in to Substrate. Allow access.

Custom connectors work on every Claude plan; the Free plan allows one. Screenshots you paste into the chat reach Substrate when Claude passes them to the tool; if it doesn't, ask it for an upload link.

### ChatGPT

1. Open Settings, then Apps, then Advanced settings, and turn on Developer mode.
2. Create an app with the address and OAuth sign-in.
3. Sign in to Substrate and allow access.

Developer mode is offered on Business, Enterprise and Edu (an admin turns it on for the workspace); some Plus and Pro accounts have it too. ChatGPT asks you to confirm before each tool call that sends a message. If it cannot attach a screenshot, ask it for an upload link.

### Claude Code and other MCP clients

```sh
claude mcp add --transport http substrate https://substrate.gliph.sh/mcp
```

Clients that cannot sign in with OAuth use a personal token from https://substrate.gliph.sh/connect, sent as `Authorization: Bearer <token>`.

### Using it

Say what you found as you would to a person: "The settings page doesn't save my dark mode. Here's a screenshot. Can you tell the Verna developer?" Later, ask "Any news on the settings bug?" Your assistant checks and tells you when the fix is live. Your assistant cannot be woken by Substrate, so ask it for news, or open the conversation link it gives you.
