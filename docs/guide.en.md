# User guide

The [README](../README.en.md) covers installation and a quick start; this page explains each feature in detail. For remote hosts and the relay, see [Remote access and relay](remote.en.md).

中文：[使用手册](guide.md)

## The background service on each machine

Sessions on every machine are hosted by a background service (the `yi-agentd` daemon). You only install the desktop app; the daemon doesn't need a separate install. The package ships with yi-agentd, and before each connection to this machine the app syncs it to `~/.yi-console/bin/yi-agentd`; the daemon and the startup service both run from there. That way, upgrading the app never fails because the daemon is holding files in the install directory.

Once the app starts the daemon, it runs on its own: closing the app doesn't stop your sessions. After an app upgrade, the old daemon keeps running and "Hosts" shows a version mismatch; click "Restart daemon" to switch to the new one. Restarting interrupts any turn in progress, but the sessions themselves are kept, and the next message picks up the context automatically.

To have it run at boot (without opening the app first), click "Register startup service" in "Hosts", or run `~/.yi-console/bin/yi-agentd install`:

- Linux: a systemd user service, with linger enabled; falls back to a crontab `@reboot` entry when systemd isn't available.
- macOS: a LaunchAgent.
- Windows: an HKCU Run registry entry.

## Setting up the tools

Install and log in to a tool on whichever machine you want to use it on:

| Tool | Requirement |
|---|---|
| Claude Code | `claude` logged in |
| Codex | `codex` logged in; needs a version that supports `app-server` |
| Cursor | `cursor-agent` logged in (run `cursor-agent login`) |
| Kiro | `kiro-cli` logged in |
| Qoder | `qodercli` logged in (run `qodercli login`). The install script puts it in `~/.qoder/bin/qodercli/`; it's found even when that isn't on PATH |
| CodeBuddy | `codebuddy` (npm package `@tencent-ai/codebuddy-code`) logged in: run `codebuddy` once and type `/login` |

yi-agentd reads the PATH of your login shell and checks the usual install directories. If a tool still isn't found, enter its full path in the host settings, or set the `YI_CONSOLE_PATH` environment variable to add search directories.

What each tool supports in chat mode:

