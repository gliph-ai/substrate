# Limitations and roadmap

## Known limitations

- **No push into the tester's chat.** Claude and ChatGPT do not let a server start a message in a user's conversation. The tester's assistant learns about replies when it calls `get_message` or `wait_for_update` (up to 25 s per call), or when the tester opens the conversation link.
- **ChatGPT plans.** Developer mode, which full MCP apps need, is offered on Business, Enterprise and Edu; some Plus and Pro accounts have it. ChatGPT confirms each write call with the user.
- **Screenshots.** Whether an assistant passes image bytes to a tool varies by client and model. The upload link always works.
- **Completion is reported, not inferred.** The agent's final answer arrives when it runs `substrate reply --final`. If the agent goes idle without one, the tester is told so, and the developer sees a terminal excerpt. Substrate never presents terminal output as the agent's answer.
- **Exactly-once delivery.** Typing into a terminal has no transaction. A relay crash between writing its record and Herdr confirming the paste leaves the message "uncertain"; the developer decides whether to send it again.
- **Deploy detection.** GitHub Actions is watched through `gh`. Other pipelines (GitLab, Pushleft) are reported by the agent with `substrate deploy status`, or detected through `--verify-url --expect-sha` when the app serves its commit.
- **macOS first.** The relay runs on macOS with launchd and the Keychain. On Linux it runs (`daemon run`, credentials in a 0600 file) without a service file yet.
- **Malware scanning.** Attachments are limited to inert types by content sniffing; there is no antivirus scan.
- **Risk flags are heuristics.** They catch the common phrasings only. The envelope, the scopes and the agent's own permissions are the protection.
- **Gliph channels with end-to-end encryption.** `@<project>` messages are read from plaintext events only. In a `+e` channel, use `/substrate send`.

## Roadmap

- Linux and Windows service integration for the relay.
- GitLab and Pushleft pipeline watching in the relay.
- Email notice to testers when an agent asks a question or a fix is ready to retest.
- MCP resource subscriptions, for clients that support server notifications.
- Per-tester rate limits configurable by the developer.
- Signed relay releases and in-place self-update.
