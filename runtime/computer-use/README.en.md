# Bundled Computer Use runtime

[简体中文](README.md)

Windows x64 yi-agentd embeds the MIT-licensed [Cua Driver](https://github.com/trycua/cua), pinned by `manifest.json`. Prepare the executable and license with:

```sh
python scripts/prepare-computer-use.py
```

The script downloads only the pinned release and license, verifies SHA-256 and extracts the executable allowlist. Windows builds fail if preparation is missing. Generated artifacts are ignored by git. Release CI prepares them before compiling the Windows daemon; end users need no driver download. The runtime extracts the executable and accompanying MIT license into its own versioned data directory, checks the executable digest, disables vendor telemetry/update checks and never chooses a runtime executable from PATH.

macOS uses the signed `CuaDriver.app` from the official `darwin-universal` release (digests also pinned in `manifest.json`). Before building the macOS installer, CI runs `python3 scripts/prepare-computer-use.py --macos app/src-tauri/resources/yi-agentd/computer-use-macos.tgz`: the script takes only `CuaDriver.app` and the license and repacks them byte for byte, so the Developer ID signature stays valid, and verifies the executable digest. The app syncs the file to `~/.yi-console/bin/`; the daemon unpacks and verifies it again on first use. Only the macOS installer carries it.

Linux and Windows ARM do not embed or activate this driver. See [Computer use](../../docs/computer-use.en.md) for usage and limitations.
