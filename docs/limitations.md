# Limitations and roadmap

## Known limitations

- **Tester chats get no push.** Claude and ChatGPT do not let a server start a message in a user's conversation. The tester's assistant gets replies when it calls `get_message` or `wait_for_update` (25 s maximum for each call). The tester can also open the conversation link.
- **ChatGPT plans.** Full MCP apps need developer mode. ChatGPT offers it on Business, Enterprise and Edu, and on some Plus and Pro accounts. ChatGPT asks the user to confirm each write call.
- **Screenshots.** Some clients and models do not pass image bytes to a tool. The upload link works on all clients.
- **Completion comes from the agent.** The final answer arrives when the agent runs `substrate reply --final`. When the agent goes idle without a final reply, the tester gets that fact and the developer gets a terminal excerpt. Substrate does not show terminal output to a tester as the agent's answer.
- **Exactly-once delivery.** Typing into a terminal has no transaction. A relay crash after the relay writes its record and before Herdr confirms the paste makes the message "uncertain". The developer decides to send it again or to cancel it.
- **Deploy detection.** The relay tracks GitHub Actions through `gh`. For other pipelines (GitLab, Pushleft), the agent reports each stage with `substrate deploy status`. When the app serves its commit, `--verify-url --expect-sha` detects the deploy.
- **macOS first.** The relay service uses launchd and the Keychain. On Linux, use `daemon run`. The credentials go to a 0600 file, and there is no service file yet.
- **No malware scan.** Substrate accepts only inert attachment types, by a check of the content. It does not run an antivirus scan.
- **Risk flags are heuristics.** They match only the common phrasings. The real protection is the envelope, the scopes and the agent's own permissions.
- **Encrypted Gliph channels.** Substrate gets `@<project>` messages only from plaintext events. In a `+e` channel, use `/substrate send`.

## Roadmap

- Linux and Windows services for the relay.
- GitLab and Pushleft pipeline tracking in the relay.
- An email to the tester when an agent asks a question or a fix is ready to retest.
- MCP resource subscriptions, for clients with server notifications.
- Rate limits for each tester, set by the developer.
- Signed relay releases and self-update.
