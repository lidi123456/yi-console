# AGENTS.md

See [README.md](README.md) for an overview and usage. This file has the project map and the rules that break things when ignored; details per area are in `docs/internals/` (read the relevant one before changing that area).

## Map

- `crates/proto`: the single source of truth for the protocol. To add a method, first add a constant in `methods`, then extend the dispatch in `crates/yi-agentd/src/server.rs`; the frontend types in `app/src/types.ts` are kept in sync by hand.
- `crates/yi-agentd/src/hub.rs`: the session state machine. Every event goes through `emit()` to be numbered, persisted and broadcast; adapters must not write to the database directly.
- `crates/yi-agentd/src/adapters/`: one file per tool, all implementing `AdapterCmd` input and `Emitter` output. The ACP tools (Cursor / Kiro / Qoder / CodeBuddy) share `acp.rs`. Adding a tool: [docs/adding-a-tool.en.md](docs/adding-a-tool.en.md).
- `crates/client/src/api.rs`: `ClientHub::call` is the complete set of commands the frontend can call. Tauri (`app/src-tauri/src/lib.rs`) and yi-web are thin wrappers around it.
- `crates/link`, `crates/relay`: relay protocol and server.
- `app/src/store.ts`: frontend state and the event loop; components live in `app/src/components/`.

## Details by area

| Area | Doc |
| --- | --- |
| Sessions: turns, queueing, chat/terminal switch, `/clear` `/compact`, usage limits, questions, task lists, hand-off, attachments | [docs/internals/sessions.md](docs/internals/sessions.md) |
| Tool detection, probing, permission modes, usage stats, per-tool quirks | [docs/internals/tools.md](docs/internals/tools.md) |
| File panel: tree, git changes and diffs, editor, search/replace, HTML preview | [docs/internals/file-panel.md](docs/internals/file-panel.md) |
| Terminal mode, tmux, bottom terminal panel | [docs/internals/terminal.md](docs/internals/terminal.md) |
| Frontend UI: chat timeline, theme, fonts, scrollbars, sidebar, links, overlays, i18n | [docs/internals/frontend-ui.md](docs/internals/frontend-ui.md) |
| Remote: SSH bridge, relay, forwarding, port forwarding | [docs/internals/remote.md](docs/internals/remote.md) |
| Daemon lifecycle, versions, auto-upgrade | [docs/internals/daemon.md](docs/internals/daemon.md) |
| App self-update, Android | [docs/internals/release.md](docs/internals/release.md) |

## Rules

Protocol and backend
- Event fields are camelCase (`EventBody` uses `rename_all_fields = "camelCase"`). `assistant_delta` / `thinking_delta` are transient (seq 0, not persisted); the final `assistant_message` / `thinking` is authoritative.
- Permission modes per tool: `permission_modes` in `hub.rs` and `permModes` in `NewSession.tsx` are synced by hand. Unknown modes reported by a tool are shown by raw name, never dropped.
- Wherever a process would start, check `ToolKind::has_process()` (Files sessions have none).
- Every git command runs with `GIT_OPTIONAL_LOCKS=0`; git paths are repo-relative, absolute paths and `..` are rejected.
- yi-agentd never listens on TCP; remote access is only via the SSH bridge or the relay. Local-only methods (`fs.open`, `proxy.setHosts`) are gated by peer in `serve_conn`.
- Relay handshake or message format changes must be made on all three sides (client, relay, yi-agentd), and `v` in the links bumped when incompatible.
- Host configs sent to the frontend go through `redact()`; a save that comes back with `********` keeps the original value.
- `DAEMON_VERSION` includes the commit hash; don't change it back to just the package version.
- The local daemon always runs from `~/.yi-console/bin/yi-agentd`; never run or autostart the sidecar from the install directory.
- The daemon's error messages are in Chinese (translated for the English UI by `app/src/i18n.ts`).

Frontend
- The backend handles requests on one connection concurrently: input that must stay ordered (e.g. terminal keystrokes) is queued in the frontend.
- No `prompt` / `confirm` / browser context menu: use `askText`, `askConfirm`, `openMenu` from `components/Overlay.tsx`. Session actions live in `components/SessionActions.ts`.
- Anything new that covers the page (dialog, menu, panel) registers with `useBack(active, close)` so Android back closes it.
- Links: clicks are handled by `guardLink` in `main.tsx`; nothing may navigate inside the app window.
- Dark-theme CSS variables are written twice (the `prefers-color-scheme` block and `:root[data-theme='dark']`); in JS use `isAppDark()`.
- Text in chat / editor areas uses the font-size variables (`--fs-chat`, `--fs-editor`, …), not fixed px. No bundled web fonts.
- Sidebar styles are changed only in the "Sidebar" section at the end of `styles.css`. Host-related entries go into the host row's menu, not new buttons.
- Panel toggles (terminal, file panel) are per session; don't make them global.
- UI text is bilingual via i18next: add every new key to both `app/src/locales/zh/` and `en/`, one key per whole sentence with placeholders (no concatenation), and include `useLang()`'s lang in `useMemo` deps for translated text. `npm run i18n:check` enforces keys and placeholders. Details in [frontend-ui.md](docs/internals/frontend-ui.md).

Writing
- Code comments are in English.
- Docs are bilingual: `X.md` is Chinese and `X.en.md` is English, each linking to the other at the top; update both in the same commit. Exceptions, English only: AGENTS.md, CLAUDE.md and `docs/internals/`.
- When a change adds or alters behaviour worth knowing, update the matching `docs/internals/` file. Keep this file to the map and rules; put details that only matter next to one piece of code in a code comment there.

## Testing

```bash
cargo test
cargo build && (cd app && npm run build)
./target/debug/yi-web --listen 127.0.0.1:3001 --dist app/dist --token dev --yi-agentd-dir ./target/debug
```

For end-to-end tests, set `YI_CONSOLE_HOME=<temp dir>` so yi-agentd runs in an isolated environment and `~/.yi-console` is left alone.

Tauri and Android are only built in CI (`.github/workflows/build.yml`). Don't force a local build on machines that lack the webkit dependencies or enough memory.
