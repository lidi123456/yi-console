# 外接 MCP 服务器

[English](mcp.en.md)

在主机设置的「MCP 服务器」中统一配置外部工具，供 Claude Code、Codex、Cursor、Kiro、Qoder 和 CodeBuddy 的聊天模式使用。配置在目标主机上运行，客户端平台不影响此能力；它独立于仅支持 Windows 的「操作电脑」。终端模式继续使用各 CLI 原有配置。

## 配置

1. 打开目标主机的设置，找到「MCP 服务器」，点「添加」。
2. 填写服务器名称，选择本地进程（stdio）或 HTTP，指定使用它的 AI 工具；默认全部六种。
3. stdio 填命令、参数 JSON 字符串数组及环境变量 JSON 对象；HTTP 填 URL 及请求头 JSON 对象。例如参数 `["C:/tools/server.py", "argument with spaces"]` 会保留空格，不经过 shell 拼接。
4. 保存编辑，再保存主机设置。空闲聊天会话在下一条消息前重载；正在回复的会话在当前回复结束后重载。禁用或删除服务器遵循相同规则，不会终止正在进行的 MCP 调用。

环境变量和请求头保存后返回 `********`，原样保存占位符会保留旧值。删除键可移除它；实际密钥保存在目标主机的设置文件中，未做磁盘加密。服务器命令拥有 daemon 用户的权限，输入框中的参数不是沙箱。

## 连接方式

- Claude Code：当前进程的 `--mcp-config`。
- Codex：当前进程的 `-c mcp_servers.<name>=...`。
- Cursor、Kiro、Qoder、CodeBuddy：ACP `session/new` / `session/load` 的 `mcpServers`。
- HTTP 需要 CLI 支持对应传输；ACP 工具必须在初始化能力中声明 HTTP 支持，否则显示错误。凭据通过配置传递，不替用户完成远程 OAuth 登录。

不修改各工具的全局 MCP 文件，已有 CLI 配置继续生效。命名冲突应在主机设置中改名；`yi_computer_use` 保留给内置桌面网关，不能创建同名外接服务器。外接 MCP 沿用工具自身的审批，内置桌面的窗口授权不会自动信任其他服务器。

## 验证

`scripts/mcp/probe-server.py` 提供无副作用的测试服务器，返回环境变量中的随机 nonce，并记录初始化、工具枚举和调用；`scripts/mcp/windows-providers-e2e.mjs` 通过隔离的 yi-web 和实际 CLI 验证六种接入、参数空格、密钥往返及设置重载。模型账号认证或额度失败必须与 MCP 连接失败分别报告。

2026-11-05 Windows 实机结果（stdio）：

| 工具 | 连接 / 枚举 | 模型实际调用 |
| --- | --- | --- |
| Claude Code 2.1.296 | 通过 | 通过 |
| Codex 0.163.0 | 通过 | 当前企业网关返回 403，未完成 |
| Cursor 2026.10.22-27dcf5f | 通过 | 既有 Ark CLI Hook 返回无效 JSON，阻止调用 |
| Kiro 2.37.1 | 通过 | 通过；忙碌和空闲会话重载均通过 |
| Qoder 1.1.80 | 通过 | 通过 |
| CodeBuddy 2.167.0 | 通过 | 通过（正常审批 DeferExecuteTool） |

这份结果验证 MCP 接入与随机返回值，不代表六种模型均已完成真实桌面操作，也不代表 HTTP 传输均已实机验证。桌面模型闭环另以 Kiro 的原生窗口输入、按钮回调和截图随机码核验为证。
