# 接入新的 AI 编码工具

English: [Adding a new AI coding tool](adding-a-tool.en.md)

这份文档说明接入一个新工具（比如 Gemini CLI、OpenCode）时要做哪些事，才能和现有的 Claude Code、Codex、Cursor、Kiro、Qoder、CodeBuddy 用起来一样。

每一项都写了**要做什么**、**改哪里**和**参考哪个工具**。工具本身不支持的能力不用硬做，但要在界面上体现出来：不给入口，或者给出明确的提示，不能点了没反应或者报看不懂的错。最后的验收清单逐条过一遍。

## 0. 先确定接入方式

| 工具提供的接口 | 做法 | 参考 |
|---|---|---|
| ACP（Agent Client Protocol，`xxx acp` / `xxx --acp`） | **首选**。复用 `crates/yi-agentd/src/adapters/acp.rs`，只补工具特有的部分 | Kiro、Qoder、CodeBuddy、Cursor |
| 自己的结构化协议（stream-json、JSON-RPC app-server 等） | 在 `adapters/` 下新写一个适配器 | `claude.rs`（stream-json）、`codex.rs`（app-server） |
| 只有交互界面 | 只做终端模式，走 `adapters/pty.rs` | `shell` |

同一个工具通常两种模式都要做：**对话模式**（结构化，Yi Console 自己画界面）和**终端模式**（在 tmux / 伪终端里跑工具自己的界面）。两种模式要能**接着同一个工具会话**互相切换。

## 1. 基础登记（必做）

| 内容 | 位置 |
|---|---|
| 工具枚举：`ToolKind` 加变体，同步 `ALL`、`as_str()` | `crates/proto/src/lib.rs` |
| 前端类型和显示名：`ToolKind` 联合类型、`TOOL_LABEL` | `app/src/types.ts` |
| 守护进程消息里的显示名 | `crates/yi-agentd/src/hub.rs` 里返回 "Claude Code"、"Kiro" 的那个 match |
| 可执行文件名（按顺序尝试）、各平台的常见安装目录（Windows 的 `%LOCALAPPDATA%`、npm 全局目录等） | `crates/yi-agentd/src/tools.rs` 的 `binary_names`、`resolve`、`detect` |
| 进 ACP 的参数（子命令还是开关） | `acp.rs` 的 `acp_arg` |
| 工具徽标（用官方 logo，图形内联在代码里，注明来源和许可） | `app/src/components/Icon.tsx` 的 `ToolBadge` |
| 工具配色 `--c-<tool>`（亮色、暗色两套都要看） | `app/src/styles.css` 顶部 |
| 系统设置里的工具列表（路径、额外参数） | `app/src/components/SettingsView.tsx` |
| 历史页、用量页里的工具列表 | `HistoryView.tsx` 的 `HISTORY_TOOLS`，`StatsView.tsx` 的颜色和 `QUOTA_TOOLS` |
| 显示名在中英文界面都要对 | 字符串加到 `app/src/locales/zh/*.json` 和 `en/*.json`，代码里用 `getT()` 取 |

## 2. 对话模式（适配器）

适配器是一个 tokio 任务，独占工具子进程：从 `rx` 收 `AdapterCmd`，把工具的输出翻译成统一事件交给 `Emitter`。**不要直接写库**，所有事件都经 `Emitter` 落库、广播。

### 2.1 要处理的命令（`AdapterCmd`，定义在 `adapters/mod.rs`）

| 命令 | 要求 |
|---|---|
| `Send(Prompt)` | 发一轮用户消息。带图片时：工具支持就内联（ACP 看 `promptCapabilities.image`），不支持就给出明确的错误 |
| `Interrupt` | 中断当前回合（ACP 是 `session/cancel`），之后必须发 `TurnEnd` |
| `SetModel` | 切换模型，下一轮生效（ACP 是 `session/set_model`）。失败时发 `Error`，别让界面卡在「切换中」 |
| `SetPermissionMode` | 尽量在运行中切换（ACP 是 `session/set_mode`）。做不到的告诉 hub 重启进程（参考 Claude 切到全部放行） |
| `SetEffort` | 思考等级。目前只有 Claude、Codex 支持，新工具支持的话要同时放开 `Hub::set_effort` 里的限制 |
| `Permission` | 回答审批或提问（见 2.4） |
| `StopTask` | 停止后台任务（只在工具有后台任务时实现） |
| `Kill` | 结束进程，不要再发后续事件 |

