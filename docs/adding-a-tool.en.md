# Adding a new AI coding tool

中文：[接入新的 AI 编码工具](adding-a-tool.md)

This document lists what it takes to integrate a new tool (say Gemini CLI or OpenCode) so that it works the same way as the existing Claude Code, Codex, Cursor, Kiro, Qoder and CodeBuddy integrations.

Each item says **what to do**, **where to change it** and **which tool to use as a reference**. Capabilities the tool itself lacks don't have to be forced, but the UI must reflect that: either don't offer the entry point, or show a clear message. Nothing should silently do nothing or fail with an incomprehensible error. Go through the acceptance checklist at the end item by item.

## 0. Choose the integration method

| What the tool offers | Approach | Reference |
|---|---|---|
| ACP (Agent Client Protocol, `xxx acp` / `xxx --acp`) | **Preferred.** Reuse `crates/yi-agentd/src/adapters/acp.rs` and add only the tool-specific parts | Kiro, Qoder, CodeBuddy, Cursor |
| Its own structured protocol (stream-json, a JSON-RPC app-server, etc.) | Write a new adapter under `adapters/` | `claude.rs` (stream-json), `codex.rs` (app-server) |
| Only an interactive UI | Terminal mode only, via `adapters/pty.rs` | `shell` |

A tool usually needs both modes: **chat mode** (structured; Yi Console draws the UI) and **terminal mode** (the tool's own UI running in tmux / a pseudo-terminal). Switching between the two must **continue the same tool session**.

## 1. Basic registration (required)

| What | Where |
|---|---|
| Tool enum: add a `ToolKind` variant and update `ALL` and `as_str()` | `crates/proto/src/lib.rs` |
| Frontend type and display name: the `ToolKind` union type and `TOOL_LABEL` | `app/src/types.ts` |
| Display name in daemon messages | the match in `crates/yi-agentd/src/hub.rs` that returns "Claude Code", "Kiro", etc. |
| Executable names (tried in order) and common install directories per platform (`%LOCALAPPDATA%` on Windows, the npm global directory, etc.) | `binary_names`, `resolve` and `detect` in `crates/yi-agentd/src/tools.rs` |
| Arguments for entering ACP (subcommand or flag) | `acp_arg` in `acp.rs` |
| Tool badge (the official logo, inlined as code, with source and license noted) | `ToolBadge` in `app/src/components/Icon.tsx` |
| Tool color `--c-<tool>` (check both light and dark) | top of `app/src/styles.css` |
| Tool list in system settings (path, extra arguments) | `app/src/components/SettingsView.tsx` |
| Tool lists on the history and usage pages | `HISTORY_TOOLS` in `HistoryView.tsx`; colors and `QUOTA_TOOLS` in `StatsView.tsx` |
| Display names correct in both the Chinese and English UI | add the strings to `app/src/locales/zh/*.json` and `en/*.json`, use them with `getT()` |

## 2. Chat mode (the adapter)

An adapter is a tokio task that owns the tool's child process: it receives `AdapterCmd` on `rx` and translates the tool's output into unified events for the `Emitter`. **Never write to the database directly**; every event is persisted and broadcast through the `Emitter`.

### 2.1 Commands to handle (`AdapterCmd`, defined in `adapters/mod.rs`)

| Command | Requirement |
|---|---|
| `Send(Prompt)` | Send one turn of user input. With images: inline them if the tool supports it (for ACP, check `promptCapabilities.image`); otherwise return a clear error |
| `Interrupt` | Interrupt the current turn (`session/cancel` in ACP); a `TurnEnd` must follow |
| `SetModel` | Switch model, effective next turn (`session/set_model` in ACP). On failure emit an `Error`, so the UI isn't stuck on "switching" |
| `SetPermissionMode` | Switch while running if possible (`session/set_mode` in ACP). If that's impossible, have the hub restart the process (see Claude switching to allow-all) |
| `SetEffort` | Thinking level. Only Claude and Codex support it today; if the new tool does, also lift the restriction in `Hub::set_effort` |
| `Permission` | Answer an approval or a question (see 2.4) |
| `StopTask` | Stop a background task (only if the tool has background tasks) |
| `Kill` | End the process; emit nothing afterwards |

### 2.2 Events and state to emit (`Emitter`, defined in `hub.rs`)

| Method / event | When |
|---|---|
| `external_id(id)` | As soon as the tool's session id is known (the result of `session/new` / `session/load` in ACP). Resuming, mode switching and history import all depend on it, so it's mandatory |
| `status(...)` | starting → idle / running / waiting_permission / error |
| `EventBody::UserMessage` | Emitted by the hub; adapters must not duplicate it. Only when replaying history does the adapter convert the user messages the tool replays into it |
| `AssistantDelta` / `ThinkingDelta` | Streaming text (transient events with seq 0, not persisted) |
| `AssistantText` / `Thinking` | The complete content when a block of text ends; this is authoritative |
| `ToolCall` / `ToolResult` | Tool calls and results. `name` is the tool's original name (ACP takes it from `_meta.<tool>.toolName`, see `meta_tool_name`), and `input` should keep the original arguments where possible; the frontend builds summaries from them |
| `PermissionRequest` / `PermissionResolved` | Approvals (see 2.4) |
| `Plan` | Task lists (see 2.6) |
| `TurnEnd` | **Must be emitted for every turn**, including failed and interrupted ones. Queued messages and permission-mode restarts are triggered by it (`after_turn`) |
| `Usage` / `context(%)` | Tokens, cost / credits, context ratio; report whatever is available (see section 6) |
| `models(list)` / `model(id)` | Available models and the current model |
| `commands(list)` | Slash command list (`available_commands_update` in ACP) |
| `permission_mode(mode)` | The tool changed its own permission mode (e.g. leaving plan after planning). Report unknown modes as is |
| `Compact` | Context compaction started, done, failed (see 2.7) |
| `Error` / `System` | Errors and system notices. Error messages must be understandable (see 2.8) |
| `title_if_empty` | Report it if the tool provides a session title |
| `exited(msg)` | The process exited; include the reason on abnormal exits |

### 2.3 Resuming and replaying history

- **When `StartSpec.resume` is set, continue the existing session** (`session/load` in ACP).
- **When `StartSpec.replay` is true, record the history the tool replays into the event log** (sessions imported from tool history); when false, drop everything replayed, or it would be duplicated.
- **`replay_skip`**: when switching from terminal back to chat mode, only record what comes after the first N user messages. See the `replay_users` counter in `acp.rs`.
- **When the tool can't find the session** (created and exited without anything said, so the tool never saved it), start a new one instead of getting stuck on an error. See `is_missing_session`.
- **When the session is held by another process**, say who holds it. See Kiro's `kiro_lock_holder`.
- **If the tool's session files can be read offline** (Claude, Codex), import them directly when the session is created instead of relying on replay. In that case, add the new tool everywhere `hub.rs` checks `offline`.

### 2.4 Approvals and questions

- Permission modes are normalized to `default / acceptEdits / auto / plan / bypass`; only expose the ones the tool really supports. **Two places must stay in sync**: `permission_modes(tool, mode)` in `hub.rs` and `permModes` in `NewSession.tsx`, each written for both modes.
- Approval requests become `PermissionRequest`, keeping the option ids the tool provides. When auto-approving, prefer "allow once" and avoid options that change the tool's own mode (Qoder's "allow for this session" switches it to acceptEdits).
- Questions from the tool to the user go the same way: `PermissionRequest` carries `questions`, with options fixed to `answer` / `skip`. See Claude's AskUserQuestion and Cursor's `cursor/ask_question`. Tools without a question mechanism that just ask in their reply text (Kiro) can be recognized in the frontend and given a clickable card (`ChatView.tsx`).

