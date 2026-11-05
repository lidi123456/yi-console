# Computer use

[简体中文](computer-use.md)

Let a Claude Code, Codex, Cursor, Kiro, Qoder or CodeBuddy chat session inspect and operate a real window on its host, with activity and checkpoint screenshots in the conversation.

## Support

The first release supports a **signed-in, unlocked Windows x64 desktop**. The entry is hidden for macOS, Linux, Windows ARM, and terminal sessions. Phones and other computers can connect as clients.

The target is the session host, not the device displaying the client. Services without an interactive desktop, SSH Session 0, locked desktops and secure system desktops report unavailable. Elevated windows, custom controls and application-specific automation depend on Windows and the driver.

The Windows daemon embeds a pinned MIT `cua-driver`. There is no runtime driver download or installation. First use extracts the verified executable and license into `bin/computer-use-<version>/` below the Yi Console data directory. Bundling does not change operating-system or application permissions.

## Use

1. Start Yi Console normally on the target Windows computer and open a Claude Code, Codex, Cursor, Kiro, Qoder or CodeBuddy chat session.
2. Click Computer use in the composer toolbar, refresh the window list and select the window the AI may operate.
3. With no reply in progress, enable computer use. The tool connects to a session-scoped MCP entry and continues the existing tool session ID where possible.
4. Give a specific task, such as: “Read this test window, enter the validation text, click Apply, then inspect a screenshot to verify the result.”
5. Review activity and screenshots in chat. Click an image to enlarge or save it, or use Capture in the control bar to inspect the target on demand. On phones, the computer icon opens capture and pause controls; the status dot and stop button remain in the toolbar.

Only one session controls a desktop at a time. Pause or stop it before another session takes control. Select and enable again to change the target; access does not automatically expand to the entire desktop.

Kiro CLI currently cannot pass MCP images directly to its model. Yi Console supplies the captured window image as a session artifact for Kiro’s built-in image reader. If file approval is requested, review and allow only that session screenshot.

## Pause, stop and recovery

- Pause blocks new actions, interrupts queued requests and releases control. Read fresh state after resuming; old element handles may have expired.
- Stop revokes desktop authorization and interrupts the current AI reply. An old MCP proxy cannot continue operating.
- Normal session interruption pauses desktop activity. Ending a session, changing mode and `/clear` revoke authorization.
- Actions already performed are not rolled back. Interrupted or timed-out actions may need a new screenshot or human verification and are not replayed automatically.
- Closing a client does not automatically end a background task. The daemon can continue while the Windows desktop remains available. Re-enable computer use after a daemon restart.

## Results

“Request returned” means the driver answered; it does not prove that the business task succeeded. The AI should inspect fresh state or a screenshot. Refusal, suspected no-op, timeout and interruption are reported separately.

Screenshots are session artifacts; events contain image references. Capture is on demand or at checkpoints, not a live video stream. Each session retains at most its latest 24 desktop screenshots, up to 80 MiB. Older activity remains, but its image may have been removed. This cache cleanup does not remove user-uploaded images. Existing session artifact cleanup applies when a session is deleted or cleared.

This feature controls operations through the Yi Console desktop gateway. It is not an operating-system sandbox for other processes running as the same user. Start with a test window to check application compatibility.

## Coverage compared with OpenBot

The comparison uses the [pinned OpenBot architecture](https://github.com/nightly-labs/openbot/blob/480dcbb30ba9aca8d5d52082b09c0f230d958d49/docs/architecture/computer-use.md) and cua-driver 0.34.0. OpenBot forwards the driver's 59 tools. Yi Console exposes 11 tools under an explicitly selected-window grant.

| Workflow | Yi Console |
| --- | --- |
| Screenshot, accessibility tree, verification | `get_window_state`, `verify_state`, `zoom` |
| Text, value, shortcuts, menus | `type_text`, `set_value`, `press_key`, `invoke_menu` |
| Click, double/right click, drag, scroll | `click` (button/count arguments), `drag`, `scroll`, `move_cursor` |
| Launch/switch apps, full desktop, global clipboard | Not exposed; requires broader grants and controls |
| Browser extension, navigation, downloads, file pickers | Not exposed; separate top-level dialogs do not inherit the grant |
| Driver installation/update/configuration/escalation | Managed by the application, unavailable to the model |

The current set supports observation, forms, clicks, and verification within one window. Cross-application workflows and separate top-level dialogs remain gaps. Actual accessibility support must be tested per application.

OpenBot places its switch in Agent settings. Yi Console grants access per session and target window, so the entry is in the composer toolbar; screenshot, pause, and stop controls appear only while enabled. External servers use the [generic MCP configuration](mcp.en.md) and share the same provider integration.
