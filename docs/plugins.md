# 编写 Bot 插件

English: [Writing Bot plugins](plugins.en.md)

Bot 插件负责把外部事件（一条聊天消息、一次 GitHub @）带给 Bot，把 Bot 的回复发回去，也可以给 Bot 提供工具。插件是一个 WebAssembly 组件，在桌面 App 内的沙箱里运行，在 **插件** 页面通过 `.yiplugin` 插件包安装。GitHub 和飞书插件随桌面 App 一起提供、一起更新，不用另外安装。

[`plugins/`](../plugins) 里的官方插件都是完整的例子：`github`（轮询 REST API）、`feishu`（WebSocket 长连接）和 `testkit`（端到端测试用的插件）。

## 插件能做什么

插件在 manifest 里声明能力，并实现对应的方法：

| 能力 | 方法 | 含义 |
| --- | --- | --- |
| `trigger` | `start`、`on_timer`、`on_ws` → `emit` | 给 Bot 带来事件 |
| `reply` | `reply` | 在事件所在的会话里回复 |
| `context` | `read_context` | 让 Bot 读取会话里之前的消息 |
| `notify` | `notify` | 可以被选为 Bot 的提醒渠道 |
| `tools` | `tools`、`call_tool` | Bot 可以调用的工具（`plugin_tools` / `plugin_call`） |

每个插件可以添加多份配置，比如几个不同的定时、几个飞书机器人、几个 GitHub 账号，各自独立运行（开发文档里把每份配置叫作*实例*）。Bot 在自己的设置里勾选用哪几份。

## 上手

需要 Rust 和 `wasm32-wasip2` target（`rustup target add wasm32-wasip2`）。

在 `plugins/` 工作区里加一个 crate，或者在自己的 crate 里依赖 SDK：

```toml
[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
yi-plugin = { path = "../sdk" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

```rust
use yi_plugin::{emit, host, Event, Plugin, SendResult};

struct Hello;

impl Plugin for Hello {
    fn start() -> Result<(), String> {
        host::set_timer(60_000, true); // 每分钟一次
        Ok(())
    }

    fn on_timer(_id: u32) {
        emit(Event::new("tick-1", "hello:1", "早上好").addressed(true));
    }

    fn reply(thread: &str, _reply_to: Option<&str>, key: &str, text: &str) -> Result<SendResult, String> {
        yi_plugin::once(key, || {
            yi_plugin::log(&format!("reply to {thread}: {text}"));
            Ok(SendResult::Sent)
        })
    }
}