### 2.5 Sending while the AI is replying

`session.send` with `queue: true`:
- If the tool supports injecting messages mid-turn (Claude via stdin, Codex via `turn/steer`), hand it over directly and add the new tool to the `steer` check in `hub.rs`.
- If not, do nothing: the hub queues the message and sends it after the turn ends.

### 2.6 Task lists

If the tool has todos / plans:
- Standard forms (ACP `plan`, Codex `turn/plan/updated`) are converted into `EventBody::Plan` in the adapter.
- Lists maintained through tool calls (Kiro `todo_list`, Claude `TaskCreate` / `TaskUpdate`) are parsed in `PlanTracker` in `app/src/plan.ts`, and the tool names are added to `PLAN_TOOLS` (so those calls themselves aren't shown in the timeline).

### 2.7 Slash commands, /clear, /compact

- Report the tool's slash commands to the UI through `commands()`.
- `/clear` is handled by the hub (events cleared, `external_id` reset, a new session next time); adapters don't need to do anything.
- `/compact` is equivalent to sending a `/compact` message. If the tool doesn't understand `/` commands, translate it to the matching API in the adapter (see `start_turn` in `codex.rs`). If the tool reports compaction events, convert them to `EventBody::Compact`; if not, keep the user bubble and note it in `AGENTS.md`.

### 2.8 Error messages

- Not logged in: show the login command to run on that machine. See `login_hint` in `acp.rs` and add the new tool's command.
- Tools that don't report failures as JSON-RPC errors (CodeBuddy uses `stopReason: refusal` + `_meta`) need the error extracted explicitly. See `meta_error`.
- When the tool only says something like "Internal error", try to find the real cause (session held elsewhere, session not found, version too old).
- Daemon error messages are written in Chinese and translated for the English UI in `app/src/i18n.ts`.

### 2.9 Images in replies

Chat mode should tell the model "reference absolute paths on the host as Markdown images in replies and the client will display them":
- The content is `adapters::IMAGE_HINT`.
- If the tool has a system prompt entry point, pass it there: Claude uses `--append-system-prompt`, Codex uses `developerInstructions`.
- If it has none, skip it for now and note that in `AGENTS.md`.

## 3. Terminal mode

| What | Where / notes |
|---|---|
| Launch arguments for the interactive UI: resuming (`--resume` and the like), pinning a session id, model, flags for each permission mode | `interactive_args` in `adapters/pty.rs` |
| Whether a session id can be pinned on first launch (`--session-id` for Claude, Qoder, CodeBuddy). If so, add the tool to `history::pins_session_id` and implement `tool_session_exists` | `crates/yi-agentd/src/history.rs` |
| If the id can't be pinned, `Hub::guess_terminal_session` looks in the tool's history for the newest session in the same directory, used after this session was created and not claimed by another session | `hub.rs` (currently Codex, Cursor, Kiro) |
| If chat and terminal mode store sessions separately, copy them across on switch (Cursor's `cursor_handoff`) | `history.rs` |
| If the session stays held by the tool's background service after exit so chat mode can't continue it, add the matching flag (Codex's `--no-daemon`) | `tools.rs`, `spec_for` in `hub.rs` |
| The command copied by "Continue in terminal" | `app/src/components/SessionActions.ts` |
| Permission modes in terminal mode | same as 2.4, keep both places in sync |

## 4. History sessions

| What | Where |
|---|---|
| List the tool's history sessions (title, directory, time, id). Read files if possible, otherwise call the tool's command (Kiro's `--list-sessions`); slow ones must be able to run in parallel | `list` in `history.rs` |
| Read the full conversation (if readable offline), for import and viewing | `read` in `history.rs` (see `read_claude`, `read_codex`) |
| Path of the session log file, needed by "Continue with another tool" | `session_file` in `history.rs` |
| Notes on the log format, for the tool taking over | `formatHint` in `app/src/handoff.ts` |
| If an imported session fails to load the first time (held elsewhere, not logged in), it must still be able to replay on resume | the tool check in `Hub::history_missing` |

## 5. Models and quotas (`crates/yi-agentd/src/probe.rs`)

- **Model list**: start the tool once, ask, exit; the working directory is `<base>/probe`. ACP takes `models` from the `session/new` result (`parse_models`). If the empty session created while probing ends up in the tool's history, delete it afterwards (Qoder uses `session/delete`).
- **Account quota**: if the tool has a query API, hook it up (Kiro `/usage`, Qoder `get_usage_info`, CodeBuddy `getUserInfo`), return the unified format, and add the tool to `QUOTA_TOOLS` in `StatsView.tsx`.

## 6. Usage statistics (`crates/yi-agentd/src/stats.rs`)

- If the tool logs tokens / cost per request locally (Claude, Codex, Qoder, CodeBuddy), **read the logs**, cached by file mtime and size. That way direct terminal use counts too. For such a tool, take only what the logs lack from usage_log to avoid double counting.
- If there are no logs, use the `Usage` events the adapter reports (only sessions inside Yi Console are counted), and say so in the usage page's explanatory text (`StatsView.tsx`, in both Chinese and English).
- Context ratio: ACP's standard `usage_update`, or wherever the tool keeps it privately (Qoder's `context_usage_ratio`, Cursor's store.db).

## 7. Chat display compatibility (`app/src/components/ChatView.tsx`)

- `toolName()`: map the tool's original names to the unified ones (Bash / Read / Update / Write / Grep / WebFetch…) so the timeline looks the same.
- `toolKind()`: produce a result summary by type. File-editing tools must expose the path and the change.
- `parseOutput()`: unwrap the tool's result format (Cursor's JSON wrapped in another string, Kiro's `{"items":[{"Json"|"Text":…}]}`) and extract stdout, stderr and exit code of commands.
- Unknown tools show their raw name, with the summary falling back to "first 3 lines of output". This is a fallback, not a substitute for the three items above.

## 8. Documentation and tests

- `AGENTS.md`: add an entry on what's special about the tool (integration method, session file location, how it differs from the others) and which features it doesn't support.
- Unit tests: protocol parsing, error detection and history file parsing, using samples cut from real output (see the tests at the end of `acp.rs`).
- Do end-to-end tests in an isolated environment (`YI_CONSOLE_HOME=<temp dir>`); leave `~/.yi-console` alone.

## 9. Acceptance checklist

Chat mode:
- [ ] Create a session and send a message; text and thinking stream in, and the turn ends normally
- [ ] Tool calls show unified names and summaries in the timeline; file edits show a diff
- [ ] Sending images works, or says it isn't supported
- [ ] Interrupt stops the turn and the status returns to idle
- [ ] Sending while the AI is replying either injects or queues, and no message is lost
- [ ] Approvals behave correctly in each permission mode, the UI lists only supported modes, and switching modes while running works
- [ ] Questions (if the tool has them) can be answered and skipped
- [ ] Switching models works; the model list is available when creating a session
- [ ] Slash commands appear in the menu; `/clear` and `/compact` work
- [ ] Task lists show in the timeline and above the input box
- [ ] Usage and context ratio are shown (omitted when unavailable, never wrong)
- [ ] After the process ends, sending a message continues the original session; this also works after idle release
- [ ] Not logged in, session not found and session held elsewhere all produce understandable messages

Terminal mode:
- [ ] Opening, closing and reopening continues the same session
- [ ] Switching chat → terminal → chat loses and duplicates nothing
- [ ] Terminal sessions reattach after a daemon restart (tmux)

History and the rest:
- [ ] The history page can list, view and continue sessions (including resuming after a failed first load)
- [ ] "Continue with another tool" can hand this tool's log to other tools and take over theirs
- [ ] Usage and quota are shown correctly, and the explanatory text states what is counted
- [ ] Badge and colors look right in light and dark; both the Chinese and English UI are correct
- [ ] The executable is found on Windows, Linux and macOS
- [ ] `AGENTS.md` is updated
