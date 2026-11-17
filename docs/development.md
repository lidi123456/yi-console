# 开发指南

English: [Development guide](development.en.md)

这里说明怎么从源码构建 Yi Console、代码怎么组织、怎么发布。改代码前请再读一下 [AGENTS.md](../AGENTS.md)（英文），里面记录了项目约定和容易踩的坑。

## 架构

```
 桌面 / 手机 App（Tauri + React）            浏览器（yi-web，开发调试用）
            │  ClientHub（crates/client）：主机管理、SSH、中继、断线重连
   ┌────────┼──────────────────────────┐
   │ 本机   │ SSH exec yi-agentd bridge │ 中继 WebSocket（端到端加密）
   ▼        ▼                           ▼
 yi-agentd（每台机器一个，unix socket / 命名管道，不监听 TCP）
   ├─ claude -p --input-format stream-json …   （结构化）
   ├─ codex app-server                          （结构化）
   ├─ cursor-agent acp / kiro-cli acp           （ACP 协议）
   ├─ qodercli --acp / codebuddy --acp          （ACP 协议）
   └─ PTY                                       （终端模式）
```

| 目录 | 内容 |
|---|---|
| `crates/proto` | 客户端与 yi-agentd 之间的 JSON-RPC 协议类型 |
| `crates/yi-agentd` | 守护进程：会话管理、各工具适配、历史导入、用量统计、开机自启、中继客户端 |
| `crates/link` | 中继协议：身份密钥、配对 / 接入链接、中继层签名鉴权、Noise IK 端到端加密 |
| `crates/relay` | 中继服务器 `yi-relay` |
| `crates/client` | 客户端核心，桌面 / 手机 App 和 yi-web 共用 |
| `crates/web` | `yi-web`：在浏览器里用同一套前端，开发调试用 |
| `app/` | React 前端；`app/src-tauri` 是 Tauri 外壳 |
| `scripts/` | 构建辅助脚本 |

要接入新的 AI 编程工具，见[接入新的 AI 编码工具](adding-a-tool.md)。

## 环境要求

