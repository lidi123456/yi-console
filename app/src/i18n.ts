import { useEffect, useState } from 'react';
import i18next from 'i18next';
import { lsGet, lsSet } from './util';
import { resources } from './locales';

// UI language: follow system / Chinese / English, stored in local localStorage.

export type LangPref = 'auto' | 'zh' | 'en';
export type Lang = 'zh' | 'en';

const KEY = 'ui-lang';
const EVENT = 'ah:lang';

export function getLangPref(): LangPref {
  return lsGet<LangPref>(KEY, 'auto');
}

export function getLang(): Lang {
  const p = getLangPref();
  if (p === 'zh' || p === 'en') return p;
  const sys = typeof navigator !== 'undefined' ? navigator.language || '' : 'zh';
  return /^zh/i.test(sys) ? 'zh' : 'en';
}

export function isEn(): boolean {
  return getLang() === 'en';
}

// Resources are bundled, so init and changeLanguage complete synchronously
i18next.init({
  resources,
  lng: getLang(),
  fallbackLng: 'zh',
  ns: Object.keys(resources.zh),
  defaultNS: false,
  initAsync: false,
  interpolation: { escapeValue: false }, // React escapes already
});

/** Translator for one namespace (one JSON file per language), usable outside components:
 *    const t = getT('palette');  t('host.files', { name })
 *  Keys and {{placeholders}} are type-checked against the Chinese file (see locales/i18next.d.ts). The language is
 *  resolved on every call, so a module-level t follows language changes. */
export function getT<N extends keyof typeof resources.zh>(ns: N) {
  return i18next.getFixedT(null, ns);
}

function apply() {
  if (i18next.language !== getLang()) i18next.changeLanguage(getLang());
  if (typeof document === 'undefined') return;
  document.documentElement.lang = isEn() ? 'en' : 'zh-CN';
}

export function setLangPref(p: LangPref) {
  lsSet(KEY, p);
  apply();
  window.dispatchEvent(new Event(EVENT));
}

export function initLang() {
  apply();
  if (typeof window !== 'undefined') {
    window.addEventListener('languagechange', () => {
      apply();
      window.dispatchEvent(new Event(EVENT));
    });
  }
}

/** Re-render when the language setting changes; returns [setting, effective language] */
export function useLang(): [LangPref, Lang] {
  const [, bump] = useState(0);
  useEffect(() => {
    const f = () => bump((n) => n + 1);
    window.addEventListener(EVENT, f);
    return () => window.removeEventListener(EVENT, f);
  }, []);
  return [getLangPref(), getLang()];
}

export function onLangChange(f: () => void): () => void {
  window.addEventListener(EVENT, f);
  return () => window.removeEventListener(EVENT, f);
}

