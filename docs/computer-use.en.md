# Computer use

[简体中文](computer-use.md)

Let a Claude Code, Codex, Cursor, Kiro, Qoder or CodeBuddy chat session operate the signed-in Windows desktop on its host (open apps, switch windows, read, click and type), with activity and checkpoint screenshots in the conversation.

## Support

The first release supports a **signed-in, unlocked Windows x64 desktop**. The entry is hidden for macOS, Linux, Windows ARM, and terminal sessions. Phones and other computers can connect as clients.

The target is the session host, not the device displaying the client. Services without an interactive desktop, SSH Session 0, locked desktops and secure system desktops report unavailable. Elevated windows, custom controls and application-specific automation depend on Windows and the driver.

The Windows daemon embeds a pinned MIT `cua-driver`. There is no runtime driver download or installation. First use extracts the verified executable and license into `bin/computer-use-<version>/` below the Yi Console data directory. Bundling does not change operating-system or application permissions.

## Use

1. Start Yi Console normally on the target Windows computer and open a Claude Code, Codex, Cursor, Kiro, Qoder or CodeBuddy chat session.
2. Click the computer icon (Computer use) right of @ in the composer toolbar.
3. With no reply in progress, enable computer use. The AI can then operate every app on this computer; it finds windows or launches apps itself, with no window to pick first. The tool connects to a session-scoped MCP entry and continues the existing tool session ID where possible.
4. Give a specific task, such as: “Open Notepad and type a line, don't save.” or “Send someone a message in Feishu.”
5. Review activity and screenshots in chat. Click an image to enlarge or save it.

Only one session controls a desktop at a time. Turn computer use off in that session before another session takes control. Access covers the whole desktop, as in OpenBot: enable it only for tasks you trust, and stop the AI's reply whenever you need to take over. The AI is told to make sure you asked for hard-to-undo actions such as sending messages, deleting or paying.

Kiro CLI currently cannot pass MCP images directly to its model. Yi Console supplies the captured image as a session artifact for Kiro’s built-in image reader. If file approval is requested, review and allow only that session screenshot.

## Stopping and turning off

As in OpenBot, there is only an on/off switch, with no separate pause or capture buttons.

- Stopping the AI's reply pauses desktop activity: queued requests are interrupted and control is released. Your next message resumes it, and the AI has to read the screen again.
- Clicking the computer icon and choosing Turn off computer use revokes the session's desktop authorization: a desktop request in progress is cancelled and later desktop calls are refused. The AI's reply is not interrupted; stop it yourself if you want. With your next message the tool restarts in the same session without the computer use tools. Ending a session, changing mode and `/clear` also revoke authorization.
- Actions already performed are not rolled back. Interrupted or timed-out actions may need a new screenshot or human verification and are not replayed automatically.
- Closing a client does not automatically end a background task. The daemon can continue while the Windows desktop remains available. Re-enable computer use after a daemon restart.

## Results

“Request returned” means the driver answered; it does not prove that the business task succeeded. The AI should inspect fresh state or a screenshot. Refusal, suspected no-op, timeout and interruption are reported separately.

Screenshots are session artifacts; events contain image references. Capture is on demand or at checkpoints, not a live video stream. Each session retains at most its latest 24 desktop screenshots, up to 80 MiB. Older activity remains, but its image may have been removed. This cache cleanup does not remove user-uploaded images. Existing session artifact cleanup applies when a session is deleted or cleared.

This feature controls operations through the Yi Console desktop gateway. It is not an operating-system sandbox for other processes running as the same user. Start with a test window to check application compatibility.

## Coverage compared with OpenBot

The comparison uses the [pinned OpenBot architecture](https://github.com/nightly-labs/openbot/blob/480dcbb30ba9aca8d5d52082b09c0f230d958d49/docs/architecture/computer-use.md) and cua-driver 0.34.0. OpenBot forwards the driver's 59 tools. Yi Console exposes the 25 that work on the desktop and apps.

| Workflow | Yi Console |
| --- | --- |
| Find windows, launch and switch apps, move windows | `list_apps`, `list_windows`, `launch_app`, `bring_to_front`, `set_window_frame` |
| Screenshot, accessibility tree, verification | `get_window_state`, `get_desktop_state`, `verify_state`, `zoom`, `get_accessibility_tree`, `get_screen_size`, `get_cursor_position` |
| Text, value, shortcuts, menus | `type_text`, `set_value`, `press_key`, `hotkey`, `invoke_menu` |
| Click, double/right click, drag, scroll | `click`, `double_click`, `right_click`, `drag`, `scroll`, `move_cursor` |
| Clipboard | `clipboard_read`, `clipboard_write` |
| Browser CDP tools (`browser_*`, `page`) | Not exposed; web tasks use the AI tool's own browser support |
| Kill process, recording/replay, driver sessions, config, install/update | Not exposed; managed by the application |

Cross-application workflows, native file dialogs and pop-ups work. Actual accessibility support must be tested per application.

OpenBot places its switch in Agent settings and does not ask for a window either. Yi Console turns it on per session, so the entry is in the composer toolbar; screenshot, pause, and stop controls appear only while enabled. OpenBot also draws a highlight around the window the AI works in; Yi Console does not yet, and names each action's window in its tool row in chat. External servers use the [generic MCP configuration](mcp.en.md) and share the same provider integration.