### 2.2 要发出的事件和状态（`Emitter`，定义在 `hub.rs`）

| 方法 / 事件 | 什么时候发 |
|---|---|
| `external_id(id)` | 工具的会话 id 一拿到就发（ACP 是 `session/new` / `session/load` 的结果）。它是续会话、切换模式、导入历史的根本，必须有 |
| `status(...)` | starting → idle / running / waiting_permission / error |
| `EventBody::UserMessage` | 由 hub 发，适配器不要重复发。只有回放历史时，适配器把工具回放的用户消息转成它 |
| `AssistantDelta` / `ThinkingDelta` | 流式文字（临时事件，seq 为 0，不落库） |
| `AssistantText` / `Thinking` | 一段文字结束时发完整内容，以它为准 |
| `ToolCall` / `ToolResult` | 工具调用和结果。`name` 用工具原本的名字（ACP 从 `_meta.<工具>.toolName` 取，见 `meta_tool_name`），`input` 尽量保留原始参数，前端靠它们显示摘要 |
| `PermissionRequest` / `PermissionResolved` | 审批（见 2.4） |
| `Plan` | 任务清单（见 2.6） |
| `TurnEnd` | **每一轮都必须发**，包括出错、被中断的回合。排队消息、换审批模式重启都靠它触发（`after_turn`） |
| `Usage` / `context(%)` | token、费用 / 积分、上下文占比，能拿到就报（见第 6 节） |
| `models(list)` / `model(id)` | 可选模型和当前模型 |
| `commands(list)` | 斜杠命令列表（ACP 是 `available_commands_update`） |
| `permission_mode(mode)` | 工具自己换了审批模式（比如规划完退出 plan）。认不出的模式也照原样报 |
| `Compact` | 上下文压缩开始、完成、失败（见 2.7） |
| `Error` / `System` | 错误和系统提示。错误信息要让人看得懂（见 2.8） |
| `title_if_empty` | 工具给了会话标题就报 |
| `exited(msg)` | 进程退出，异常退出要带原因 |

### 2.3 续会话和回放历史

- **`StartSpec.resume` 有值时接着已有会话**（ACP 是 `session/load`）。
- **`StartSpec.replay` 为 true 时，把工具回放的历史记进事件日志**（从工具历史导入的会话）；为 false 时回放的内容全部丢掉，不然会重复。
- **`replay_skip`**：从终端模式切回对话模式时，只记跳过前 N 条用户消息之后的内容。参考 `acp.rs` 里 `replay_users` 的计数。
- **工具里找不到这个会话时**（会话建了没说话就退出，工具没存），改成新开，不要报错卡住。参考 `is_missing_session`。
- **会话被别的进程占着时**，说清楚是谁占着。参考 Kiro 的 `kiro_lock_holder`。
- **能离线读工具的会话文件**（Claude、Codex）就在建会话时直接导入，不靠工具回放。这时 `hub.rs` 里判断 `offline` 的几处都要加上新工具。

### 2.4 审批和提问

- 审批模式统一为 `default / acceptEdits / auto / plan / bypass`，只开放工具真正支持的。**两处要同步**：`hub.rs` 的 `permission_modes(tool, mode)` 和 `NewSession.tsx` 的 `permModes`，两种模式分别写。
- 审批请求转成 `PermissionRequest`，选项保留工具给的 id。自动放行时，优先选「只允许这一次」，不要选会改变工具自身模式的选项（Qoder 的「本会话都允许」会把它自己切到 acceptEdits）。
- 工具向用户提问时，走同一条路：`PermissionRequest` 带 `questions`，选项固定为 `answer` / `skip`。参考 Claude 的 AskUserQuestion 和 Cursor 的 `cursor/ask_question`。工具没有提问机制、只在正文里问的（Kiro），可以在前端识别后给出可点选的卡片（`ChatView.tsx`）。

### 2.5 回复中继续发消息

