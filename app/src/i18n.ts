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
  [/这个会话不支持调整模型参数/, 'This session does not support model parameters'],
  [/当前模型不支持这个参数/, 'The current model does not support this parameter'],
  [/模型参数值无效/, 'Invalid model parameter value'],
  [/^切换模型参数失败[:：]\s*(.+)$/, (_, m) => `Could not change model parameter: ${m}`],
  [/MCP 名称只能包含字母、数字、下划线和短横线，长度为 1–64/, 'MCP names must contain 1–64 ASCII letters, digits, underscores or hyphens'],
  [/MCP 名称 yi_computer_use 为内置操作电脑保留/, 'The MCP name yi_computer_use is reserved for Computer Use'],
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

  [/^未知方法[:：]\s*(.+)$/, (_, m) => `Unknown method: ${m}`],
  [/未知方法/, 'Unknown method'],
  [/文件在这期间被修改过，请刷新后再试/, 'File was modified on disk, please reload and try again'],
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
