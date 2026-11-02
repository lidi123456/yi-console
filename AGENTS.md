# AGENTS.md

整体介绍和使用方法见 [README.md](README.md)。这里记录改代码时要知道的约定。

## 地图

- `crates/proto`：协议唯一来源。新增方法时，先在 `methods` 里加常量，再改 `crates/yi-agentd/src/server.rs` 的 dispatch，前端类型在 `app/src/types.ts` 里手动同步。
- `crates/yi-agentd/src/hub.rs`：会话状态机。所有事件都经 `emit()` 编号、落库、广播，适配器不要直接写库。
- `crates/yi-agentd/src/adapters/`：每个工具一个文件，统一实现 `AdapterCmd` 输入、`Emitter` 输出。ACP（Cursor / Kiro / Qoder / CodeBuddy）共用 `acp.rs`。接入新工具要做哪些事、怎么和现有工具保持一致，见 [docs/adding-a-tool.md](docs/adding-a-tool.md)。
- `crates/client/src/api.rs`：`ClientHub::call` 是前端能调的全部命令。Tauri（`app/src-tauri/src/lib.rs`）和 yi-web 都只是把它转一层。
- `app/src/store.ts`：前端状态与事件循环；组件放在 `app/src/components/`。

## 约定

- 事件里的字段用 camelCase。`EventBody` 用了 `rename_all_fields = "camelCase"`，新加的变体不用再单独标注。
- `assistant_delta`、`thinking_delta` 是临时事件，seq 为 0，不落库。回合结束时，以完整的 `assistant_message` / `thinking` 为准。
- 模型列表、额度这类"不属于某个会话"的查询放在 `crates/yi-agentd/src/probe.rs`：临时起一次工具、问完就退，工作目录是 `<base>/probe`。
- 状态栏里要在主机上算的东西（git 状态、自定义命令）在 `crates/yi-agentd/src/statusline.rs`，命令输入沿用 Claude Code statusLine 的 JSON 字段，自己的字段只往 `yi_console` 里加。
- 排队消息（`session.send` 带 `queue: true`）、审批模式切换都在 `hub.rs`：回合结束时由 `Emitter::event` 调 `after_turn()` 发下一条或重启进程。适配器收到 `AdapterCmd::SetPermissionMode` 时要尽量在运行中切换，做不到的（Claude 切到全部放行）由 hub 重启进程。
- 会话改动的 git 操作在 `crates/yi-agentd/src/gitops.rs`，路径一律相对仓库根目录，并拒绝绝对路径和 `..`。
- 用量统计（`stats.rs`）：Claude / Codex / Qoder / CodeBuddy 的用量读工具自己的日志（`~/.claude/projects`、`~/.codex/sessions`、`~/.qoder/projects`、`~/.codebuddy/projects`，按文件修改时间和大小缓存；Qoder 内置模型只记 credits，CodeBuddy 同时记 token 和 credits），usage_log 里这四个工具只取费用（Claude），避免重复计算。
- 「版本不一致」= 版本号不同或 `buildTime` 不同：拷源码构建（无 .git）版本号都是 `+dev`，未提交改动时提交号也不变，只比版本号会漏掉升级；这两种情况 `build.rs` 都用当前时间作构建时间。
- 客户端连上主机发现版本不一致时自动升级（`ClientHub::auto_upgrade`）。本机总是升级；SSH 只在客户端的 `build_time()` 比守护进程新时升级，免得两台不同版本的电脑来回覆盖；中继主机只提示。有回合在跑就等它们结束（`upgrade` 事件 phase=waiting，界面只在角落提示），上传和重启期间前端整屏遮罩（`Upgrade.tsx`）。同一次启动里每台主机只自动升级一次。
- `DAEMON_VERSION` 带提交号（`crates/proto/build.rs`，CI 用 `GITHUB_SHA`），客户端据此提示重启旧守护进程；不要再改回只用包版本号。
- 终端模式的 tmux 封装在 `crates/yi-agentd/src/tmux.rs`（`StartSpec.tmux` 有值时 pty 适配器改用 `tmux -L yi-console new-session -A`）；守护进程启动时 `Hub::reattach_terminals` 接上还活着的会话。用户主动停止 / 删除才 `kill-session`，适配器的命令通道断开（换进程）只结束 tmux 客户端。
- 终端模式的 xterm 不留回滚（tmux 每次整屏重画），往上翻走 `pty.scroll`：让 tmux 进 copy-mode 翻页，前端画自己的滚动条；打字前先退出 copy-mode。挂载时问 tmux 状态常早于 tmux 会话建好，确认在 tmux 之前每 3 秒再问（最多 20 次），否则滚动条一直不出。终端留白写在 `.xterm` 上（`--term-pad`），不要写在 `.term` 容器上：全局 border-box 下 FitAddon 会把容器 padding 算进高度，多出一行被状态栏挡住。
- 终端会话（`mode: terminal` 或 shell）在 `hub.rs` 的 `start_when_sized` 里等第一个 `pty.resize` 再起进程（最多 3 秒），前端 resize 去抖 200ms：cursor-agent、Claude Code 这类全屏程序启动后马上改尺寸会留下残影。
- 对话区仿 Claude Code 的时间线（`ChatView.tsx` 的 `ToolLine`）：工具名用 `toolName()` 统一成 Bash / Read / Update 这类叫法，结果摘要按 `toolKind()` 分类。新工具认不出时照原名显示，摘要退回"输出前 3 行"。
- `/clear` 走 `session.clear`（`Hub::clear`）：只限对话模式，结束工具进程（先作废代号）、删掉这个会话的全部事件和 uploads、用量和上下文清零、`external_id` 置空，下次发消息自然新开工具会话；Yi Console 会话本身不变。删完补一条落库的 `EventBody::Cleared`（seq 接着编），前端 `addEvents` 见到它就丢掉更早的事件，重连补拉也靠它。usage_log 不删，用量统计页照旧计入。输入框里直接打 `/clear` 回车也会被拦下，不发给工具。
- `/compact` 走 `session.compact`，等同发一条 `/compact` 消息。Codex 的 app-server 不认 `/` 命令，由 `codex.rs` 的 `start_turn` 转成 `thread/compact/start`、`review/start`（`/review`）和生成 AGENTS.md 的提示词（`/init`），命令列表见 `slash_commands()`；工具自带的同名命令在菜单里去重。
- 「换个工具继续」（会话菜单，只限对话模式、有 `external_id` 的会话，点开时没有用户消息也会拦下）：`session.logFile` 按 tool + `external_id` 给出工具自己的会话记录（`history::session_file`：Claude / Codex / Qoder / CodeBuddy 的 jsonl，Kiro 的 `~/.kiro/sessions/cli/<id>.jsonl`）；Cursor 的 ACP 会话没有可读文件，前端把「导出 Markdown」的内容经 `session.attachFile` 存进原会话的上传目录。然后打开新建对话框（`newSession.handoff`：不能选原工具和 shell、固定对话模式、不能换主机），第一条消息由 `app/src/handoff.ts` 的模板生成，只让新工具读记录后汇报、不接着做。模板可在系统设置里改（`HandoffCard.tsx`，存 localStorage `handoff-template`，留空用默认，跟随界面语言），占位符 `{tool}` `{title}` `{cwd}` `{path}` `{format}`，新增占位符时同步改 `HANDOFF_VARS`；各工具记录格式的说明在 `formatHint()`。
- 压缩进度统一用 `EventBody::Compact`（phase start/done/failed，trigger auto/manual，压缩前后 token、耗时）：Claude 取自 `system/status` 的 compacting、compact_result 和 `compact_boundary`；Codex 取自 `contextCompaction` 的 item/started、completed（手动与否看是不是我们发的 `thread/compact/start`，不报耗时由前端按时间差算）。前端原地把「正在压缩」换成结果，Claude、Codex 会话里不显示「/compact」用户气泡；Cursor、Kiro 没有压缩事件，保留气泡。
- 对话区的工具摘要要兼容各家格式：Cursor 的结果是 JSON（可能多套一层字符串），Kiro 包在 `{"items":[{"Json"|"Text":…}]}` 里，命令失败只回 `exit code: N`。都在 `ChatView.tsx` 的 `parseOutput` 里处理。
- Windows 上设了 `YI_CONSOLE_HOME` 时，命名管道名会带数据目录的哈希（`paths::pipe_name`），这样隔离测试不会连到正在用的守护进程。
- 界面主题在 `app/src/theme.ts`：选了亮色 / 暗色就在 `<html>` 上写 `data-theme`。styles.css 里暗色变量要写两份：`@media (prefers-color-scheme: dark) { :root:not([data-theme='light']) … }` 和 `:root[data-theme='dark'] …`；JS 里判断暗色用 `isAppDark()`，不要直接查 matchMedia。
- 界面字体用 `system-ui` 打头，中文 Windows 上会落到微软雅黑 UI，不要再内置网络字体。
- 侧边栏样式：`styles.css` 末尾「侧边栏」一段统一覆盖，宽 244px、行高 `--side-row-h` 32px、字号 `--fs-side` 13px、文字离左边约 14px；改侧栏改那一段，不要在前面零散加。结构是主机标题 → 目录小标题 → 会话行：主机相关的入口（历史、文件、终端、用量、主机设置）都在主机行的「…」/ 右键菜单（`hostMenu`），不要再在列表里加按钮；会话行是原色工具图标 + 灰色标题（悬停、选中时变深）+ 右侧时间，只有运行中 / 等审批 / 出错才把时间换成状态；侧栏滚动区不显示滚动条。筛选框同时是消息搜索入口（回车跳到搜索页）。
- 新建会话按工具记住上次的界面（对话 / 终端）、审批模式、模型、思考等级（localStorage `new-session-prefs`，`NewSession.tsx`），换工具时带出；记住的值当前工具或模型不支持时退回默认。
- 各工具支持哪些审批模式由 `hub.rs` 的 `permission_modes(tool, mode)` 决定，前端 `NewSession.tsx` 的 `permModes` 手动同步；界面只列出当前工具支持的模式。工具自己报回来的、我们不认识的模式（如 Claude 的 `dontAsk`）照原名记下和显示，不要丢掉。
- 后台任务（`SessionInfo.background_tasks`，只在内存里）目前只有 Claude 有：以 stream-json 的 `background_tasks_changed` 列表为准，输出文件从后台命令的 tool_result 文本里解析，`task_notification` 在对话里留一行提示，停止走 `stop_task` 控制请求。任务结束后 Claude 会自己开一轮（只有 `system/init`，没有用户消息），适配器据此把状态切回运行中。
- 任务清单统一由前端 `app/src/plan.ts` 的 `PlanTracker` 拼出来：Claude `TodoWrite`、Codex `turn/plan/updated`、ACP `plan` 由 yi-agentd 转成 `plan` 事件直接用；Kiro 的 `todo_list` 工具（结果是完整列表，只有完成 / 未完成，全部完成后会清空，此时沿用上一份全打勾）和新版 Claude 的 `TaskCreate` / `TaskUpdate`（编号取自结果 `Task #N created successfully`）在前端从工具调用里解析，这些工具调用本身不在时间线显示（出错的除外）。时间线只留最新一份卡片，全部完成后收成一行；还有没完成的任务时输入框上方显示进度条（`PlanDock.tsx`）。Kiro 没有「进行中」，回合运行时把第一个没完成的当作进行中。
- 会话页的「工作区文件」（`FilePanel.tsx`）：没打开文件时只有一栏文件树（`.file-panel.browse`，宽 `--tree-w`），打开文件后变宽（`--panel-w`），左边编辑器（上方是打开过的文件标签）、右边文件树；对话区至少留 420px，会话头和输入框靠 `.session-view` 的容器查询在窄时收起文字。文件树按 `git.changes`（带 `diff: false`，只列状态，大仓库里逐个 git diff 太慢）标出改动（M / U / D）；增删行数 `added` / `deleted` 不受 `diff: false` 影响，已跟踪文件一次 `git diff HEAD --numstat` 算完，新文件数行数（二进制、超过 200KB 给 null），「改动」视图里每个文件右边和顶上合计显示 +N −M，打开文件时再用 `paths` 取这一个文件的 diff；git 状态回来前不要拿会话里工具记的片段当 diff，「改动」视图只列改过的文件，打开后可在「文件 / 改动」之间切换看 diff。内容搜索走 `fs.grep`（Rust 里逐文件找，不依赖 rg），上传 `fs.upload`、下载和 PDF 用 `fs.read` 带 `raw: true`（都限 50MB），`fs.open`（系统程序打开 / 在文件管理器显示）只接受本机连接，前端只在本机主机上给入口。`fs.list` 在 Windows 上要去掉 canonicalize 加的 `\\?\` 前缀（`fsapi::strip_verbatim`）再拼子项路径，否则子项路径和根目录、git 路径对不上（面包屑出现「?」、git 标记丢失）；前端 `unverbatim` 兼容旧记录。大纲是 `app/src/outline.ts` 按扩展名用正则找，不靠语法树。Markdown 预览是 `FilePreview.tsx` 的 `MdPreview`：按顶层块渲染，每块前插 `<a class="md-ln" data-line>` 锚点，分栏滚动对齐和大纲跳转都按锚点插值；相对路径图片改成 `data-src` 再经 `fs.read` 取。「查看改动」不再是单独页面：`SessionActions.showChanges` 发 `OPEN_CHANGES`，会话页打开文件面板切到「改动」，「只看这个会话改的」在 `FilePanel.tsx` 里；面板里暂时不提供提交 / 撤销（yi-agentd 的 `git.commit`、`git.discard` 保留）。浏览器标签在 `Browser.tsx`，标签键是 `web:<地址>`（`web:file:<绝对路径>` 是主机上的 HTML），作为普通标签存在 tabs 里；远程 localhost 地址先 `ports.forward`。HTML 预览走 `ClientHub::serve_preview`：`preview.root` 给一个目录换随机令牌，Tauri 自定义协议 `ahfs`、yi-web 的 `/fsv/<令牌>/<相对路径>` 都映射到 `fs.read` raw，只能读令牌对应目录（拒绝 ..）。预览页的 iframe 不带 allow-same-origin，外部网址不允许和应用同源，避免网页调到 `hub_call`。Tauri 窗口关了 `dragDropEnabled`，HTML 的拖放才能拿到系统文件。编辑器是 CodeMirror 6（`CodeEditor.tsx`），语言包由 `@codemirror/language-data` 按文件名懒加载，高亮颜色是 `.file-panel` 上的 `--hl-*` 变量。筛选框走 `fs.search`，图片用 `fs.read` 带 `image: true` 取 base64。改文件走 `fs.write`：保存时带上读取时的 `modified`，文件期间被 agent 改过就拒绝覆盖，由用户选读取磁盘版本或覆盖；`fs.delete` 只删文件和空文件夹。回合结束时刷新文件树，打开的文件没改过就重新读。打开的标签和当前文件存在 localStorage `wp-open-<主机>/<cwd>`，面板挂载拿到根目录后恢复（读不到的文件静默丢掉）。「添加到对话」发 `INSERT_COMPOSER` 事件，Composer 把 `@相对路径` 接到草稿末尾。
- 会话页右上角的终端按钮打开底部终端面板（`TermPanel.tsx`），可以开多个，用标签切换，拖上边缘调高度。面板开关按会话记（localStorage `term-panel-<主机>/<会话>`），不要改回全局开关。右侧文件面板的开关同样按会话记（`file-panel-<主机>::<会话>`）。每个终端是一个 shell 会话，`SessionInfo.parent_id` 指向对话会话：侧边栏、总览、命令面板都过滤掉带 `parentId` 的会话，`Hub::delete` 删对话时连带删掉它们。
- AI 回复中继续发消息（`session.send` 带 `queue: true`）：Claude、Codex 直接交给工具（Claude 写进 stdin，Codex 走 `turn/steer`），工具在下一次工具调用后读到，没赶上的回合结束后由工具自己接着处理；Codex 插话时回合刚好结束（steer 报错）就改成开新回合。Cursor、Kiro 和「回合结束要重启进程」时仍在 hub 里排队。
- AI 向用户提问走审批同一条路：`PermissionRequest` 带 `questions`，选项固定为 `answer` / `skip`（kind 不是 allow/reject，手机通知不出按钮），`session.permission` 带 `answers` 回答，`PermissionResolved` 记下答案。Claude 是 `can_use_tool` 里的 AskUserQuestion（回答填进 `updatedInput.answers`，键是问题原文）；Codex 是 `item/tool/requestUserInput`（只在 Codex 的 plan 协作模式下出现）；Cursor 是扩展请求 `cursor/ask_question`（只能选不能填；客户端不支持时它会退回逐题的 request_permission，是否给模型这个工具由 Cursor 服务端决定）；Kiro 没有提问工具，前端从最后一条回复里识别「编号问题 + a) b) 选项」，回答拼成 `1=a, 2=b` 作为消息发出。
- 回复显示风格（系统设置「对话外观」或会话页右上角的切换按钮，存 localStorage `chat-style`）：默认「简约」，相邻的思考和工具调用收成一组、标题一行概括，进行中展开；「Claude Code」是原来的圆点时间线。
- Cursor 的 ACP 不回传 token 用量和上下文：上下文占比在回合结束（和恢复会话）时读 `~/.cursor/chats/<md5(cwd)>/<会话 id>/store.db`，meta 里 latestRootBlobId 指向的根记录 5 号字段是 token_details {1 used, 2 max}；单轮 token 用量拿不到。
- 终端模式的编码工具重启时接着原来的会话：Claude 第一次启动就用 `--session-id` 指定 id（记成 `external_id`，没说过话就退出时下次还用这个 id 新开）；Codex、Cursor、Kiro 在 `session.resume` 前由 `Hub::guess_terminal_session` 从工具历史里找同目录、本会话创建后用过、没被别的会话占用的最新一个。
- Qoder（`qodercli --acp`）、CodeBuddy（`codebuddy --acp`）走 `acp.rs`，进 ACP 的参数见 `acp_arg`。两者都是 Claude Code 的仿制品：终端模式和 Claude 一样第一次就用 `--session-id` 定 id（`history::pins_session_id`），会话文件在 `~/.qoder/projects`、`~/.codebuddy/projects` 下的 `<id>.jsonl`（`tool_session_exists` 判断能不能 `--resume`）。Qoder 的 jsonl 就是 Claude 格式，历史列表和查看复用 `scan_claude` / `read_claude`；CodeBuddy 的是 `{type:"message", role, content:[{input_text|output_text}]}`，只解析列表（`scan_codebuddy`），内容靠 `session/load` 回放。Qoder 没有 plan 模式；它的审批里「本会话都允许」会把自己切到 acceptEdits，所以自动放行时优先选 `allow_once`。CodeBuddy 回合失败（比如没登录）不回 JSON-RPC error，而是 `stopReason: refusal`、错误在 `_meta["codebuddy.ai/errorMessage"]`（`meta_error`）；上下文占用走 ACP 标准的 `usage_update`（同时从 `cost.amount` 取积分），Qoder 从 jsonl 的 `context_usage_ratio` 取上下文占比。账号额度（`probe.rs`）：Qoder 走 `-p --no-session-persistence --input-format stream-json` 发 `get_usage_info`；CodeBuddy 走 ACP 的 `_codebuddy.ai/getUserInfo` 拿令牌再查 `get-user-resource-summary`。探测模型时 Qoder 建的空会话会进它的历史，探完用 `session/delete` 删掉。
- Android 的 `MainActivity` 用仓库里的 `app/src-tauri/android/MainActivity.kt`（`gen/android` 是生成的、不进仓库，CI 在 `tauri android init` 后拷过去）：全面屏下系统不再因输入法缩小窗口，由它按 IME inset 给内容区加底部 padding（减掉网页已用 safe-area 留的导航栏高度）。对话区 `ResizeObserver` 在变矮时保持贴底。返回键 / 返回手势由它交给网页的 `window.__yiConsoleBack()`（`app/src/back.ts`）：先关最上层用 `useBack(active, close)` 登记的弹窗、菜单、图片查看、文件 / 终端面板，再收起命令面板和左侧导航（抽屉，留在当前页），再 `goBack()`；退无可退时先提示「再按一次退出」，2 秒内再按网页才返回 false，App `finish()` 退出。新加会盖住页面的东西要用 `useBack` 登记。扫码配对不用原生插件（Tauri 官方 barcode-scanner 靠 Google Play 服务的 ML Kit，国内手机开相机就闪退），是 `QrScanner.tsx` 在网页里 `getUserMedia` + jsQR 自己识别；WebView 要相机时 wry 会向系统申请 CAMERA，清单里的权限由 `src-tauri/build.rs` 在 Android 构建时写进 `gen/android`。手机上输入框的「+」先选「相册图片」（`accept="image/*"`，系统打开相册）还是「文件」。
- 侧边栏宽度：`Sidebar.tsx` 的 `.side-resizer` 拖动，写到 `.sidebar` 的 `--side-w`（244–480px，localStorage `side-w`），只在桌面宽度生效。
- 工具徽标（`ToolBadge`）用各工具自己的 logo（Claude Code 用 claude.ai 的星芒，Codex 用 ChatGPT 的 OpenAI 花形），图形取自 `@lobehub/icons`（MIT），直接内联在 `Icon.tsx` 里。
- 历史会话页可以只看某个目录（`View.history.cwd`，含子目录），入口是侧边栏目录行的右键菜单。
- 主区域和终端容器用 `overflow: clip` 而不是 `hidden`：hidden 仍可被浏览器滚动，终端输入法的隐藏输入框在右边缘时会把整个会话区推到左边。
- 对话 / 终端模式切换走 `session.setMode`（`Hub::set_mode`）：先结束当前进程（作废代号，旧进程的退出事件不进会话；tmux 里的程序用 `tmux::kill_and_wait` 等它真正退出），再按新模式接着同一个 `external_id` 拉起。切到终端时把工具会话里已有的用户消息数记在 `SessionInfo.synced_users`；切回对话时只导入这之后的：Claude / Codex 读工具日志跳过前 N 条用户消息（`after_users`），Cursor / Kiro 让 ACP 回放并用 `StartSpec.replay_skip` 跳过。Cursor 的 ACP 会话（`~/.cursor/acp-sessions/<id>`）和终端界面的会话（`~/.cursor/chats/<md5(cwd)>/<id>`）是两份，切换时由 `history::cursor_handoff` 拷 store.db 过去。终端模式的 Codex 带 `--no-daemon`（`tools::codex_no_daemon` 先看 `--help` 支不支持），否则会话退出后还被 Codex 的共享后台服务占着，对话模式接续时报 "already has an active writer"。
- 终端模式的 Cursor 不传 `--model`（`pty.rs` 的 `interactive_args`）：对话模式（ACP）的模型 id 是 `grok-4.7[reasoning_effort=high,fast=true]` 这种带参数的，交互界面在 Windows 上经常报「Cannot use this model」，和 `--list-models` 的名字对应又要先跑一遍 cursor-agent（Windows 上约 8 秒）。不传时 Cursor 用 `~/.cursor/cli-config.json` 里记的上次选择。
- 只限本机的方法（`fs.open`、`proxy.setHosts`）靠 `serve_conn` 的 peer 判断：本机客户端起的是 `yi-agentd bridge --local`；SSH 起的 `yi-agentd bridge` 不带 `--local`，连上后先发一行 `BRIDGE_REMOTE_MARK`，守护进程把这条连接记成 `ssh`。标记行故意不带 id，旧版守护进程会当坏请求忽略。能 SSH 登录同一账号的人本来就能做任何事，这里防的是 Yi Console 客户端经 SSH 调到这些方法。
- 闲置释放在 `Hub::release_idle`（main 里每分钟跑一次），只处理对话模式、有工具会话 id、状态 idle 的会话。
- 页面里的 `<a href>` 点击由 `main.tsx` 的 `guardLink` 统一拦截：外部网址经 `openUrl` 打开（桌面用系统浏览器，手机 App 用 opener 插件的 `inAppBrowser`，即 Android Custom Tabs），相对路径等直接拦下，不许在应用窗口里跳转。链接上右键 / 长按是 `linkMenu`：打开、手机上「用其他浏览器打开」（`openUrlExternal`，调 MainActivity 注入的 `YiConsoleAndroid.openExternal` 弹系统选择框，只认 http/https）、复制链接。手机 capability 要有 `opener:allow-default-urls`，否则报 "Not allowed to open url"。组件要自己处理链接就 `preventDefault`，下载链接带 `download` 属性。
- 前端不要再用 `prompt` / `confirm` / 浏览器右键菜单：用 `components/Overlay.tsx` 的 `askText`、`askConfirm`、`openMenu`；会话相关操作统一放在 `components/SessionActions.ts`，会话页菜单和侧边栏右键共用。
- 终端回放历史时会吞掉里面的终端查询（`TerminalView` 里的 CSI 钩子），否则 xterm.js 的回答会被当成键盘输入打进 shell。
- 输入框里引用文件：文件面板的「复制」往剪贴板写相对路径，同时记在 `FilePanel.tsx` 的 `copiedRef`；Composer 粘贴时文字正好等于它就换成 `@相对路径`。系统文件（`clipboardData.files` / 拖放）里不是图片的走 `session.attachFile`（`hub::attach_file`，存到 `uploads/<会话>/files/`，文件名里的空格和非法字符换成 `_`，重名加序号），插入 `@绝对路径`。
- 图片由 yi-agentd 落盘到 `<base>/uploads/<会话 id>/`，事件里只带 `ImageRef`（id + mimeType），前端用 `session.image` 按需取。
- 回复正文里的图片（`![](…)`）：`util.ts` 的 `renderMarkdown` 不输出 `<img>`，而是 `<span class="md-rimg" data-kind data-src>` 占位，由 `chatImages.ts` 的 `hydrateImages` 填内容（`ChatView` 用 `ImgContext` 给主机、cwd）。主机上的文件（绝对路径、相对 cwd、`file://`）在本机主机直接经 `fs.read` 带 `image + strict` 读；远程主机先占位，点击后 `probe` 取大小、`askConfirm` 确认再下载。公网链接一律点了才由当前设备加载，打开对话不请求第三方。`strict` 按文件头只认图片，防止模型在回复里引用 `~/.ssh/id_rsa` 之类的文件被读出来。对话模式下 Claude（`--append-system-prompt`）和 Codex（`developerInstructions`，会覆盖 config.toml 的同名配置）带 `adapters::IMAGE_HINT`，告诉模型用绝对路径的 Markdown 图片给用户看图；ACP 工具没有系统提示词入口，不加。
- 经电脑转发（`yi-agentd/src/proxy.rs`）：桌面客户端把可共享的 SSH / 中继主机（`HostConfig.relay_share`，默认开）连同跳板机和自己的 known_hosts 同步给本机 yi-agentd（`proxy.setHosts`，只接受本机连接）。设备经中继连上电脑后用 `proxy.list` 发现它们，存成 `HostKind::Proxy`（id 为 `<中继主机 id>~<目标 id>`）；连接时新开一条中继连接，第一条消息发 `proxy.connect`，之后这条连接原样透传到目标 yi-agentd。转发主机不自动升级。
- 端口转发：yi-agentd 侧在 `crates/yi-agentd/src/ports.rs`（`ports.list` 列监听端口；`port.connect` 和 `proxy.connect` 一样是连接的第一条消息，之后字节原样转到 127.0.0.1:port，只连回环）。客户端侧在 `crates/client/src/forward.rs`（本机 127.0.0.1 开监听，每条连接调 `ClientHub::open_port`：SSH 用 direct-tcpip，失败退回 exec `yi-agentd bridge` + `port.connect`；中继 / 转发主机新开一条连接发 `port.connect`）。命令 `ports.forward` / `ports.unforward` / `ports.forwards`，变化时推 `ports.changed`；`url.open` 用系统浏览器打开网址。转发只在客户端内存里，不落盘。
- yi-agentd 只开本地 IPC，不监听 TCP。需要远程访问时，走 SSH bridge 或中继，不要给 yi-agentd 加网络端口。
- 中继（设计见 `docs/relay-security-v2.md`）：协议都在 `crates/link`（`auth.rs` 中继层挑战签名、`e2e.rs` Noise IK / IKpsk1、`wire.rs` 连接和控制消息），中继服务端 `crates/relay`，yi-agentd 侧 `crates/yi-agentd/src/relay.rs`。URL 里不放任何密钥，鉴权都在 WebSocket 升级后的第一组消息里。三种链接：`yiconsole://relay?v=2`（接入链接，带一次性邀请码，给电脑登记中继）、`yiconsole://pair?v=2`（配对二维码，设备公钥 + 一次性配对码）、`yiconsole://paired?v=2`（客户端配对后保存在 `HostConfig.pairing` 里的凭据，含客户端私钥）。`hosts.save` 收到配对链接时先 `client_pair` 再存凭据。改握手或消息格式要同时改三端，旧版本不兼容时链接里的 `v` 要跟着升。
- 中继鉴权失败分两类（`relay/src/hub.rs` 的 `deny` / `refuse`）：签名错、格式错、猜邀请码或配对票据计入单 IP 拉黑；身份已证明只是没权限（被移除、设备离线）不计入，客户端遇到这类拒绝（`fatal_code` 里的 4403 / 4410）也停止自动重连。
- 发给前端的主机配置要先经过 `redact()`，把密码、私钥替换成 `********`。保存时如果收到的还是 `********`，就保留原值。
- 本机守护进程一律从 `~/.yi-console/bin/yi-agentd` 运行（`crates/client/src/localbin.rs` 负责同步），不要直接运行安装目录里的 sidecar，也不要把安装目录里的路径注册成自启服务。
- 后端对同一连接的请求是并发处理的，需要保证顺序的输入（例如终端按键）要在前端排队发送，参考 `TerminalView.tsx`。
- 注释和守护进程的错误信息用中文。界面文案中英双语：`app/src/i18n.ts` 的 `tr(中文, English)`，语言存在本机 localStorage `ui-lang`（跟随系统 / 简体中文 / English），在系统设置里切换；`App` 里的 `useLang()` 会让整棵界面重绘。新增用户能看见的字符串要两种语言都写，不要只写中文。`useMemo` 里拼出来的文案要把 `useLang()` 的 lang 放进依赖，否则切换语言不更新。

## 测试

```bash
cargo test
cargo build && (cd app && npm run build)
./target/debug/yi-web --listen 127.0.0.1:3001 --dist app/dist --token dev --yi-agentd-dir ./target/debug
```

做端到端测试时，设置 `YI_CONSOLE_HOME=<临时目录>`，让 yi-agentd 跑在隔离的环境里，不要动 `~/.yi-console`。

Tauri 和 Android 只在 CI（`.github/workflows/build.yml`）里构建。本地机器缺 webkit 依赖、内存也不够时，不要强行编译。