`session.send` 带 `queue: true`：
- 工具支持插话（Claude 写 stdin，Codex `turn/steer`）就直接交给工具，并把新工具加进 `hub.rs` 里 `steer` 的判断。
- 不支持的什么都不用做，hub 会排队，回合结束后再发。

### 2.6 任务清单

工具有 todo / plan 时：
- 标准形式（ACP `plan`、Codex `turn/plan/updated`）在适配器里转成 `EventBody::Plan`。
- 通过工具调用维护的（Kiro `todo_list`、Claude `TaskCreate` / `TaskUpdate`），在 `app/src/plan.ts` 的 `PlanTracker` 里解析，并把工具名加进 `PLAN_TOOLS`（这些调用本身就不在时间线显示了）。

### 2.7 斜杠命令、/clear、/compact

- 工具的斜杠命令通过 `commands()` 报给界面。
- `/clear` 由 hub 统一处理（清空事件、`external_id` 置空、下次新开），适配器不用管。
- `/compact` 等同发一条 `/compact` 消息。工具不认 `/` 命令的，要在适配器里转成对应的接口（参考 `codex.rs` 的 `start_turn`）。有压缩事件的转成 `EventBody::Compact`。没有的话保留用户气泡，并在 `AGENTS.md` 里注明。

### 2.8 错误提示

- 没登录：给出这台机器上该执行的登录命令。参考 `acp.rs` 的 `login_hint`，加上新工具的命令。
- 工具不是用 JSON-RPC error 报失败的（CodeBuddy 用 `stopReason: refusal` + `_meta`），要专门取出错误信息。参考 `meta_error`。
- 只给 "Internal error" 这类信息时，尽量查出真实原因（占用、找不到会话、版本过旧）。
- 守护进程的错误信息用中文，英文界面在 `app/src/i18n.ts` 里翻译。

### 2.9 回复里的图片

对话模式要告诉模型「回复里用 Markdown 图片引用主机上的绝对路径，客户端会显示」：
- 内容是 `adapters::IMAGE_HINT`。
- 工具有系统提示词入口的就传进去：Claude 用 `--append-system-prompt`，Codex 用 `developerInstructions`。
- 没有入口的，目前不加，并在 `AGENTS.md` 里注明。

## 3. 终端模式

| 内容 | 位置 / 说明 |
|---|---|
| 交互界面的启动参数：续会话（`--resume` 之类）、指定会话 id、模型、审批模式对应的开关 | `adapters/pty.rs` 的 `interactive_args` |
| 能不能第一次启动就指定会话 id（Claude、Qoder、CodeBuddy 的 `--session-id`）。能的话加进 `history::pins_session_id`，并实现 `tool_session_exists` | `crates/yi-agentd/src/history.rs` |
| 不能指定 id 的，在 `Hub::guess_terminal_session` 里从工具历史找同目录、本会话创建后用过、没被别的会话占用的最新一个 | `hub.rs`（目前是 Codex、Cursor、Kiro） |
| 对话 / 终端两种模式的会话存储不是同一份时，切换时要搬过去（Cursor 的 `cursor_handoff`） | `history.rs` |
| 退出后还被工具的后台服务占着、对话模式接不上的，要加对应参数（Codex 的 `--no-daemon`） | `tools.rs`、`hub.rs` 的 `spec_for` |
| 「在终端里继续」复制的命令 | `app/src/components/SessionActions.ts` |
| 终端模式下的审批模式 | 同 2.4，两处同步 |

## 4. 历史会话

| 内容 | 位置 |
|---|---|
| 列出工具的历史会话（标题、目录、时间、id）。能读文件就读文件，否则调工具命令（Kiro 的 `--list-sessions`），慢的要能并行 | `history.rs` 的 `list` |
| 读出完整对话（能离线读的话），用于导入和查看 | `history.rs` 的 `read`（参考 `read_claude`、`read_codex`） |
| 会话记录文件的路径，「换个工具继续」要用 | `history.rs` 的 `session_file` |
| 会话记录格式说明，给接手的工具看 | `app/src/handoff.ts` 的 `formatHint` |
| 导入后的会话第一次加载失败（被占用、没登录），恢复时还要能重新回放 | `Hub::history_missing` 的工具判断 |

