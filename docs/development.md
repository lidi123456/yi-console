# Development guide

How to build Yi Console from source, how the code is organized, and how releases are made. Before changing code, also read [AGENTS.md](../AGENTS.md), which records the conventions and the pitfalls worth knowing.

## Architecture

```
 Desktop / mobile app (Tauri + React)          Browser (yi-web, for development)
            │  ClientHub (crates/client): host management, SSH, relay, reconnects
   ┌────────┼───────────────────────────┐
   │ local  │ SSH exec yi-agentd bridge  │ relay WebSocket (end-to-end encrypted)
   ▼        ▼                            ▼
 yi-agentd (one per machine; unix socket / named pipe, no TCP listener)
   ├─ claude -p --input-format stream-json …   (structured)
   ├─ codex app-server                          (structured)
   ├─ cursor-agent acp / kiro-cli acp           (ACP)
   ├─ qodercli --acp / codebuddy --acp          (ACP)
   └─ PTY                                       (terminal mode)
```

| Directory | Contents |
|---|---|
| `crates/proto` | JSON-RPC protocol types between clients and yi-agentd |
| `crates/yi-agentd` | The daemon: session management, per-tool adapters, history import, usage stats, autostart, relay client |
| `crates/link` | Relay protocol: identity keys, pairing / access links, relay-level signature auth, Noise IK end-to-end encryption |
| `crates/relay` | The relay server, `yi-relay` |
| `crates/client` | Client core shared by the desktop / mobile app and yi-web |
| `crates/web` | `yi-web`: serves the same frontend to a browser, for development |
| `app/` | React frontend; `app/src-tauri` is the Tauri shell |
| `scripts/` | Build helpers |

To add support for another AI coding tool, see [Adding a new AI tool](adding-a-tool.md).

## Prerequisites

- Rust stable
- Node 20+
- For the Tauri desktop app on Linux, `libwebkit2gtk-4.1-dev` and the other packages listed in the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)

## Building and running

```bash
# Daemon, relay and browser version
cargo build

# Frontend
cd app && npm install && npm run build && cd ..

# Debug in a browser at http://127.0.0.1:3001/?token=dev
./target/debug/yi-web --listen 127.0.0.1:3001 --dist app/dist --token dev \
  --yi-agentd-dir ./target/debug
```

yi-web adds a "Local" host automatically, using the `yi-agentd` in `--yi-agentd-dir`. To auto-deploy yi-agentd to SSH hosts, it looks for `yi-agentd-<os>-<arch>[.exe]` in `--yi-agentd-dir` or in the directory named by the `YI_CONSOLE_AGENTD_DIR` environment variable, where os is linux / macos / windows and arch is x86_64 / aarch64.

Desktop app:

```bash
scripts/prepare-sidecar.sh          # build yi-agentd for this machine into app/src-tauri/binaries/
cd app && npx tauri dev
```

## Testing

```bash
cargo test
cd app && npm run build             # runs tsc --noEmit, then vite build
```

For end-to-end tests, set `YI_CONSOLE_HOME=<temp dir>` so yi-agentd runs isolated and your real `~/.yi-console` is left alone. On Windows the named pipe name then includes a hash of that directory, so a test daemon never collides with the one you use day to day.

## Releases

Installers for every platform, the per-platform yi-agentd binaries and the Android APK are built by `.github/workflows/build.yml`. It runs when a `v*` tag is pushed (and can be triggered manually); tagged builds are published to GitHub Releases.

Android signing uses two repository secrets: `ANDROID_KEYSTORE_B64` (the base64-encoded `.jks`) and `ANDROID_KEYSTORE_PASSWORD`; the key alias must be `app`. Builds work without them, but each build is then signed with a different key, so users have to uninstall the old version before upgrading.

Desktop installers are not code-signed yet.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](../LICENSE), the same as the project.
