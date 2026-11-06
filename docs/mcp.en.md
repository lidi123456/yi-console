# External MCP servers

[中文](mcp.md)

Configure external tools once under **MCP servers** in host settings for Claude Code, Codex, Cursor, Kiro, Qoder, and CodeBuddy chat sessions. Commands execute on the session host, independently of the client platform and the Windows-only Computer Use gateway. Terminal sessions retain each CLI's original configuration.

## Configuration

1. Open the target host's settings and add an MCP server.
2. Name it, choose stdio or HTTP, and select the AI tools that should receive it. All six are selected by default.
3. For stdio, enter a command, a JSON string array of arguments, and a JSON string map of environment variables. For HTTP, enter a URL and a JSON string map of headers. For example, `["C:/tools/server.py", "argument with spaces"]` preserves spaces without shell interpolation.
4. Save the editor and host settings. Idle sessions reload before the next message; busy sessions reload after their current reply. Disabling/removing a server follows the same rule and does not cancel an in-flight MCP call.

Environment and header values return as `********`. Saving those placeholders preserves existing values; removing a key removes its value. Secrets remain in the host's settings file without disk encryption. Server processes run with the daemon user's permissions; argument fields are not a sandbox.

## Provider integration

- Claude Code: process-scoped `--mcp-config`.
- Codex: process-scoped `-c mcp_servers.<name>=...`.
- Cursor, Kiro, Qoder, CodeBuddy: ACP `session/new` / `session/load` `mcpServers`.
- HTTP requires compatible CLI transport support. ACP agents must advertise HTTP capability during initialization or the adapter reports an error. Header configuration does not perform remote OAuth sign-in.

Global MCP files are not modified; existing CLI configuration remains in effect. Resolve name collisions by renaming the external server. `yi_computer_use` is reserved for the built-in desktop gateway. External MCP retains each provider's permission handling and does not inherit the desktop grant.

## Validation

`scripts/mcp/probe-server.py` is a harmless server that returns an environment-supplied random nonce and records initialization, discovery, and calls. `scripts/mcp/windows-providers-e2e.mjs` uses an isolated yi-web and real CLI processes to check six-provider integration, argument spaces, secret round-trips, and settings reload. Report account/authentication/quota failures separately from MCP connection failures.

Windows stdio acceptance results on 2026-11-05:

| Tool | Connect / discover | Actual model call |
| --- | --- | --- |
| Claude Code 2.1.296 | Passed | Passed |
| Codex 0.163.0 | Passed | Blocked by enterprise gateway HTTP 403 |
| Cursor 2026.10.22-27dcf5f | Passed | Existing Ark CLI hook returned invalid JSON and blocked execution |
| Kiro 2.37.1 | Passed | Passed, including busy and idle session reload |
| Qoder 1.1.80 | Passed | Passed |
| CodeBuddy 2.167.0 | Passed | Passed with normal DeferExecuteTool approval |

These checks establish MCP connectivity and unpredictable return values, not real desktop actions by all six models or actual HTTP validation on all six providers. The desktop model loop is separately evidenced by Kiro's native window input, button callback, and screenshot-only random code verification.