- Rust stable
- Node 20+
- 快捷构建脚本需要 Python 3.9+；编译内置插件还需要 `rustup target add wasm32-wasip2`
- 在 Linux 上构建 Tauri 桌面版，还要装 `libwebkit2gtk-4.1-dev` 等依赖，见 [Tauri 文档](https://v2.tauri.app/start/prerequisites/)

## 构建和运行

统一入口是 `scripts/build.py`，从任何目录运行均可（Windows 可将 `python3` 换成 `python`）。默认使用 release 模式：

```bash
python3 scripts/build.py --list                         # 查看组件、平台和分组
python3 scripts/build.py backend frontend               # 本机三个 Rust 程序和前端
python3 scripts/build.py yi-agentd                      # 只编译守护进程
python3 scripts/build.py yi-relay --debug                # 只编译中继的 debug 版
python3 scripts/build.py yi-agentd --target aarch64-unknown-linux-musl --dry-run
python3 scripts/build.py plugins                        # 构建并打包内置 GitHub / 飞书插件
python3 scripts/build.py all                            # 本地所有组件，含插件
```

本地组件为 `frontend`、`yi-agentd`、`yi-relay`、`yi-web`、`plugins`，`backend` 是三个 Rust 程序的分组；可用空格或逗号组合。前端在缺少 `app/node_modules` 时自动执行 `npm ci`，加 `--npm-ci` 可强制重装依赖。Rust 产物在 `target/release/`（`--debug` 时为 `target/debug/`，指定 `--target` 时多一层目标目录，遵循 `CARGO_TARGET_DIR`），前端在 `app/dist/`，插件包在 `plugins/dist/`，并复制到桌面资源目录。`--debug` / `--target` 只作用于 Rust 程序，插件固定为 release。交叉编译所需的 Rust target 和链接器需要自行准备，脚本不会安装系统依赖；发布用的 Linux musl 守护进程仍由 CI 中的 `cross` 构建。

正式安装包默认在 CI 构建。安装 [GitHub CLI](https://cli.github.com/)，运行 `gh auth login`，并先将代码和本次脚本 / workflow 改动推送到构建分支：

```bash
python3 scripts/build.py --ci all --download out         # 所有平台，等完成并下载产物
python3 scripts/build.py --ci windows                    # 仅 Windows 安装包
python3 scripts/build.py --ci windows android --wait     # 指定多个平台
python3 scripts/build.py --ci macos-aarch64 --ref main   # 仅 Apple Silicon 安装包
python3 scripts/build.py --ci daemons                    # 仅各平台守护进程和 Linux 中继
python3 scripts/build.py --ci linux --dry-run            # 仅预览，不触发构建
```

`--ci` 可选 `windows-x86_64`、`macos-aarch64`、`macos-x86_64`、`linux-x86_64`、`android`、`daemons`；也可用 `windows`、`macos`、`linux`、`desktop`、`all` 分组。安装包格式沿用 CI：Windows EXE / MSI、macOS DMG、Linux DEB / RPM / AppImage、Android APK。桌面安装包仍会构建并内置各平台的 yi-agentd，以便部署到 SSH 主机；只选 Android 时跳过守护进程和桌面任务。

CI 默认构建当前分支的**已推送代码**，不会上传本地未提交的改动；游离 HEAD 时必须指定 `--ref <分支或标签>`，`--repo OWNER/REPO` 可指定仓库。默认返回构建链接，`--wait` 等待完成并返回构建状态，`--download <目录>` 会等待成功后按 artifact 名称分目录下载。下载目录相对于运行脚本时的当前目录。不带 `--ci` 的 `all` 表示本地所有组件，带 `--ci` 的 `all` 表示所有平台包。所有模式均支持 `--dry-run`。

本机已装好 JDK 17、Android SDK / NDK 和对应 Rust target 时，也可以显式构建 Android APK（`all` 不包含此入口）：

```bash
python3 scripts/build.py android                        # 默认 ARM64 release，未签名
python3 scripts/build.py android --debug                # 可安装的 debug APK
# 设置 ANDROID_KEYSTORE_PASSWORD 后，用已有密钥签名 release APK
python3 scripts/build.py android --keystore /path/to/app.jks --key-alias app
```

设置 `ANDROID_HOME`、`NDK_HOME`、`JAVA_HOME`，可通过重复 `--android-target aarch64` / `--android-target armv7` / `--android-target x86_64` 选择 ABI。脚本刷新原生 Activity 和图标，构建 APK 并收集到项目 `out/`；指定 `--keystore`（或 `ANDROID_KEYSTORE_PATH`）时自动对齐、签名并校验，密码只从 `ANDROID_KEYSTORE_PASSWORD` 环境变量读取。复用原来的签名密钥才能覆盖安装旧包。此模式不会发布到 GitHub。

以下是手动构建和调试命令：

```bash
# 守护进程、中继、浏览器版
cargo build

# 前端
cd app && npm install && npm run build && cd ..

# 在浏览器里调试：http://127.0.0.1:3001/?token=dev
./target/debug/yi-web --listen 127.0.0.1:3001 --dist app/dist --token dev \
  --yi-agentd-dir ./target/debug
```

yi-web 会自动加一台"本机"主机，用 `--yi-agentd-dir` 下的 `yi-agentd`。SSH 主机要自动部署 yi-agentd 时，会在 `--yi-agentd-dir` 或环境变量 `YI_CONSOLE_AGENTD_DIR` 指定的目录里找 `yi-agentd-<os>-<arch>[.exe]`，os 取 linux / macos / windows，arch 取 x86_64 / aarch64。

桌面 App：

```bash
scripts/prepare-sidecar.sh          # 编译本机的 yi-agentd，放到 app/src-tauri/binaries/
cd app && npx tauri dev
```

iOS 开发包需要在 Mac 上构建，准备 Xcode、XcodeGen、CocoaPods、libimobiledevice，以及 Xcode 15 之前所需的 ios-deploy。在 Xcode 的 Settings → Accounts 中登录 Apple ID，并连接、信任用于调试的 iPhone：

```bash
rustup target add aarch64-apple-ios x86_64-apple-ios
cd app
npm run tauri -- ios init
# 使用 Xcode 中显示的 Team ID；不要将个人账号或签名材料提交到仓库
APPLE_DEVELOPMENT_TEAM=YOUR_TEAM_ID npm run tauri -- ios build --target aarch64 --archive-only
```

`tauri.ios.conf.json` 排除桌面守护进程和插件资源；iOS 客户端只连接远程主机。生成的 Xcode 工程位于 `app/src-tauri/gen/apple/`，可在 Xcode 中选择团队和真机运行。归档后，使用 Xcode 导出 Development 开发包；较新 Xcode 也可用 Tauri 的 `--export-method debugging`。免费 Personal Team 仅供本人设备开发测试，描述文件会过期，不能作为通用分发签名；可否导出和安装取决于账号、设备注册和描述文件。iOS 尚未加入 CI 发布矩阵。

## 测试

```bash
cargo test
python3 -m unittest discover -s scripts -p 'test_build.py'
cd app && npm run build             # 先跑 tsc --noEmit，再 vite build
```

端到端测试时设置 `YI_CONSOLE_HOME=<临时目录>`，让 yi-agentd 在隔离环境里运行，不动真实的 `~/.yi-console`。Windows 上命名管道的名字也会带上这个目录的哈希，测试用的守护进程不会和日常用的那个冲突。

## 发布

各平台安装包、各平台的 yi-agentd 和 Android APK 由 `.github/workflows/build.yml` 构建：推送 `v*` 标签时触发（也可以手动触发），打标签的构建会发布到 GitHub Releases。手动触发可通过 `packages` 输入框填写与 `build.py --ci` 相同的组件 / 分组，留空默认全部，也兼容原来的 `only_daemons` / `windows_only`；这两个旧选项不能与 `packages` 同时设置。标签构建始终构建完整发布包。

发一个版本：

```bash
git tag -a v0.2.0 -m "这次的更新说明"   # 标签说明就是 Release 说明，也会显示在应用的更新提示里
git push origin v0.2.0
```

版本号只来自标签，仓库里的版本号不用改：CI 用 `scripts/set-version.mjs` 把标签版本写进 `Cargo.toml`、`tauri.conf.json` 等（只在构建机上改，不提交）。格式必须是 `v主.次.修订`，哪一位都可以涨，但必须比上一个版本大；次版本号和修订号不能超过 999（Android 的 versionCode 由它们算出）。

应用自更新：release 任务用 `scripts/release-manifest.mjs` 生成 `latest.json`（桌面端，tauri-plugin-updater 的格式）和 `latest-android.json`（Android，带 APK 的 sha256），和安装包一起上传。应用从 `releases/latest/download/` 读取这两个文件，所以仓库必须是公开的，否则下载会失败（应用会当作没有新版本）。桌面端在 Windows 上用 NSIS 安装包更新，macOS 用 `.app.tar.gz`，Linux 只有 AppImage 能自更新；Android 下载 APK 后交给系统安装器，第一次需要用户允许"安装未知应用"。

需要的仓库 secrets：

- `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`：桌面更新包的签名私钥和密码（`npx tauri signer generate` 生成），公钥写在 `tauri.conf.json` 的 `plugins.updater.pubkey`。换密钥后旧版本就收不到更新了，私钥要妥善保存。
- `ANDROID_KEYSTORE_B64`（base64 编码的 `.jks`）和 `ANDROID_KEYSTORE_PASSWORD`，密钥别名必须是 `app`。应用内更新只能装同一密钥签名的 APK，所以发布版本必须配置；不配时只有手动触发的构建能用临时密钥打包。

打标签的构建缺少这些 secrets 时会直接失败。

桌面安装包目前没有做代码签名。

## 开源协议

提交贡献即表示你同意贡献的内容和本项目一样，以 [MIT 协议](../LICENSE)发布。