## 5. 模型和额度（`crates/yi-agentd/src/probe.rs`）

- **模型列表**：临时起一次工具，问完就退，工作目录是 `<base>/probe`。ACP 从 `session/new` 结果的 `models` 取（`parse_models`）。探测时建的空会话如果会进工具的历史，探完要删掉（Qoder 用 `session/delete`）。
- **账号额度**：工具有查询接口就接上（Kiro `/usage`，Qoder `get_usage_info`，CodeBuddy `getUserInfo`），返回统一格式，加进 `StatsView.tsx` 的 `QUOTA_TOOLS`。

## 6. 用量统计（`crates/yi-agentd/src/stats.rs`）

- 工具在本地日志里记了每次请求的 token / 费用（Claude、Codex、Qoder、CodeBuddy），就**读日志**，并按文件修改时间和大小缓存。这样直接在终端里用的也算在内。同时这个工具的 usage_log 只取日志里没有的部分，避免重复计算。
- 没有日志的，用适配器报的 `Usage` 事件（只统计 Yi Console 里的会话），并在用量页的说明文字里写清楚（`StatsView.tsx`，中英文都要写）。
- 上下文占比：ACP 标准的 `usage_update`，或者工具私有的位置（Qoder 的 `context_usage_ratio`、Cursor 的 store.db）。

## 7. 对话区显示兼容（`app/src/components/ChatView.tsx`）

- `toolName()`：把工具原名映射成统一叫法（Bash / Read / Update / Write / Grep / WebFetch…），时间线才会显示成一样的样子。
- `toolKind()`：按类型给出结果摘要。改文件的工具要能取到路径和改动内容。
- `parseOutput()`：工具结果的包装格式（Cursor 多套一层字符串的 JSON，Kiro 的 `{"items":[{"Json"|"Text":…}]}`），把命令的 stdout、stderr、退出码抽出来。
- 认不出的工具照原名显示，摘要退回「输出前 3 行」。这是兜底，不能代替上面三项。

## 8. 文档和测试

- `AGENTS.md`：写一条这个工具的特殊之处（接入方式、会话文件位置、和其他工具不一样的地方），以及不支持的功能。
- 单元测试：协议解析、错误识别、历史文件解析，用真实输出截取的样例写成测试（参考 `acp.rs` 末尾的 tests）。
- 端到端测试在隔离环境里做（`YI_CONSOLE_HOME=<临时目录>`），不要动 `~/.yi-console`。

## 9. 验收清单

对话模式：
- [ ] 新建会话，发消息，流式显示文字和思考，回合正常结束
- [ ] 工具调用在时间线里显示成统一的名字和摘要，改文件能看到 diff
- [ ] 发图片：能用或者提示不支持
- [ ] 中断：能停下来，状态回到空闲
- [ ] 回复中继续发消息：插话或排队，都不丢消息
- [ ] 审批：各审批模式表现正确，界面只列出支持的模式；运行中切换审批模式有效
- [ ] 提问（如果工具有）：能回答、能跳过
- [ ] 切换模型生效；模型列表能在新建会话时取到
- [ ] 斜杠命令出现在菜单里；`/clear`、`/compact` 正常
- [ ] 任务清单显示在时间线和输入框上方
- [ ] 用量、上下文占比显示（拿不到的不显示，不显示错的）
- [ ] 结束进程后再发消息，自动接着原会话；闲置释放后也能接上
- [ ] 没登录、找不到会话、会话被占用时，提示看得懂

终端模式：
- [ ] 新开、关掉后重开，接着同一个会话
- [ ] 对话 → 终端 → 对话来回切换，对话内容不丢、不重复
- [ ] 守护进程重启后，终端会话还能接上（tmux）

历史和其他：
- [ ] 历史页能列出、能查看、能接着做（包括第一次加载失败后再恢复）
- [ ] 「换个工具继续」能把这个工具的记录交给别的工具，也能接别的工具的
- [ ] 用量页、额度显示正确，说明文字写清楚统计口径
- [ ] 徽标、颜色在亮色和暗色下都正常；中英文界面都正常
- [ ] Windows、Linux、macOS 上都能找到可执行文件
- [ ] `AGENTS.md` 已更新
