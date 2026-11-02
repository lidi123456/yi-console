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
- 在 Linux 上构建 Tauri 桌面版，还要装 `libwebkit2gtk-4.1-dev` 等依赖，见 [Tauri 文档](https://v2.tauri.app/start/prerequisites/)

## 构建和运行

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

## 测试

```bash
cargo test
cd app && npm run build             # 先跑 tsc --noEmit，再 vite build
```

端到端测试时设置 `YI_CONSOLE_HOME=<临时目录>`，让 yi-agentd 在隔离环境里运行，不动真实的 `~/.yi-console`。Windows 上命名管道的名字也会带上这个目录的哈希，测试用的守护进程不会和日常用的那个冲突。

## 发布

各平台安装包、各平台的 yi-agentd 和 Android APK 由 `.github/workflows/build.yml` 构建：推送 `v*` 标签时触发（也可以手动触发），打标签的构建会发布到 GitHub Releases。

Android 签名用两个仓库 secrets：`ANDROID_KEYSTORE_B64`（base64 编码的 `.jks`）和 `ANDROID_KEYSTORE_PASSWORD`，密钥别名必须是 `app`。不配也能构建，但每次构建签名都不同，用户升级前要先卸载旧版。

桌面安装包目前没有做代码签名。

## 开源协议

提交贡献即表示你同意贡献的内容和本项目一样，以 [MIT 协议](../LICENSE)发布。
