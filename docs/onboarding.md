# Onboarding

## Developers

1. Sign in at https://substrate.gliph.sh with an email link or your Gliph account.
2. Install the relay on the Mac that runs your agents, and pair it. Refer to [cli.md](cli.md).
3. Bind a project to its Herdr pane: `substrate project add <handle> --pane <id>`.
4. On the project page, add a description and the retest hosts for testers (for example `verna.app`).
5. Create an invitation link and send it to your tester. Copy it at once: the page shows it one time.
6. Approve messages on the project page, with `substrate approve <id>`, or with `/substrate approve <id>` in a linked Gliph channel.

### Gliph channels

1. Sign in to Substrate with Gliph one time.
2. In the channel, an op adds Substrate with `/invite substrate`. `/kick substrate` removes it.
3. Use these commands in the channel:

| Command | Result |
|---|---|
| `/substrate link <project>` | Shows the project's conversations in this channel. |
| `@<project> <text>` | Sends a message to the project's agent. A member's message waits for your approval, except in auto relay. |
| `/substrate approve <id>` / `reject <id>` | Approves or rejects a message. |
| `/substrate send <project> <text>` | Sends a message in your own name. |
| `/substrate status [project]` | Shows the agent, relay and queue state. |
| `/substrate mode <project> approve\|auto [1h\|8h]` | Changes the relay mode. |
| `/substrate unlink` | Removes the project from this channel. |

An agent plugin in a linked channel can also write `@<project>`. One agent can send work to another this way.

## Testers

1. Open the invitation link.
2. Accept it with your email or your Gliph account.
3. Add Substrate to your assistant one time, with the address `https://substrate.gliph.sh/mcp`.

### Claude (claude.ai and the desktop app)

1. Open Settings, then Connectors, then Add custom connector.
2. Paste the address and save.
3. Click Connect, sign in to Substrate and allow access.

All Claude plans have custom connectors. The Free plan has one. When Claude passes a pasted screenshot to the tool, Substrate gets it. When it does not, ask Claude for an upload link.

### ChatGPT

1. Open Settings, then Apps, then Advanced settings. Turn on Developer mode.
2. Create an app with the address and OAuth sign-in.
3. Sign in to Substrate and allow access.

ChatGPT offers developer mode on Business, Enterprise and Edu, where an admin turns it on for the workspace. Some Plus and Pro accounts also have it. ChatGPT asks you to confirm each tool call that sends a message. When ChatGPT cannot attach a screenshot, ask it for an upload link.

### Claude Code and other MCP clients

```sh
claude mcp add --transport http substrate https://substrate.gliph.sh/mcp
```

For a client without OAuth sign-in, get a personal token from https://substrate.gliph.sh/connect. Send it as `Authorization: Bearer <token>`.

### Daily use

Tell your assistant about the bug in your own words: "The settings page doesn't save my dark mode. Here's a screenshot. Can you tell the Verna developer?" Later, ask: "Any news on the settings bug?" Your assistant gets the status and tells you when the fix is live. Substrate cannot start a message in your chat. Ask your assistant for news, or open the conversation link that it gives you.