const ERR_PATTERNS: [RegExp, string | ((...m: string[]) => string)][] = [
  [/^未找到可用的基准分支$/, 'No available base branch found'],
  [/^找不到分支或提交[:：]\s*(.+)$/, (_, r) => `Branch or commit not found: ${r}`],
  [/^两个分支没有共同祖先$/, 'These two branches have no common ancestor'],
  [/^找不到提交[:：]\s*(.+)$/, (_, r) => `Commit not found: ${r}`],
  [/^范围表达式无效[:：]\s*(.+)$/, (_, r) => `Invalid range expression: ${r}`],
  [/^不支持的审阅范围模式[:：]\s*(.+)$/, (_, m) => `Unsupported review scope mode: ${m}`],
  [/快照引用无效[:：]?\s*(.*)/, (_, r) => (r ? `Invalid checkpoint ref: ${r}` : 'Invalid checkpoint ref')],
  [/这个会话不支持调整模型参数/, 'This session does not support model parameters'],
  [/^ChatGPT 登录已失效（令牌过期且无法刷新），请点击「重新登录」$/, 'The ChatGPT login has expired and its token can no longer be refreshed. Click Re-authenticate'],
  [/^子 agent 在等待审批/, "A subagent is waiting for approval, but agy doesn't pass subagent approval requests on to Yi Console. Stop it in the background tasks, switch to Bypass All and try again, or handle it in terminal mode"],
  [/该后台任务不支持从外部停止/, 'This background task cannot be stopped from outside'],
  [/当前模型不支持这个参数/, 'The current model does not support this parameter'],
  [/模型参数值无效/, 'Invalid model parameter value'],
  [/当前模型不支持调整思考等级/, 'The current model does not support thinking levels'],
  [/当前模型不支持这个思考等级/, 'The current model does not support this thinking level'],
  [/^切换模型失败，发下一条消息时会再试[:：]\s*(.+)$/s, (_, m) => `Could not switch models; it will be tried again with the next message: ${m}`],
  [/^切换模型失败，仍在使用 (\S+)（刚刷新出的模型可能要停止进程后再发消息才能用）[:：]\s*(.+)$/s, (_, cur, m) => `Could not switch models, still on ${cur} (a model that just appeared after a refresh may need the process stopped and a new message sent): ${m}`],
  [/^Cursor 没有模型 (.+)，改用 (.+)$/, (_, a, b) => `Cursor has no model ${a}; using ${b}`],
  [/^切换模型参数失败[:：]\s*(.+)$/, (_, m) => `Could not change model parameter: ${m}`],
  [/MCP 名称只能包含字母、数字、下划线和短横线，长度为 1–64/, 'MCP names must contain 1–64 ASCII letters, digits, underscores or hyphens'],
  [/MCP 名称 yi_computer_use 为内置操作电脑保留/, 'The MCP name yi_computer_use is reserved for Computer Use'],
  [/MCP 名称 yi_browser 为内置浏览器保留/, 'The MCP name yi_browser is reserved for AI Browser'],
  [/MCP 名称 yi_memory 为内置长期记忆保留/, 'The MCP name yi_memory is reserved for Personal Memory'],
  [/MCP 名称 yi_session 为内置会话读取保留/, 'The MCP name yi_session is reserved for built-in session reading'],
  [/记忆标识只能包含字母、数字、下划线和短横线，长度为 1–64（不能为 project）/, 'Memory id must be 1–64 ASCII letters, digits, underscores or hyphens (cannot be "project")'],
  [/记忆正文不能为空/, 'Memory content cannot be empty'],
  [/^记忆正文超过上限（最多 (\d+) KB）$/, (_, kb) => `Memory content exceeds the limit (max ${kb} KB)`],
  [/^当前作用域的记忆已达上限（最多 (\d+) 条），请先合并或删除旧记忆$/, (_, n) => `Memory limit reached for this scope (max ${n}); merge or delete older entries first`],
  [/^找不到记忆[:：]\s*(.+)$/, (_, id) => `Memory not found: ${id}`],
  [/^目标记忆标识已存在[:：]\s*(.+)$/, (_, id) => `Target memory id already exists: ${id}`],
  [/^目标位置已存在同名记忆[:：]\s*(.+)$/, (_, id) => `A memory with this id already exists in the target scope: ${id}`],
  [/没有找到对应的历史记录，可能已经撤销或过期/, 'History record not found; it may have already been undone or expired'],
  [/这条待确认记忆已不存在或已处理/, 'This pending memory proposal no longer exists or was already handled'],
  [/当前目录不是项目目录，请改存为全局记忆/, 'This directory is not a project directory; save it as a global memory instead'],
  [/当前目录不是项目目录，无法保存项目记忆/, 'This directory is not a project directory'],
  [/找不到对应的项目记忆目录/, 'Project memory folder not found'],
  [/项目标识无效/, 'Invalid project key'],
  [/MCP 名称重复：(.+)/, (_, name) => `Duplicate MCP name: ${name}`],
  [/MCP 只能分配给 AI 工具/, 'MCP servers can only be assigned to AI tools'],
  [/MCP stdio 服务缺少启动命令：(.+)/, (_, name) => `Missing MCP stdio command: ${name}`],
  [/MCP HTTP 地址必须以 http:\/\/ 或 https:\/\/ 开头：(.+)/, (_, name) => `MCP URL must start with http:// or https://: ${name}`],
  [/MCP 配置包含无效字符：(.+)/, (_, name) => `Invalid character in MCP configuration: ${name}`],
  [/MCP 环境变量或请求头无效：(.+)/, (_, name) => `Invalid MCP environment variable or header: ${name}`],
  [/MCP 密钥占位符没有原值，请重新填写：(.+)/, (_, name) => `MCP secret placeholder has no saved value; enter the value again: ${name}`],
  [/该工具未声明支持 HTTP MCP，请改用 stdio 或调整服务的工具范围/, 'This tool does not advertise HTTP MCP support; use stdio or change its tool scope'],
  [/桌面不可用，请在已登录且解锁的 Windows 用户会话中运行 Yi Console/, 'Desktop unavailable. Run Yi Console in the signed-in, unlocked Windows session'],
  [/未发现可操作窗口，请确认 daemon 运行在已登录的 Windows 桌面中/, 'No windows found. Run the daemon in the signed-in Windows desktop'],
  [/操作电脑首版仅支持 Windows x64/, 'Computer use currently supports Windows x64 only'],
  [/操作电脑仅支持 Windows x64 和 macOS/, 'Computer use supports Windows x64 and macOS only'],
  [/桌面不可用，请在这台 Mac 上登录桌面后再试/, 'Desktop unavailable. Sign in to this Mac and try again'],
  [/未发现可操作窗口，请确认 daemon 运行在已登录的桌面中/, 'No windows found. Run the daemon in the signed-in desktop session'],
  [/无法读取桌面窗口，请确认已登录并解锁桌面/, 'Cannot read desktop windows. Sign in and unlock the desktop'],
  [/无法截取桌面，请确认已登录并解锁桌面/, "Can't capture the desktop. Make sure you are signed in and the desktop is unlocked"],
  [/未找到 macOS 桌面驱动，请用 macOS 安装包重新安装 Yi Console/, 'The macOS desktop driver is missing. Reinstall Yi Console from the macOS installer'],
  [/解压 macOS 桌面驱动失败/, 'Could not unpack the macOS desktop driver'],
  [/无法启动 macOS 桌面驱动/, 'Could not start the macOS desktop driver'],
  [/桌面驱动 socket 路径过长[:：]\s*(.+)$/, (_, p) => `Desktop driver socket path is too long: ${p}`],
  [/无法打开系统设置/, 'Could not open System Settings'],
  [/未知的系统设置页面/, 'Unknown System Settings pane'],
  [/只有 macOS 需要在系统设置里授权/, 'Only macOS needs grants in System Settings'],
  [/请先停止当前回复，再开启操作电脑/, 'Stop the current reply before enabling computer use'],
  [/当前会话未开启操作电脑/, 'Computer use is off for this session'],
  [/操作电脑已关闭，请由用户重新开启/, 'Computer use is off. Ask the user to enable it again'],
  [/连不上守护进程，请重新开启操作电脑/, "Can't reach the daemon. Turn computer use off and on again"],
  [/操作电脑只接受 AI 工具自己启动的桌面代理，请直接调用 yi_computer_use 工具/, 'Computer use only accepts the desktop proxy started by the AI tool. Call the yi_computer_use tools directly'],
  [/无法确认桌面代理进程/, "Can't identify the desktop proxy process"],
  [/操作电脑连接已失效，请重连会话/, 'The desktop connection has expired. Reconnect the session'],
  [/操作电脑已暂停，用户发下一条消息后恢复/, 'Computer use is paused; it resumes with the user\'s next message'],
  [/只读规划模式不能操作桌面输入/, 'Read-only planning mode cannot send desktop input'],
  [/操作电脑仅支持 AI 工具的聊天模式/, 'Computer use requires an AI chat session'],
  [/MCP 名称 yi_browser 为内置浏览器保留/, 'The MCP name yi_browser is reserved for the AI browser'],
  [/没有找到 Chrome、Edge 或 Chromium。请在这台主机上安装 Chrome，或在主机设置里指定浏览器路径/, 'No Chrome, Edge or Chromium found. Install Chrome on this host, or set the browser path in the host settings'],
  [/请先停止当前回复，再开启浏览器/, 'Stop the current reply before turning on the AI browser'],
  [/当前会话未开启浏览器/, 'The AI browser is off for this session'],
  [/浏览器仅支持 AI 工具的聊天模式/, 'The AI browser requires an AI chat session'],
  [/浏览器代理只接受本机连接/, 'The browser proxy accepts local connections only'],
  [/连不上守护进程，请重新开启浏览器/, "Can't reach the daemon. Turn the AI browser off and on again"],
  [/未知的浏览器控制命令/, 'Unknown AI browser command'],
  [/无法启动浏览器 (.+)：(.+)/, "Can't start the browser $1: $2"],
  [/浏览器启动超时，没有写出调试端口/, 'The browser did not start in time'],
  [/浏览器资料目录正被另一个浏览器进程使用/, 'The browser profile is in use by another browser process'],
  [/浏览器启动后退出了（(.+?)）/, 'The browser exited right after starting ($1)'],
  [/界面元素已失效，请先重新读取窗口/, 'The element is stale. Read the window again'],
  [/界面截图已失效，请先重新读取窗口/, 'The screenshot is stale. Read the window again'],
  [/坐标操作前请先读取窗口截图/, 'Read the window screenshot before using coordinates'],
  [/该桌面工具不在当前会话允许范围内/, 'This desktop tool is not available to the current session'],
  [/截图只能保存在当前会话的产物目录/, 'Screenshots can only be saved as artifacts of the current session'],
  [/无法读取桌面窗口，请确认已登录并解锁 Windows 桌面/, 'Cannot read desktop windows. Sign in to Windows and unlock the desktop'],
  [/无法截取桌面，请确认 Windows 已登录并解锁/, "Can't capture the desktop. Make sure Windows is signed in and unlocked"],
  [/内置桌面驱动校验失败/, 'Bundled desktop driver verification failed'],
  [/桌面驱动启动超时/, 'The desktop driver took too long to start'],
  [/桌面控制状态已变化，请重新读取窗口/, 'Desktop control has changed. Read the window again'],
  [/桌面操作已中断，结果待核验，请重新读取窗口/, 'Desktop action interrupted. Read the window to verify its result'],
  [/桌面操作超时，结果待核验，请读取窗口后再继续/, 'Desktop action timed out. Read the window to verify its result before continuing'],
  [/桌面驱动连接已断开，请重新读取窗口/, 'Desktop driver disconnected. Read the window again'],
  [/主机上另一个桌面任务「(.+)」正在运行，请稍后再试/, (_, title) => `Another desktop task "${title}" is running on that host; try again once it finishes`],
  [/远端电脑缺少桌面驱动/, 'The remote host does not have the desktop driver'],
  [/桌面不可用，请先在远端电脑上登录并解锁桌面/, 'Desktop unavailable. Sign in and unlock the desktop on the remote host first'],
  [/请先在远端 Mac 的系统设置中为 Cua Driver 开启辅助功能和屏幕录制权限/, 'Grant Accessibility and Screen Recording to Cua Driver in System Settings on the remote Mac first'],
  [/只有等待你处理的任务才能继续/, 'Only a task waiting on you can be continued'],
  [/任务会话不存在/, 'Task session not found'],
  [/任务已结束/, 'The task has ended'],

  // Accounts and same-session failover
  [/^Cursor 启动超时（60 秒未创建会话），请检查账号登录状态和网络后重试$/, 'Cursor did not create a session within 60 seconds. Check your account sign-in and network, then try again'],
  [/所选账号不存在|目标账号不存在/, 'Selected account was not found'],
  [/^账号 (.+) 不存在$/, (_, id) => `Account "${id}" was not found`],
  [/请先停止当前回复，再切换账号/, 'Stop the current reply before switching accounts'],
  [/仅 AI 会话支持切换账号/, 'Only AI sessions support switching accounts'],
  [/该工具.*不支持多账号/, 'This tool does not support multi-account management'],
  [/当前没有可抓取的登录凭据/, 'No native credentials were found for this tool to capture'],
  [/已有会话正在运行.*切换/, 'Another session is currently running for this tool; stop it before switching the host-active account'],
  [/正在运行.*无法.*切换/, 'A session is currently running; stop it before switching accounts'],
  [/凭据包格式无效/, 'Invalid account credential bundle'],
  [/缺少工具或凭据/, 'Missing tool or credential payload'],
  [/无法切换该账号凭据以探测额度/, 'Could not activate this account to probe its quota'],
  [/当前有活跃会话正在运行，已跳过切换探测/, 'An active session is running; skipped switching account to probe quota'],
  [/^自动切换备用账号失败[:：]\s*(.+)$/, (_, m) => `Auto failover to backup account failed: ${m}`],
  [/^切换备用账号后自动继续失败[:：]\s*(.+)$/, (_, m) => `Auto-continue after account failover failed: ${m}`],
  [/登录超时/, 'Account sign-in timed out'],
  [/正在登录中/, 'Another sign-in flow is already running for this tool'],
  [/CURSOR_API_KEY 无效或没有权限/, 'CURSOR_API_KEY is invalid or lacks permission'],
  [/^~\/\.codex\/config\.toml 没有选择带 env_key 的自定义 provider，无法按 API Key 抓取$/, "~/.codex/config.toml doesn't select a custom provider with an env_key, so there's no API key to capture"],
  [/^环境变量 (\S+) 未设置，无法抓取 provider (.+) 的 API Key$/, (_, e, p) => `The environment variable ${e} isn't set, so the API key for provider ${p} can't be captured`],
  [/^环境变量 (\S+) 未设置，无法转为 API Key 账号$/, (_, e) => `The environment variable ${e} isn't set, so the account can't be converted to an API key account`],
  [/^这个账号已经是 API Key 账号$/, 'This account is already an API key account'],
  [/^这个账号的 config\.toml 没有选择自定义 provider，它使用的就是 ChatGPT 授权$/, "This account's config.toml selects no custom provider; it already uses the ChatGPT login"],
  [/^provider (.+) 没有配置 env_key，无法转为 API Key 账号$/, (_, p) => `Provider ${p} has no env_key, so the account can't be converted to an API key account`],
  [/^这个 API Key 账号没有保存过 ChatGPT 授权，请用「重新登录」或抓取当前登录$/, 'This API key account has no saved ChatGPT login; use "Re-authenticate" or capture the current login'],
  [/^这个账号已经在使用 ChatGPT 授权$/, 'This account already uses the ChatGPT login'],
  [/^只有 Codex 账号可以转换$/, 'Only Codex accounts can be converted'],
  [/^压缩上下文失败：Cursor 没有返回摘要，原会话保持不变$/, 'Compaction failed: Cursor returned no summary, the session is unchanged'],
  [/^(\S+) 没有返回模型列表$/, (_, tool) => `${tool} returned no model list`],
  [/^Codex 读取模型列表失败: (.*)$/s, (_, m) => `Codex could not read its model list: ${m}`],
  [/^(.+) 是系统级单活账号，切换为当前账号后才能读取它的模型列表$/, (_, a) => `${a} is a system-wide account; make it the current account to read its model list`],
  [/^读取模型列表期间切换了当前账号，结果已丢弃，请重试$/, 'The current account changed while reading the model list; the result was discarded, please try again'],
  [/^(\S+) 正在切换账号或登录，完成后再读取模型列表$/, (_, t) => `${t} is switching accounts or signing in; read the model list once that is done`],
  [/^Cursor 返回的模型列表不完整（(\d+) 个，原来有 (\d+) 个），已保留原来的列表，请稍后再试$/, (_, n, m) => `Cursor returned an incomplete model list (${n}, there were ${m}); the previous list was kept, please try again later`],
  [/^这个工具没有模型可选$/, 'This tool has no models to choose from'],
  [/^压缩上下文时创建新会话失败: (.*)$/, (_, m) => `Creating the new session for compaction failed: ${m}`],
  [/^CURSOR_API_KEY 校验失败（HTTP (\d+)）$/, (_, c) => `CURSOR_API_KEY validation failed (HTTP ${c})`],
  [/CURSOR_API_KEY 交换访问令牌失败/, 'Failed to exchange CURSOR_API_KEY for an access token'],
  [/当前系统 Cursor auth\.json 为 CURSOR_API_KEY 凭据而非 OAuth 登录，请重新登录 OAuth/, 'System Cursor auth.json holds a CURSOR_API_KEY credential instead of an OAuth login; sign in with OAuth again'],
  [/当前 Cursor 授权账号缺少登录快照，请在账号管理中点击「重新登录」完成网页授权/, 'This Cursor OAuth account has no saved login snapshot; click "Re-authenticate" in Accounts to complete web sign-in'],
  [/当前 Cursor 授权账号保存的凭据实际为 API Key 而非网页授权登录，请在账号管理中点击「重新登录」完成授权/, 'This Cursor OAuth account holds an API Key credential instead of a web OAuth sign-in; click "Re-authenticate" in Accounts to complete sign-in'],
  [/已保存凭据实际为 CURSOR_API_KEY 而非网页授权登录，请点击「重新登录」完成授权/, 'Saved credential is a CURSOR_API_KEY rather than a web OAuth sign-in; click "Re-authenticate" to sign in'],
  [/当前系统未检测到网页授权登录凭据（本地保存的是 CURSOR_API_KEY 凭据），请点击「OAuth \/ CLI 登录」完成授权登录/, 'No local web OAuth login detected (local storage holds a CURSOR_API_KEY credential); click "OAuth / CLI Sign-In" to authenticate'],
  [/当前系统未检测到 CURSOR_API_KEY 环境变量或 API Key 凭据/, 'No CURSOR_API_KEY environment variable or API Key credential detected on this host'],
  [/登录进程未生成网页授权凭据，请重试/, 'Sign-in did not produce a web OAuth credential; please try again'],
  [/宿主环境变量 CURSOR_API_KEY 优先级高于本地 OAuth 登录；托管 OAuth 账号会自动隔离该变量/, 'Host CURSOR_API_KEY overrides local OAuth login; managed OAuth accounts isolate it automatically'],
  [/宿主环境变量 CURSOR_API_KEY 优先级高于本地 OAuth 登录；切换到授权登录账号时会自动屏蔽该变量/, 'Host CURSOR_API_KEY overrides local OAuth login; switching to an OAuth account isolates it automatically'],
  [/使用宿主环境变量 CURSOR_API_KEY；切换到授权登录账号时会自动屏蔽该变量/, 'Using host environment variable CURSOR_API_KEY; switching to an OAuth account isolates it automatically'],
  [/使用宿主环境变量 CURSOR_API_KEY/, 'Using host environment variable CURSOR_API_KEY'],
  [/auth\.json 中保存的是 CURSOR_API_KEY 凭据/, 'auth.json holds a CURSOR_API_KEY credential'],
  [/未检测到系统 Cursor 登录凭据或 CURSOR_API_KEY/, 'No native Cursor login credentials or CURSOR_API_KEY detected'],
  // Sessions and general errors
  [/^未知方法[:：]\s*(.+)$/, (_, m) => `Unknown method: ${m}`],
  [/未知方法/, 'Unknown method'],
  [/文件在这期间被修改过，请刷新后再试/, 'File was modified on disk, please reload and try again'],
  [/文件夹不是空的，请先删除里面的内容/, 'Folder is not empty; delete its contents first'],
  [/不能删除系统根目录或主目录/, 'Cannot delete the filesystem root or home directory'],
  [/不能删除 Yi Console 数据目录/, 'Cannot delete the Yi Console data directory'],
  [/文件已存在|已经存在/, 'File already exists'],
  [/会话不存在/, 'Session not found'],
  [/主机未连接/, 'Host not connected'],
  [/连接已断开/, 'Connection closed'],
  [/无法连接 Yi Console 服务/, 'Cannot connect to Yi Console service'],
  [/认证失败/, 'Authentication failed'],
  [/权限不足|拒绝访问/, 'Permission denied'],
  [/超时/, 'Timed out'],
  [/这个会话没有在等待额度恢复/, 'This session is not waiting for a usage limit to reset'],
  [/^自动继续失败[:：]\s*(.+)$/, (_, m) => `Automatic continue failed: ${m}`],
  [/^未知语言[:：]\s*(.+)$/, (_, m) => `Unknown language: ${m}`],
  [/^未知操作[:：]\s*(.+)$/, (_, m) => `Unknown action: ${m}`],
  // Queued messages (hub.rs queue_action)
  [/这条消息已经发出去了/, 'That message has already been sent'],
  [/排队消息里的图片已经不在了/, 'An image in the queued message is gone'],
  [/^消息不能为空$/, 'The message is empty'],
  [/这个会话没有被重启打断的回合/, 'This session has no turn cut off by a restart'],
  [/^发送排队消息失败[:：]\s*(.+)$/, (_, m) => `Could not send the queued message: ${m}`],
  // App self-update (updater.rs / MainActivity.kt)
  [/^检查更新失败[:：]\s*(.+)$/, (_, m) => `Update check failed: ${m}`],
  [/^安装更新失败[:：]\s*(.+)$/, (_, m) => `Update install failed: ${m}`],
  [/^下载失败[:：]\s*(.+)$/, (_, m) => `Download failed: ${m}`],
  [/^无法打开安装程序[:：]\s*(.+)$/, (_, m) => `Cannot open the installer: ${m}`],
  [/正在安装更新/, 'An update is being installed'],
  [/没有可安装的更新|请先检查更新/, 'No update to install, check for updates first'],
  [/没有找到发布的版本/, 'No published release found'],
  [/下载的安装包校验失败/, 'The downloaded package failed verification, please try again'],
  [/已经是最新版本/, 'Already up to date'],
  [/这种安装方式不支持自动更新/, 'This kind of install cannot update itself; download the new version from the releases page'],
  // Tool upgrades (toolupdate.rs)
  [/^有 (\d+) 个 (\S+) 会话正在使用，等它们结束后再升级$/, (_, n, t) => `${n} ${t} session(s) are in use; upgrade once they finish`],
  [/另一个工具正在升级，等它完成后再试/, 'Another tool is being upgraded; try again when it finishes'],
  [/^这台机器上没有安装 (.+)$/, (_, t) => `${t} is not installed on this host`],
  [/这个工具不能升级/, 'This tool cannot be upgraded'],
  [/^升级命令超过 (\d+) 分钟没有结束，已停止$/, (_, m) => `The update command was stopped after running for ${m} minutes`],
  [/^启动升级命令失败[:：]\s*(.+)$/, (_, m) => `Could not start the update command: ${m}`],
  [/^查询最新版本失败[:：]\s*(.+)$/, (_, m) => `Could not look up the latest version: ${m}`],
  // Side chats (hub.rs)
  [/旁路对话仅支持 AI 工具的聊天模式/, 'Side chats are only supported in structured chat mode for AI tools'],
  [/旁路对话缺少所属主会话/, 'Side chat is missing its parent session'],
  [/所属主会话不存在/, 'Parent session no longer exists'],
  [/不能在旁路对话内嵌套创建旁路对话/, 'Cannot start a side chat inside another side chat'],
  // Session-bound worktrees (hub.rs)
  [/^只有顶层 AI 对话会话支持独立 Worktree$/, 'Only top-level AI chat sessions support isolated worktrees'],
  [/^只有顶层 AI 对话会话支持切入独立 Worktree$/, 'Only top-level AI chat sessions can move into an isolated worktree'],
  [/^当前会话已在独立 Worktree 中$/, 'This session is already in an isolated worktree'],
  [/^会话正在回复中，请等当前轮次结束后再切入 Worktree$/, 'Wait for the current turn to finish before moving to a worktree'],
  [/^会话正在切入 Worktree，请稍候再发送$/, 'Moving session into worktree, please try again shortly'],
  [/^Worktree 切换进行中，请稍候$/, 'Worktree migration is already in progress'],
  [/^已切入独立 Worktree：(.+?)（基于 (.+?)）$/, (_, br, base) => `Moved to isolated worktree: ${br} (from ${base})`],
  [/^当前会话没有绑定独立 Worktree$/, 'This session is not bound to an isolated worktree'],
  [/^当前处于分离头指针状态，无法重命名分支$/, 'Cannot rename branch while HEAD is detached'],
  [/^目标分支已存在[:：]\s*(.+)$/, (_, b) => `Target branch already exists: ${b}`],
  [/^分支名与已有分支层级冲突[:：]\s*(.+)$/, (_, b) => `Branch name conflicts with an existing branch hierarchy: ${b}`],
  [/^找不到分支[:：]\s*(.+)$/, (_, b) => `Branch not found: ${b}`],
  [/^无法为分支找到可用名称[:：]\s*(.+)$/, (_, b) => `Could not find an available branch name for: ${b}`],
  [/^Worktree 分支已重命名：(.+?) → (.+)$/, (_, a, b) => `Worktree branch renamed: ${a} → ${b}`],
  // Git branch & worktree management (gitops.rs)
  [/^找不到目标分支[:：]\s*(.+)$/, (_, br) => `Target branch not found: ${br}`],
  [/^找不到分支或提交[:：]\s*(.+)$/, (_, r) => `Branch or commit not found: ${r}`],
  [/^分支名无效[:：]\s*(.+)$/, (_, br) => `Invalid branch name: ${br}`],
  [/^原分支名无效[:：]\s*(.+)$/, (_, br) => `Invalid source branch name: ${br}`],
  [/^新分支名无效[:：]\s*(.+)$/, (_, br) => `Invalid new branch name: ${br}`],
  [/^基准分支无效[:：]\s*(.+)$/, (_, br) => `Invalid base branch: ${br}`],
  [/^目标分支无效[:：]\s*(.+)$/, (_, br) => `Invalid target branch: ${br}`],
  [/^远程仓库名无效[:：]\s*(.+)$/, (_, r) => `Invalid remote name: ${r}`],
  [/^未配置远程仓库（remote）$/, 'No git remote configured'],
  [/^未找到对应的 worktree[:：]\s*(.+)$/, (_, p) => `Worktree not found: ${p}`],
  [/^不能删除主仓库目录$/, 'Cannot remove the main repository directory'],
  [/^不能删除 bare 仓库$/, 'Cannot remove a bare repository'],
  [/^当前处于分离头指针状态，请先切换到分支再推送$/, 'Detached HEAD: switch to a branch before pushing'],
  [/^当前处于分离头指针状态，请先切换到分支再拉取$/, 'Detached HEAD: switch to a branch before pulling'],
  [/^当前处于分离头指针状态，请先切换到分支再合并$/, 'Detached HEAD: switch to a branch before merging'],
  [/^当前处于分离头指针状态，无法变基$/, 'Detached HEAD: switch to a branch before rebasing'],
  [/^源分支与目标分支相同$/, 'Source and target branches are the same'],
  [/^目标分支未检出且主仓库有未提交改动，请先提交或清理$/, 'Target branch is not checked out and the main repository has uncommitted changes'],
  [/^目标分支目录有未提交改动，请先提交或清理后再合并$/, 'The target branch directory has uncommitted changes; commit or clean them first'],
  [/^当前工作区有未提交改动，请先提交或放弃后再变基$/, 'Working tree has uncommitted changes; commit or discard them before rebasing'],
  [/^合并存在冲突，已自动中止，请在终端手动解决冲突$/, 'Merge conflicts occurred; aborted cleanly so you can resolve them in the terminal'],
  [/^变基存在冲突，已自动中止，请在终端手动处理$/, 'Rebase conflicts occurred; aborted cleanly so you can resolve them in the terminal'],
  [/^分支 (.+?) 正被 Worktree 占用，请先切换或删除对应 Worktree$/, (_, br) => `Branch ${br} is checked out in a worktree; switch or remove that worktree first`],
];

/** Translate Chinese error messages from the backend or runtime (applies in the English UI) */
export function trErr(msg: string): string {
  if (!isEn() || !msg) return msg;
  for (const [re, rep] of ERR_PATTERNS) {
    if (re.test(msg)) {
      return typeof rep === 'function' ? msg.replace(re, rep) : msg.replace(re, rep);
    }
  }
  return msg;
}
