# Development guide

中文：[开发指南](development.md)

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

To add support for another AI coding tool, see [Adding a new AI tool](adding-a-tool.en.md).

## Prerequisites

- Rust stable
- Node 20+
- Python 3.9+ for the build shortcut; bundled plugins also need `rustup target add wasm32-wasip2`
- For the Tauri desktop app on Linux, `libwebkit2gtk-4.1-dev` and the other packages listed in the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/)

## Building and running

Use `scripts/build.py` from any directory (`python` can replace `python3` on Windows). Local builds use the release profile by default:

```bash
python3 scripts/build.py --list                         # list components, platforms and groups
python3 scripts/build.py backend frontend               # three native Rust programs and frontend
python3 scripts/build.py yi-agentd                      # daemon only
python3 scripts/build.py yi-relay --debug                # relay only, debug profile
python3 scripts/build.py yi-agentd --target aarch64-unknown-linux-musl --dry-run
python3 scripts/build.py plugins                        # build and pack bundled GitHub / Feishu plugins
python3 scripts/build.py all                            # all local components, including plugins
```

Local components are `frontend`, `yi-agentd`, `yi-relay`, `yi-web` and `plugins`; `backend` groups the three Rust programs. Combine names with spaces or commas. The frontend runs `npm ci` automatically if `app/node_modules` is absent; use `--npm-ci` to force a reinstall. Rust output goes to `target/release/` (`target/debug/` with `--debug`, an extra target directory with `--target`, respecting `CARGO_TARGET_DIR`), the frontend to `app/dist/`, and plugin packages to `plugins/dist/` and the desktop resources directory. `--debug` / `--target` apply only to Rust programs; plugins always use release. Install Rust targets and linkers yourself for cross-compilation; the script does not install system dependencies. Release Linux musl daemons still use `cross` in CI.

Release installers are built in CI by default. Install [GitHub CLI](https://cli.github.com/), run `gh auth login`, and push your code, including these script / workflow changes, to the build branch first:

```bash
python3 scripts/build.py --ci all --download out         # all platforms, wait and download artifacts
python3 scripts/build.py --ci windows                    # Windows installer only
python3 scripts/build.py --ci windows android --wait     # select several platforms
python3 scripts/build.py --ci macos-aarch64 --ref main   # Apple Silicon installer only
python3 scripts/build.py --ci daemons                    # per-platform daemons and Linux relays only
python3 scripts/build.py --ci linux --dry-run            # preview without dispatching
```

CI packages are `windows-x86_64`, `macos-aarch64`, `macos-x86_64`, `linux-x86_64`, `android` and `daemons`. Groups are `windows`, `macos`, `linux`, `desktop` and `all`. Installer formats follow CI: Windows EXE / MSI, macOS DMG, Linux DEB / RPM / AppImage, Android APK. Desktop installers still build and bundle yi-agentd for every platform for deployment to SSH hosts; selecting Android alone skips the daemon and desktop jobs.

CI builds the current branch's **pushed code** by default; local uncommitted changes are not uploaded. Detached HEAD requires `--ref <branch-or-tag>`; `--repo OWNER/REPO` selects a repository. By default the script returns the run URL. `--wait` waits and propagates failure; `--download <directory>` waits for success and downloads artifacts into subdirectories named after each artifact. Relative download paths are resolved from the directory where you invoked the script. Without `--ci`, `all` means all local components; with `--ci`, it means all platform packages. Every mode supports `--dry-run`.

With JDK 17, the Android SDK / NDK and Rust targets already installed, explicitly build a native Android APK (`all` excludes this entry):

```bash
python3 scripts/build.py android                        # ARM64 release, unsigned by default
python3 scripts/build.py android --debug                # installable debug APK
# Set ANDROID_KEYSTORE_PASSWORD, then sign with an existing release key
python3 scripts/build.py android --keystore /path/to/app.jks --key-alias app
```

Set `ANDROID_HOME`, `NDK_HOME` and `JAVA_HOME`. Repeat `--android-target aarch64` / `--android-target armv7` / `--android-target x86_64` to select ABIs. The script refreshes the native Activity and icons, builds APKs and collects them into the project's `out/`. With `--keystore` (or `ANDROID_KEYSTORE_PATH`), it aligns, signs and verifies the APK; the password is read only from the `ANDROID_KEYSTORE_PASSWORD` environment variable. Reuse the existing signing key to upgrade an installed build. This mode does not publish to GitHub.

Manual build and debugging commands:

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
python3 -m unittest discover -s scripts -p 'test_build.py'
cd app && npm run build             # runs tsc --noEmit, then vite build
```

For end-to-end tests, set `YI_CONSOLE_HOME=<temp dir>` so yi-agentd runs isolated and your real `~/.yi-console` is left alone. On Windows the named pipe name then includes a hash of that directory, so a test daemon never collides with the one you use day to day.

## Releases

Installers for every platform, the per-platform yi-agentd binaries and the Android APK are built by `.github/workflows/build.yml`. It runs when a `v*` tag is pushed (and can be triggered manually); tagged builds are published to GitHub Releases. Manual runs accept a `packages` input with the same components / groups as `build.py --ci`. Empty means all, and still supports the legacy `only_daemons` / `windows_only` inputs; those cannot be combined with `packages`. Tag builds always produce a complete release.

To publish a version:

```bash
git tag -a v0.2.0 -m "What changed"   # the tag message becomes the release notes and shows in the app's update prompt
git push origin v0.2.0
```

The version comes only from the tag; the version in the repository is never edited by hand. CI runs `scripts/set-version.mjs` to write the tag's version into `Cargo.toml`, `tauri.conf.json` and so on (on the build machine only, not committed). The format must be `v<major>.<minor>.<patch>`; any part may go up, but each release must be higher than the last, and minor and patch can't exceed 999 (Android's versionCode is derived from them).

Self-update: the release job runs `scripts/release-manifest.mjs` to write `latest.json` (desktop, in tauri-plugin-updater's format) and `latest-android.json` (Android, with the APK's sha256), uploaded with the installers. The app reads both from `releases/latest/download/`, so the repository must be public, otherwise the download fails (and the app treats it as no update). On the desktop, Windows updates through the NSIS installer, macOS through `.app.tar.gz`, and on Linux only the AppImage can update itself. Android downloads the APK and hands it to the system installer; the first time, the user has to allow "install unknown apps".

Repository secrets:

- `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`: private key and password that sign the desktop update bundles (made with `npx tauri signer generate`); the public key is `plugins.updater.pubkey` in `tauri.conf.json`. Installed versions stop receiving updates if the key changes, so keep the private key safe.
- `ANDROID_KEYSTORE_B64` (the base64-encoded `.jks`) and `ANDROID_KEYSTORE_PASSWORD`; the key alias must be `app`. The in-app updater can only install an APK signed with the same key, so releases require it; without it only manually triggered builds can be signed with a throwaway key.

Tagged builds fail if these secrets are missing.

Desktop installers are not code-signed yet.

## License

By contributing, you agree that your contributions are licensed under the [MIT License](../LICENSE), the same as the project.
