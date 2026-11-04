<p align="center"><img src="app/public/icon.svg" width="96" alt="Yi Console"></p>

<h1 align="center">Yi Console</h1>

<p align="center">Use and manage all your AI coding tools from one window, on this machine or your servers, from desktop or phone.</p>

<p align="center"><a href="README.md">简体中文</a> · English</p>

---

https://github.com/user-attachments/assets/52adcbc2-4c73-4164-8b36-76c213bd91c9

*56-second walkthrough: create a tool session, describe a task, review the Git diff and page preview, then switch to the mobile layout. English interface and captions; no audio.*

AI coding tools like Claude Code and Codex live in the terminal. Several sessions mean several terminal windows; switch computers or close the terminal and a half-finished task is out of sight; checking progress from your phone is a hassle.

Yi Console brings them together:

- **Many tools, one interface**: Claude Code, Codex, Cursor, Kiro, Qoder, CodeBuddy, plus a plain terminal.
- **Sessions that don't die**: a background service on each machine hosts every session. Close the window, put the computer to sleep, or send the phone app to the background, and the tools keep working; come back and pick up where you left off.
- **Two views, switch any time**: a chat view (messages, tool calls, file changes, approval buttons) or the tool's own terminal UI. Switch the same session back and forth without losing context.
- **Remote machines**: connect to servers over SSH, with automatic deployment on first connect and nothing to install by hand. Reach computers on a home or office network from your phone or another computer through a relay, end-to-end encrypted.
- **A handy workspace**: file browsing and editing, git changes and diffs, full-text search, built-in browser tabs, port forwarding, and a bottom terminal panel.
- **Usage at a glance**: tokens, cost and account quotas per tool; system notifications when a reply finishes or needs approval.
- **Continue earlier work**: import each tool's own history sessions and keep chatting, or hand a session over to a different tool.

Runs on Windows, macOS and Linux desktops, and on Android phones. The UI is available in English and Chinese.

## Install

1. Download the package for your system from [Releases](../../releases):

   | System | File |
   |---|---|
   | Windows | `*-setup.exe` or `.msi` |
   | macOS | `.dmg` |
   | Linux | `.deb`, `.rpm` or `.AppImage` |
   | Android | `yi-console.apk` |

   The packages aren't code-signed yet: on Windows, SmartScreen may warn you; click "More info → Run anyway". On macOS, the first time, right-click the app in Finder and choose "Open".

2. Install and log in to at least one AI coding tool on the machines you'll use:

   | Tool | Setup |
   |---|---|
   | Claude Code | install `claude` and log in |
   | Codex CLI | install `codex` and log in |
   | Cursor CLI | install `cursor-agent`, run `cursor-agent login` |
   | Kiro CLI | install `kiro-cli` and log in |
   | Qoder CLI | install `qodercli`, run `qodercli login` |
   | CodeBuddy Code | `npm i -g @tencent-ai/codebuddy-code`, run `codebuddy` and type `/login` |

   You can also skip the AI tools and use it just as a terminal.

## Quick start

1. Open Yi Console. "Local" appears on the left, and the background service starts automatically; there's nothing else to install.
2. Click "New Session", pick a tool and a working directory, and choose the chat or terminal view.
3. Describe what you want in the input box. When the AI wants to edit files or run commands, an approval card appears; you can also switch to "Accept Edits" or "Bypass All" at the bottom left of the input box.
4. The folder button at the top right opens the workspace: browse files and review changes and diffs.
5. Closing the window is fine; the tool keeps working in the background. You'll get a system notification when it's done, and the result is there when you open the app again.

See the [user guide](docs/guide.en.md) for more.

## Connecting to other machines

- **SSH servers**: add a host under "Add / Manage Hosts", or import from `~/.ssh/config`. On first connect, Yi Console uploads and starts the background service on the server automatically. Nothing needs to be installed on the server by hand, and no ports are opened there.
- **Reaching this computer from a phone or another computer**: when the computer is on an internal network that can't be reached from outside, go through a relay. First deploy the relay (a small standalone program) on a server with a public address, then generate a QR code on the computer and scan it with your phone to pair. Data is end-to-end encrypted; the relay can't see the content.

See [Remote access and relay](docs/remote.en.md) for step-by-step instructions.

## How it works

```
  Desktop / mobile app                     Browser (yi-web, for development)
        │
        ├── local ───────────────── yi-agentd ── Claude Code / Codex / Cursor / Kiro / Qoder / CodeBuddy / terminal
        ├── SSH ─────────────────── yi-agentd ── …
        └── relay (end-to-end) ──── yi-agentd ── …
```

Each machine runs a background service, `yi-agentd`, which starts and hosts the AI tool processes and stores session logs. It only talks over a local socket / named pipe and listens on no network ports; remote access always goes through SSH or the relay. The app is just the interface: after a disconnect, it reconnects and resumes receiving messages where it stopped.

## Documentation

- [User guide](docs/guide.en.md): sessions, the input box, terminals, files and changes, shortcuts, notifications, where data lives
- [Remote access and relay](docs/remote.en.md): SSH hosts, connecting through a relay, deploying a relay server, network proxies
- [Development guide](docs/development.en.md): building from source, project layout, packaging and releases
- [Relay security design](docs/relay-security.en.md)
- [Adding a new AI tool](docs/adding-a-tool.en.md)

## Contributing

Issues and pull requests are welcome. See the [development guide](docs/development.en.md) to build from source, and read [AGENTS.md](AGENTS.md) before changing code; it records the project layout and conventions.

## License

[MIT License](LICENSE)

Claude Code, Codex, Cursor, Kiro, Qoder and CodeBuddy are trademarks of their respective owners; Yi Console is not affiliated with these companies. Tool icons in the UI come from [@lobehub/icons](https://github.com/lobehub/lobe-icons) (MIT).