| Feature | Claude Code | Codex | Cursor | Kiro | Qoder | CodeBuddy |
|---|---|---|---|---|---|---|
| Switch model mid-session | ✓ | ✓ (from the next turn) | ✓ | ✓ | ✓ | ✓ |
| Thinking level | ✓ (immediately) | ✓ (from the next turn) | — | — | — | — |
| Image input (≤ 5 MB each, png/jpeg/gif/webp) | ✓ | ✓ | ✓ | if the tool declares support | ✓ | ✓ |
| Context usage | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Permission modes | all | all | Default / Accept Edits / Plan / Bypass All | same as Cursor | no Plan | same as Cursor |
| Account quota (Usage Stats page) | 5-hour / weekly windows | 5-hour / weekly windows | included, Auto, API usage and on-demand (same as cursor-agent's `/usage`) | plan usage | plan credits, add-ons and dedicated packs | plan credits and bonus credits |
| Token source for usage stats | the tool's own logs (direct terminal use counts too) | the tool's own logs (direct terminal use counts too) | Yi Console sessions | Yi Console sessions | the tool's own logs (counted in credits) | the tool's own logs (tokens + credits) |
| Import history sessions | ✓ | ✓ | ✓ | ✓ | ✓ (viewable right away) | ✓ (viewable after continuing) |

Model lists and quotas come from yi-agentd briefly starting the tool and asking it (in `~/.yi-console/probe/`, so no session is created). Model lists are cached in `models.json`; quotas are cached for 60 seconds. Cursor usage is fetched from Cursor's backend with the token cursor-agent saved at login (`~/.config/cursor/auth.json`; `%APPDATA%\Cursor` on Windows, `~/.cursor` on macOS). If the token has expired, run cursor-agent once.

## The input box

- **Queued messages**: you can keep typing while the AI is replying. Press Enter and the message joins a queue shown above the input box; queued messages are sent in order when the current turn ends. "Send Now" interrupts the current turn and sends that message right away; × (or ↑ in an empty input box) pulls it back into the input box for editing. After a manual interrupt (Esc or the stop button), the queue isn't sent automatically. The queue lives in yi-agentd, so your phone and computer see the same one; it's cleared when the daemon restarts.
- **`/` commands**: type `/` at the start of the input box to list:
  - prompt snippets, maintained under "Host Settings → Prompt snippets" and stored by that machine's yi-agentd;
  - built-in Yi Console actions: switch model, switch permission mode, view changes, export, rename, new session in the same directory, and so on;
  - the tool's own commands, such as Claude Code's `/compact` and custom commands, or the commands Cursor / Kiro report over ACP.
- **`@` files**: type `@` plus part of a file name to fuzzy-search the session directory (in git repos this uses `git ls-files` and respects `.gitignore`); pick one to insert its relative path.
- **Model and thinking level**: switch at the bottom right of the input box (takes effect from the next message), or type `/model`. The bottom of that menu has the thinking level (Claude Code and Codex; the levels offered are whatever the model reports). You can also pick both when creating a session.
- **Referencing files**: right-click "Copy" in the file panel (or click a file in the tree and press Ctrl+C), then paste into the input box to insert `@relative/path`, the same as dragging it in. Files copied in your OS (Ctrl+C in Explorer/Finder) can be pasted or dragged into the input box too: images are sent as images, and other files are uploaded to this session's upload directory on the host (`~/.yi-console/uploads/<session id>/files/`, up to 50 MB each, deleted with the session) and inserted as `@/absolute/path`. This works for SSH and relay hosts as well.
- **Images**: paste or drag them into the input box; double-click a thumbnail to enlarge it. Click an image in the conversation to enlarge it; scroll to zoom, drag to pan, rotate if needed, and press Esc to close.
- **Permission mode**: switch at any time at the bottom left of the input box between "Default", "Accept Edits", "Plan" and "Bypass All", or cycle with Shift+Tab. The setting is saved on the session and kept when you resume it. Switching Claude Code to "Bypass All" restarts the tool process; if it's mid-reply, the restart waits for the turn to end. Claude doesn't allow Bypass All when running as root.

## Keyboard shortcuts

| Shortcut | Action |
|---|---|
| Ctrl+K (⌘K on macOS) | Command palette: jump to any session, open host settings, and more |
| Ctrl+Shift+F | Search messages across all sessions |
| Ctrl+N / Alt+N | New session (in a browser Ctrl+N belongs to the browser, so use Alt+N) |
| Alt+↑ / Alt+↓ | Previous / next session |
| Esc | Interrupt the current turn (when the input box has focus) |
| Shift+Tab | Cycle permission mode (when the input box has focus) |
| Alt+← / Alt+→, mouse side buttons | Back / forward |
| Ctrl+B | Collapse / expand the sidebar |
| F2 | Rename the current session (or the one selected in the sidebar) |

## Window and context menus

- Choose the theme under "Preferences → Appearance & Language" at the bottom left: System, Light or Dark. Each device remembers its own choice.
- Drag the sidebar's right edge to resize it (the default 244 px is the narrowest; up to 480 px), double-click the edge to reset. Each device remembers its own width.
- The desktop app has no system title bar; the top row matches the sidebar. On the left are collapse sidebar, back and forward; on the right, minimize / maximize / close (macOS keeps its traffic lights). Drag any empty area to move the window; double-click to maximize.
- Context menus: right-click a session to open, rename, pin, view changes, export, copy path, stop, or delete it. In the input box, right-click for cut / copy / paste / select all; on selected text, right-click to copy.
- You can select and copy text while the AI is still writing. The reply pauses updating while text is selected and catches up when you release or clear the selection.
- Sending a message scrolls to the bottom. When you've scrolled far up, a "Jump to bottom" button appears at the bottom right (with a blue dot while a reply is in progress).

## Terminal sessions

- The look follows Windows Terminal: Cascadia Mono (falling back to Microsoft YaHei / PingFang for CJK), One Half Dark / One Half Light colors, GPU rendering, and double-width CJK alignment. Pick follow-UI / dark / light and the font size under "Preferences → Terminal Appearance"; you can also switch from the right end of the status bar under the terminal.
- Right-click for copy, paste, select all and clear. Ctrl+C copies when there's a selection, Ctrl+V pastes; Ctrl+Shift+C / V work too.
- **tmux persistence**: on Linux / macOS with tmux 3.2 or later, terminal sessions run inside tmux (on a separate socket, `tmux -L yi-console`, so your own tmux is untouched). Programs keep running across daemon restarts and upgrades, and yi-agentd reattaches when it starts. tmux shows no status bar and doesn't capture the mouse; scrolling and selection are still handled by Yi Console's terminal. You can turn this off under "Host Settings → Session processes". Deleting a session or "Stop Process" also ends the program inside tmux.
- **Switch between chat and terminal**: the "Terminal Mode" / "Chat Mode" button at the top right of a session (also in the session's context menu) switches the same session between the two views. The tool continues the same conversation, so no context is lost. Switching to the terminal resumes it in the tool's own interactive UI; switching back to chat imports what you said in the terminal into the chat history. Switching ends the current tool process and interrupts any turn in progress.
- **Idle release**: a chat-mode session idle for more than 30 minutes (change or disable this under "Host Settings → Session processes") has its tool process ended to save memory. The chat history stays, and the next message resumes the same conversation automatically.

## Files, search and session management

- **Workspace files**: the folder button at the top right of a session opens the file panel on the right. With no file open it's just the file tree; open a file and it widens, with tabs for opened files across the top (scroll horizontally with the wheel when they don't fit; the current file always scrolls into view, and the drop-down on the right lists every open file; right-click a tab to close others, to the left, or to the right). The button at the top right of the panel makes it full screen, leaving a narrow strip of the conversation on the left; click it to exit. Open files are remembered per working directory and restored, along with the file you were viewing, the next time you open the panel (or restart the app). The file tree marks git changes with colors and letters (M modified, U new, D deleted). Git status refreshes when a turn ends, when the window regains focus, and every 10 seconds, so commits made in a terminal show up promptly. Switch to "Changes" to see only changed files; once a file is open, toggle between file and changes to see its diff.
  - **Finding things**: "Search" in the side bar searches file contents (case-sensitive and whole-word options; git repos respect .gitignore); click a result to jump to that line. Ctrl+P quick-opens a file by name; type `@` to list the current file's functions, classes and headings (Ctrl+Shift+O), or `:` to jump to a line (Ctrl+G).
  - **Drag and drop**: drag a file from the tree or a tab into the input box to insert `@relative/path`. Drag files from your OS into the file tree to upload them to that folder (up to 50 MB each; works for SSH and relay hosts too).
  - **Download / open with the OS**: right-click "Download" to save a file to this computer. On the local host you also get "Open with default app" and "Show in file manager".
  - **Side by side and compare**: right-click "Open to the side" to put another file on the right, read-only, for reference; "Compare with <current file>" shows the differences between two files. Diffs can be shown side by side or inline.
- **Files sessions**: pick "Files" as the tool when creating a session; you only choose the host and directory, and nothing is started. The session page is just the file panel above (with exactly the same features) and has no input box; the terminal button at the top right opens terminals at the bottom. Open tabs are remembered per session, and you can create several Files sessions in the same directory, each with its own tabs.
  - **Images**: the bottom toolbar has zoom out / in, fit-to-window / actual size, rotate left / right, flip horizontally, and full screen. Scroll to zoom around the cursor, drag to pan, double-click to zoom in or reset; pinch on phones. Keys: `+` `-` zoom, `0` fit to window, `1` actual size, `R` / `Shift+R` rotate.
  - **Previews**: PDF (using the system WebView's built-in viewer); CSV / TSV as a table you can filter.
  - **Browser tabs**: the globe button at the right of the tab bar (or "Open web page…" in the "New" menu) opens a browser tab. Enter an address or just a port number; the new-tab page lists the ports listening on the host, so one click opens them. On a remote host, `localhost` means that machine, and port forwarding is set up automatically before opening. For HTML files, right-click "Open in browser tab", or open the file and switch to "Preview": relative css, js and images are read from the host, and the page reloads when you save. Pages are shown in an iframe, so sites that refuse to be embedded (many external sites) appear blank; use "Open in system browser" for those. The address bar doesn't follow links you click inside the page.
  - **Markdown**: opens in preview by default; switch to "Source" or "Split" (edit on the left, live preview on the right, with scrolling kept in sync). Your last choice is remembered. Relative image paths in the preview are read from the host, relative links open that file in the panel, and `#heading` links jump to the heading. The Ctrl+Shift+O outline works in the preview too.
- **Reply style**: the list button at the top right of a session switches between "Minimal" (thinking and tool calls folded into one group) and a "Claude Code"-style timeline. You can also change it under "Preferences → Chat Appearance".
- **Session usage**: the usage button at the top right of a session (showing this session's tokens and cost; also "View Usage" in the "More" menu) opens a dialog with this session's input, output and cache tokens, cost, context usage, and this tool account's quota (the same as the Usage Stats page). It can be refreshed.
- **Search**: the search box at the top of the sidebar filters sessions by title and directory as you type. Press Enter (or click "Search messages for …" below it) to search user messages and replies in the sessions of every connected host; click a result to jump straight to that message. Ctrl+Shift+F does the same.
- **Host menu**: the "…" on each host row in the sidebar (or its context menu) has Session History, Browse Files, Open Terminal, Usage Stats and Host Settings. "Add / Manage Hosts" at the bottom opens host management.
- **Session list**: sessions are grouped by directory. Click a directory name to collapse it (collapsed groups show a session count, and a dot lights up when a session is running or waiting for approval). Right-click a directory name to search its session history, start a session there, or delete all sessions in it. Pin a session with the pin icon on hover or from the context menu. Sessions that have exited and haven't been touched for more than 3 days move into "N older sessions".
- **View changes**: the "View Changes" button at the top right of the session page (also in the sidebar's session context menu, or `/changes` in the input box) opens the "Changes" view of the file panel. If tools in this session edited files, only those are listed by default (uncheck "Only this session" to see all); click one to see its diff. When the session directory isn't a git repo, only the edits recorded in tool calls are listed, each with its diff.
- **Export**: the session menu can copy the session as Markdown or download it as an `.md` file. It includes user messages, replies and a summary of tool calls, but not thinking or tool output.

## Session status line

Below a session's input box is a status line. Configure it under "Host Settings → Session status line"; it applies only to sessions on that host:

- **Built-in fields**: model, context, tokens, cost, last turn duration, git branch (with change count and ahead/behind), directory, status, tool and account quota. Check the ones you want and reorder them,
  or write a template such as `{model} · {branch}{dirty} · {context}`.
- **Custom command**: follows the same convention as Claude Code's `statusLine`. It runs in the session directory and receives JSON on stdin (`session_id`, `cwd`, `model`, `workspace`, `cost`, `context_window`,
  plus tool, usage and git information under `yi_console`). The first line of its output is shown, ANSI colors are supported, and it times out after 3 seconds. On Windows it runs under cmd.
- With no command set, Claude Code sessions use the `statusLine` configured in `~/.claude/settings.json` by default, run with Git Bash on Windows. A project's `.claude/settings*.json` is used only if you've already marked that directory (or a parent) as trusted in Claude Code, so opening someone else's repo never runs their commands.

## Continuing after a usage limit

When Claude Code or Codex stops in the chat UI because the account's usage limit is used up, Yi Console records when the limit resets and, one minute after that, sends "continue" automatically (in Chinese or English, following the UI language) so the task carries on. While it waits, a countdown above the input box offers "Continue now" and "Cancel", and the sidebar row shows when it will continue. Sending a message yourself, `/clear`, or switching to the terminal UI cancels it. It doesn't wait for limits that reset more than a day out, or after hitting the limit again right after continuing several times in a row. You can turn it off under "Host Settings → Session processes". The background service on the host keeps the timer, so it works with the app closed.

## Notifications

While the app is open, it shows a system notification when an AI reply finishes or is waiting for approval, including the start of the reply. Under "Preferences → Notifications" at the bottom left you can turn each kind on or off, and choose between notifying only "When away from session" (the default; a minimized window, a window behind others, or viewing another session all count) or always. You can also send a test notification to check system permissions. These are per-device settings, stored in `client-settings.json` in the client data directory.

Once the mobile app goes to the background its live connection drops, so no notifications arrive in the meantime; session content catches up when you bring it back.

## Port forwarding

Dev servers and APIs running on a remote host can be forwarded so you can open them in a browser on this computer. Find "Port Forwarding" in the "…" menu of a host row in the sidebar, in the session menu, or in the command palette.

- The bottom of the page lists the TCP ports listening on the host with their processes; click "Forward" or "Forward and open". You can also type a port number by hand.
- The local port defaults to the same number as the host port (for example 3000 → localhost:3000). If it's taken, another one is picked automatically; you can also choose your own.
- SSH hosts use SSH's own port forwarding (if sshd forbids TCP forwarding, it goes through yi-agentd instead). For relay hosts, and hosts reached through another computer, each connection opens a new relay channel and the host's yi-agentd connects to its own port. yi-agentd still listens on no network ports.
- Forwarding only reaches the host's own 127.0.0.1, and this computer also only listens on 127.0.0.1. Forwards end when you close the app.
- The local host needs no forwarding; the page just offers "Open".

## Where data lives

| What | Where |
|---|---|
| yi-agentd (session database, logs, socket, relay config) | `~/.yi-console/`; set `YI_CONSOLE_HOME` to move it |
| Images sent in sessions | `~/.yi-console/uploads/<session id>/`, deleted with the session |
| Client (host list, known_hosts) | the OS app-data directory; for yi-web, set with `--data` |