yi_plugin::export!(Hello);
```

编译并打包：

```bash
cd plugins
cargo build --release --target wasm32-wasip2 -p my-plugin
cargo run -p yi-plugin-pack -- my-plugin      # 生成 dist/<id>/ 和 dist/<id>-<version>.yiplugin
```

然后在桌面 App 里打开 **插件 → 安装插件**：选择 `.yiplugin` 文件；开发时也可以直接填 `dist/<id>` 目录的路径。

## 让 AI 来写

在 Bot 会话里告诉它需要接哪个服务。Bot 自己不写代码，它会派一个任务去写：任务调用 `plugin_kit`，在任务所在主机上生成一个 cargo 工作区（SDK、WIT、示例插件和这份指南），在里面写插件、编译成 `wasm32-wasip2`。任务做完后，Bot 检查插件申请的网络和命令权限，再用 `propose_plugin` 提交插件包或插件目录。App 会打开插件页的权限确认，由你决定是否安装；Bot 和任务都不能自己安装插件。

## plugin.json

```json
{
  "id": "hello",
  "version": "0.1.0",
  "api": "0.1",
  "runtime": "wasm",
  "main": "plugin.wasm",
  "name": { "zh": "示例", "en": "Hello" },
  "description": { "zh": "…", "en": "…" },
  "capabilities": ["trigger", "reply"],
  "permissions": { "net": ["api.example.com", "*.example.net"], "exec": [] },
  "config": {
    "type": "object",
    "required": ["token"],
    "properties": {
      "token": { "type": "string", "title": "令牌", "x-secret": true, "x-en": { "title": "Token" } }
    }
  }
}
```

- `id`：小写字母、数字和 `-`，最长 40 个字符。内置插件的 id（`cron`、`session-events`、`app-notify`、`app`、`builtin`）不能用。
- `api`：接口版本，目前是 `0.1`。
- `permissions.net`：插件可以通过 HTTP(S) 和 WebSocket 访问的主机名。`*.x` 匹配 `x` 的子域名（不含 `x` 本身）。其他地址都访问不了。
- `permissions.exec`：插件可以在这台电脑上运行的命令。命令在沙箱之外运行，安装确认时会醒目提示；除非插件确实需要调用本机程序，否则不要用。
- `config`：实例配置的 JSON Schema，会渲染成表单。`title` / `description` 写中文，`x-en` 里写英文。`x-secret: true` 的字段保存后不再显示。枚举可以配标签：`"x-labels": {"a": "甲"}`，英文写在 `"x-en": {"x-labels": {"a": "A"}}`。字符串数组按每行一项编辑。
- 报错和健康状态信息（`Err(..)`、`host::health`）用英文：它们会作为工具结果交给 Bot 的 AI，也会原样显示在 App 里，不经翻译。
- 与 manifest 同目录的 `prompt.md`（可选）：一段简短的英文说明，告诉 Bot 怎么理解这个来源的事件（会话格式、什么时候回复），会放进 Bot 的系统提示词。

安装时 App 会列出插件要的权限（更新时标出新增的权限），用户确认之前不会写入任何文件。

## 宿主接口

完整接口见 [`plugins/wit/yi-plugin.wit`](../plugins/wit/yi-plugin.wit)，SDK 做了封装：

| SDK | |
| --- | --- |
| `yi_plugin::config::<T>()` | 实例配置（含密钥） |
| `emit(Event::new(key, thread, text)...)` | 发出事件。`key` 每个事件唯一（重复的会被忽略），`thread` 把同一个会话归在一起；`.sender(id, name, bot)`，事件是冲着 Bot 来的（@、指派）时加 `.addressed(true)`，还有 `.url`、`.at`、`.meta` |
| `http::get(url).bearer(t).send()?` | 经宿主发 HTTP；`.json(v)`、`.header`、`.body`；返回 `Response { status, headers, body }`，`.expect_json()` |
| `host::ws_open(url, headers_json)` | 打开 WebSocket；事件通过 `on_ws(id, kind, data)` 送达，kind 为 `open` / `text` / `binary` / `close` / `error` |
| `host::set_timer(ms, repeat)` / `clear_timer` | 定时器，到点调用 `on_timer(id)` |
| `kv_get` / `kv_set` / `kv_del` | 每个实例一份的小型持久化键值存储（游标、缓存） |
| `once(key, send)` | 同一个 key 最多发送一次 |
| `host::health(state, detail)` | 在插件页显示 `ok` / `error` 和原因 |
| `log` / `warn` | 日志 |
| `host::exec(cmd, args, stdin)` | 运行 `permissions.exec` 里列出的命令 |

### 规则

- 每次调用插件，插件自己的计算时间不能超过 5 秒（等 HTTP 的时间不算），内存上限 64MB。超限或 panic 的插件会被重启，重启间隔逐步加长，插件页会显示为异常。
- `reply` / `notify` 带有幂等 key，失败后可能重试：同一个 key 最多发送一次（`once` 就是做这个的）。会话已经不能再发消息时返回 `SendResult::Gone`，App 就不再重试。
- 来自机器人账号的事件用 `.sender(id, name, true)` 标出来，这类事件永远不会启动任务。
- 工具：在 `tools()` 里用 `yi_plugin::tool(name, description, input_schema, read_only)` 返回。不改动任何东西的工具标为只读；Bot 不在自动模式时只能运行只读工具。
- 纯逻辑在本机用 `cargo test` 测：非 wasm 目标上宿主函数是桩函数，所以像官方插件那样把解析、格式化写成纯函数。
