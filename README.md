<p align="center"><img src="app/public/icon.svg" width="96" alt="Yi Console"></p>

<h1 align="center">Yi Console</h1>

<p align="center">在一个窗口里使用和管理所有 AI 编程工具：本机的、服务器上的，在电脑和手机上都能用。</p>

<p align="center">简体中文 · <a href="README.en.md">English</a></p>

---

Claude Code、Codex 这类 AI 编程工具都跑在命令行里：开多个会话要开多个终端；换台电脑、关了终端，进行到一半的任务就看不到了；想在手机上看一眼进度更是麻烦。

Yi Console 把它们统一管起来：

- **一个界面用多种工具**：Claude Code、Codex、Cursor、Kiro、Qoder、CodeBuddy、Antigravity，以及普通终端。
- **会话不会断**：每台机器上有一个后台服务托管所有会话。关掉窗口、电脑休眠、手机切到后台，工具都继续干活；回来后接着看。
- **两种界面随时切换**：对话界面（消息、工具调用、文件改动、审批按钮），或者工具自己的终端界面。同一个会话来回切，上下文不丢。
- **管远程机器**：通过 SSH 连服务器，首次连接自动部署，不用手动安装；家里或公司内网的电脑，可以经中继服务器从手机或另一台电脑连上，全程端到端加密。
- **顺手的工作区**：文件浏览和编辑、git 改动和 diff、全文搜索、内置浏览器标签（Windows 上直接内嵌网页，不受「禁止嵌入」限制）、端口转发、底部终端面板。
- **AI 浏览器**：让 AI 在一个独立的 Chrome / Edge 里打开网页、点按钮、填表单、看请求和控制台，帮你测试本地开发的页面。页面实时显示在文件面板里，你随时可以接手操作或叫停。
- **操作电脑**：让 AI 操作主机上已登录的 Windows 或 macOS 桌面，打开应用、点击、输入，截图带回聊天。
- **外接 MCP 服务器**：在主机设置里统一配置，按工具分配，各工具的对话界面都能用。
- **Bot**（桌面版）：常驻的 AI 会话，接收 GitHub、飞书等插件带来的消息，自己回复，或者派出任务会话去做，需要人时才提醒你。手机上可以远程查看和管理。
- **多账号**：每个工具可以保存多个登录账号和 API Key，会话里随时切换，额度用完按顺序换下一个账号；额度恢复后自动发「继续」。
- **用量一目了然**：各工具的 token、费用和账号额度；回复完成或需要审批时弹系统通知。
- **接上以前的会话**：导入各工具自己的历史会话继续聊，或者把一个会话交给另一个工具接着做。

支持 Windows、macOS、Linux 桌面，以及 Android 手机（iOS 可以自行构建开发包）。桌面版和 Android 版都能在 App 内自动更新。界面有中文和英文。

## 安装

1. 到 [Releases](../../releases) 下载对应系统的安装包：

   | 系统 | 文件 |
   |---|---|
   | Windows | `*-setup.exe` 或 `.msi` |
   | macOS | `.dmg` |
   | Linux | `.deb`、`.rpm` 或 `.AppImage` |
   | Android | `yi-console.apk` |

   安装包目前没有做代码签名：Windows 可能弹出 SmartScreen 提示，点「更多信息 → 仍要运行」；macOS 第一次打开时在访达里右键 App 选「打开」。

2. 在要使用的机器上装好 AI 编程工具并登录，至少装一个：

   | 工具 | 准备 |
   |---|---|
   | Claude Code | 安装 `claude` 并登录 |
   | Codex CLI | 安装 `codex` 并登录 |
   | Cursor CLI | 安装 `cursor-agent`，执行 `cursor-agent login` |
   | Kiro CLI | 安装 `kiro-cli` 并登录 |
   | Qoder CLI | 安装 `qodercli`，执行 `qodercli login` |
   | CodeBuddy Code | `npm i -g @tencent-ai/codebuddy-code`，运行 `codebuddy` 后输入 `/login` |
   | Antigravity CLI | 安装 `agy`，运行一次 `agy` 登录（远程主机上用 `agy acp --login`） |

   不装 AI 工具也可以，只把它当终端用。

## 快速上手

1. 打开 Yi Console。左侧会出现「本机」，后台服务会自动启动，不用另外安装。
2. 点「新建会话」，选择工具和工作目录，选「对话」或「终端」界面。
3. 在输入框里说出你的需求。AI 要改文件或执行命令时，会弹出审批卡片；也可以在输入框左下角改成「自动接受编辑」或「全部放行」。
4. 右上角的文件夹按钮打开工作区：浏览文件、查看改动和 diff。
5. 关掉窗口也没关系，工具会在后台继续工作。完成时会收到系统通知，再打开 App 就能看到结果。

更多用法见[使用手册](docs/guide.md)。

## 连接其他机器

- **SSH 服务器**：在左下角「添加 / 管理主机」里添加主机，也可以从 `~/.ssh/config` 导入。第一次连接时，Yi Console 会自动把后台服务上传到服务器并启动，不需要在服务器上手动安装任何东西，也不会在服务器上开放端口。
- **手机或另一台电脑连这台电脑**：电脑在内网、从外面连不进来时，用中继服务器转发。先在一台有公网地址的服务器上部署中继（一个独立的小程序），然后在电脑上生成二维码，手机扫码配对即可。数据端到端加密，中继服务器看不到内容。

详细步骤见[远程连接与中继](docs/remote.md)。

## 工作原理

```
  桌面 / 手机 App                         浏览器（yi-web，开发调试用）
        │
        ├── 本机 ──────────────── yi-agentd ── Claude Code / Codex / Cursor / Kiro / Qoder / CodeBuddy / Antigravity / 终端
        ├── SSH ───────────────── yi-agentd ── …
        └── 中继（端到端加密）── yi-agentd ── …
```

每台机器上运行一个后台服务 `yi-agentd`，负责启动和托管 AI 工具进程、保存会话记录。它只通过本机的 socket / 命名管道通信，不监听任何网络端口；远程访问都经 SSH 或中继转发。App 只是界面，断线后重新连上，会从断点继续接收消息。

## 文档

- [使用手册](docs/guide.md)：会话、对话输入框、终端、文件和改动、快捷键、通知、数据位置
- [远程连接与中继](docs/remote.md)：SSH 主机、经中继连接、部署中继服务器、网络代理
- [操作电脑](docs/computer-use.md)：支持范围、权限、使用步骤
- [外接 MCP 服务器](docs/mcp.md)
- [编写 Bot 插件](docs/connectors.md)
- [开发指南](docs/development.md)：从源码构建、项目结构、打包发布
- [中继安全设计](docs/relay-security.md)
- [接入新的 AI 工具](docs/adding-a-tool.md)

## 参与开发

欢迎提交 Issue 和 Pull Request。从源码构建见[开发指南](docs/development.md)，统一构建入口为 `python3 scripts/build.py --list`，支持本地组件和 CI 各平台安装包；改代码前请先读 [AGENTS.md](AGENTS.md)（英文，主要给 AI 编程助手看，也是项目结构和约定的说明）。

## 开源协议

[MIT License](LICENSE)

Claude Code、Codex、Cursor、Kiro、Qoder、CodeBuddy、Antigravity 是各自所有者的商标，Yi Console 与这些公司没有关联。界面中的工具图标取自 [@lobehub/icons](https://github.com/lobehub/lobe-icons)（MIT）。
