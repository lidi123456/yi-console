# Personal long-term memory (`yi_memory`)

Host-scoped personal long-term memory shared across all 7 structured AI tools (Claude Code, Codex, Cursor, Kiro, Qoder, CodeBuddy, Antigravity) using progressive disclosure. Kept outside git repositories under `<YI_CONSOLE_HOME>/memory/`.

## Storage layout (`crates/yi-agentd/src/memory/store.rs`)

- `<base>/memory/global/<id>.md`: host-wide personal environment notes and preferences.
- `<base>/memory/projects/<slug>-<hash8>/project.json` and `<id>.md`: per-project personal notes (not committed to git).
- `<base>/memory/.history/<undo_id>.json`: pre-write / pre-delete / pre-move snapshots (last 100 kept) backing 1-click undo (`memory.undo`).
- `<base>/memory/pending.json`: AI-proposed writes / deletes awaiting user confirmation when `writePolicy == "confirm"`.
- Files are Markdown with YAML frontmatter (`title`, `description`, `mode: on-demand | always`, `tags`, `updatedAt`); plain `.md` files dropped manually without frontmatter fall back cleanly (first `# ` heading as title, first body line as description, `mode: on-demand`).
- Project resolution runs `git rev-parse --path-format=absolute --git-common-dir` (with `GIT_OPTIONAL_LOCKS=0`, falling back to relative `--git-common-dir` on older git), so a main repo, subdirectories, git worktrees, and Bot task worktrees resolve to the same project key. Non-git directories fall back to canonicalized `cwd` (excluding generic dirs such as `~`, `/tmp`, and `<base>/bots/`). Existing `project.json` manifests (and their `aliases`) are checked first so `memory.rebindProject` keeps a moved repo's memories attached.
- Search (`memory_search`) is pure Rust with CJK bigrams + ASCII tokens + exact substring weighting (`id`/`title` > `tags` > `description` > `content`) and searches only Global + current Project.

## Progressive disclosure & launch lifecycle

- **L0 snapshot (`StartSpec.memory_prompt` / `StartSpec.memory_notice`)**:
  - Computed once per process launch (`Memories::build_l0_prompt`) and kept separate from `SessionInfo.append_system_prompt`.
  - Contains short `always` guardrails (capped at 350 chars each, 1800 chars total) followed by the `on-demand` index (`id: description`, capped at 25 global + 35 project / 3500 chars total, project after global with project overriding global on conflict).
  - **Prompt cache preservation**: memory contents never enter `session_launch_fingerprint`, so saving or editing a memory never restarts running or idle tool processes. Only toggling `DaemonSettings.memory.enabled` changes the fingerprint (`-memory` when disabled) and marks busy sessions `restart_pending`.
  - Delivery per provider:
    - Claude (`--append-system-prompt`) and Codex (`developerInstructions`) receive it via `adapters::system_text(spec)` (survives native compaction).
    - ACP tools (Cursor, Kiro, Qoder, CodeBuddy, Antigravity) receive it wrapped in `<system-reminder>` on the first non-`/` prompt of a fresh session (`State.memory_notice`), and re-arm a fresh snapshot after Cursor compaction (`cursor_compact_done`), Kiro compaction (`compaction_status` completed), or `session/load` fallback to `session/new`.
- **L1 & L2 (`yi_memory` MCP gateway, `crates/yi-agentd/src/memory/mod.rs`)**:
  - Built-in stdio proxy `yi-agentd memory-mcp --session <sid> --ticket-file <f>` relaying over local IPC `memory.mcp` (rejected for remote peers in `serve_conn`; bound to the proxy + tool PID via `proxy_access`, matching `yi_browser` and `client-mcp`).
  - Tools: read-only `memory_list`, `memory_read` (single `id` or batch `items`), `memory_search`; write tools `memory_save`, `memory_delete` (omitted from `tools/list` when `writePolicy == "readonly"` or on Bot sessions).
  - Preauthorized across all 7 structured providers (Claude `--allowedTools` + `can_use_tool`, Kiro `--trust-tools`, ACP `gateway_tool` exact forms including CodeBuddy `DeferExecuteTool`) while the gateway uniformly enforces:
    - `plan` mode, Bot sessions (`info.bot.is_some()`), and `writePolicy == "readonly"` refuse writes/deletes.
    - AI writes run `detect_secret` to reject private keys, API keys, tokens, JWTs, and hardcoded password assignments (while allowing placeholders and certificate fingerprints).
    - `writePolicy == "direct"` (default): writes immediately with `.history/` backup and emits `EventBody::Memory` (`action: "saved" | "deleted"`, with `undoId`) on the session timeline.
    - `writePolicy == "confirm"`: queues into `pending.json` and emits `EventBody::Memory` (`action: "pending_save" | "pending_delete"`, with `pendingId`) without activating until confirmed.
    - `serve_conn` also blocks local child processes of `yi-agentd` (`from_tool(peer_pid)`) from calling mutating `memory.*` admin RPCs directly, preventing AI shell commands from bypassing the MCP gateway.

## Frontend & RPC

- Protocol methods: `memory.list`, `memory.get`, `memory.save`, `memory.delete`, `memory.move`, `memory.undo`, `memory.resolvePending`, `memory.preview`, `memory.scanImport`, `memory.import`, `memory.rebindProject`, plus local-only `memory.mcp` and broadcast notification `memory.changed`.
- Host setting `DaemonSettings.memory`: `{ enabled: true, writePolicy: "direct" | "confirm" | "readonly" }` (preserved when omitted by older clients in `settings.set`).
- UI:
  - `MemoryView.tsx`: host-level manager (Global + Projects sidebar, pending queue, search, editor modal, L0 prompt preview, move/split, rebind moved repo, and read-only importer scanning `.kiro/steering`, `.agents/rules`, `GEMINI.md`, `CLAUDE.local.md`, `.cursor/rules`, `.codebuddy/rules`, `~/.claude/projects/*/memory`, `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`, `~/.gemini/GEMINI.md`, `~/.codebuddy/CODEBUDDY.md`), plus `SessionMemoryDrawer` opened from structured sessions.
  - `ChatView.tsx`: renders `EventBody::Memory` timeline cards with Edit / Undo (direct) or Confirm / Edit & Confirm / Dismiss (confirm), and gives `yi_memory` tool calls localized labels.
  - Entry points: host row menu (`Sidebar.tsx`), Command Palette (`Palette.tsx`), host settings `MemoryCard` (`SettingsView.tsx`), and structured session "More" menu (`SessionView.tsx`, `SessionActions.ts`).
