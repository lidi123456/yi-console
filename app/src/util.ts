import DOMPurify from 'dompurify';
import { Marked, marked } from 'marked';
import { getT, isEn } from './i18n';
import type { HubEvent, SessionInfo, SessionStatus, Usage } from './types';

const tx = getT('util');

marked.setOptions({ gfm: true, breaks: true });

const escAttr = (s: string) => s.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

/** Classify image URLs in replies: web is a public URL, file is a path on the host (absolute, relative to cwd, file://),
 *  data is an inline image; other schemes (javascript: and the like) aren't shown */
export function classifyImageSrc(href: string): { kind: 'web' | 'file' | 'data' | 'other'; src: string } {
  const h = href.trim();
  if (/^https?:\/\//i.test(h)) return { kind: 'web', src: h };
  if (/^data:image\//i.test(h)) return { kind: 'data', src: h };
  // Decode before checking the scheme: marked encodes backslashes as %5C (C:%5CUsers%5Ca.png)
  let p = h;
  if (/%[0-9a-fA-F]{2}/.test(p)) {
    try {
      p = decodeURIComponent(p);
    } catch {
      /* not valid encoding, keep as is */
    }
  }
  if (/^file:/i.test(p)) {
    // file:///C:/a.png -> C:/a.png, file:///home/a.png -> /home/a.png
    p = p.replace(/^file:(\/\/[^/]*)?/i, '');
    if (/^\/[a-zA-Z]:[\\/]/.test(p)) p = p.slice(1);
  } else if (/^[a-zA-Z][a-zA-Z0-9+.-]*:/.test(p) && !/^[a-zA-Z]:[\\/]/.test(p)) {
    return { kind: 'other', src: h };
  }
  return p ? { kind: 'file', src: p } : { kind: 'other', src: h };
}

/** For chat replies only: images aren't loaded by the browser right away but rendered as placeholder spans; ChatView decides per host and source whether to show them directly or on click */
const chatMarked = new Marked({
  gfm: true,
  breaks: true,
  renderer: {
    image({ href, text, title }) {
      const c = classifyImageSrc(href ?? '');
      if (c.kind === 'data') return false;
      if (c.kind === 'other') return escAttr(text || href || '');
      const t = title ? ` data-title="${escAttr(title)}"` : '';
      return `<span class="md-rimg" data-kind="${c.kind}" data-src="${escAttr(c.src)}" data-alt="${escAttr(text ?? '')}"${t}></span>`;
    },
  },
});

export function renderMarkdown(src: string): string {
  const html = chatMarked.parse(src, { async: false }) as string;
  return DOMPurify.sanitize(html);
}

export function basename(p: string): string {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? p;
}

/** Abbreviate the home directory as ~ */
export function shortPath(p: string, home?: string): string {
  if (home && p.startsWith(home)) return '~' + p.slice(home.length);
  return p;
}

export function timeAgo(ms: number): string {
  if (!ms) return '';
  const d = (Date.now() - ms) / 1000;
  if (d < 60) return tx('justNow');
  if (d < 3600) return isEn() ? `${Math.floor(d / 60)}m ago` : `${Math.floor(d / 60)} 分钟前`;
  if (d < 86400) return isEn() ? `${Math.floor(d / 3600)}h ago` : `${Math.floor(d / 3600)} 小时前`;
  if (d < 86400 * 7) return isEn() ? `${Math.floor(d / 86400)}d ago` : `${Math.floor(d / 86400)} 天前`;
  const t = new Date(ms);
  return `${t.getFullYear()}-${String(t.getMonth() + 1).padStart(2, '0')}-${String(t.getDate()).padStart(2, '0')}`;
}

export function clock(ms: number): string {
  if (!ms) return '';
  const t = new Date(ms);
  return `${String(t.getHours()).padStart(2, '0')}:${String(t.getMinutes()).padStart(2, '0')}`;
}

export function fmtTokens(n: number): string {
  if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M';
  if (n >= 1e3) return (n / 1e3).toFixed(1) + 'k';
  return String(n);
}

export function fmtUsage(u?: Usage): string {
  if (!u) return '';
  const parts: string[] = [];
  const tokens = u.inputTokens + u.outputTokens + u.cacheReadTokens + u.cacheWriteTokens;
  if (tokens) parts.push(`${fmtTokens(tokens)} tokens`);
  if (u.costUsd) parts.push(`$${u.costUsd.toFixed(u.costUsd < 1 ? 3 : 2)}`);
  if (u.credits) parts.push(`${u.credits.toFixed(2)} credits`);
  return parts.join(' · ');
}

const STATUS_LABEL_ZH: Record<SessionStatus, string> = {
  starting: '启动中',
  idle: '空闲',
  running: '运行中',
  'waiting-permission': '等你确认',
  exited: '已退出',
  error: '出错',
};

const STATUS_LABEL_EN: Record<SessionStatus, string> = {
  starting: 'Starting',
  idle: 'Idle',
  running: 'Running',
  'waiting-permission': 'Waiting approval',
  exited: 'Exited',
  error: 'Error',
};

export const STATUS_LABEL: Record<SessionStatus, string> = new Proxy(STATUS_LABEL_ZH, {
  get(target, prop: string) {
    if (isEn() && prop in STATUS_LABEL_EN) return STATUS_LABEL_EN[prop as SessionStatus];
    return target[prop as SessionStatus];
  },
});

export function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export function toB64(s: string): string {
  const bytes = new TextEncoder().encode(s);
  let bin = '';
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

export function isMobile(): boolean {
  return window.matchMedia('(max-width: 760px)').matches;
}

export function lsGet<T>(k: string, d: T): T {
  try {
    const v = localStorage.getItem(k);
    return v ? (JSON.parse(v) as T) : d;
  } catch {
    return d;
  }
}

export function lsSet(k: string, v: unknown) {
  try {
    localStorage.setItem(k, JSON.stringify(v));
  } catch {
    /* ignore */
  }
}

function fullTime(ms: number): string {
  if (!ms) return '';
  const t = new Date(ms);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${t.getFullYear()}-${p(t.getMonth() + 1)}-${p(t.getDate())} ${p(t.getHours())}:${p(t.getMinutes())}`;
}

/** Export a session as Markdown: user messages, reply text and tool call summaries; thinking and tool output are not exported */
export function sessionMarkdown(info: SessionInfo, events: HubEvent[], toolLabel: string): string {
  const out: string[] = [`# ${info.title || tx('untitledSession')}`, ''];
  out.push(isEn() ? `- Tool: ${toolLabel}${info.model ? ` (${info.model})` : ''}` : `- 工具：${toolLabel}${info.model ? `（${info.model}）` : ''}`);
  out.push(isEn() ? `- Directory: \`${info.cwd}\`` : `- 目录：\`${info.cwd}\``);
  out.push(isEn() ? `- Time: ${fullTime(info.createdAt)} – ${fullTime(info.updatedAt)}` : `- 时间：${fullTime(info.createdAt)} ～ ${fullTime(info.updatedAt)}`);
  const usage = fmtUsage(info.usage);
  if (usage) out.push(isEn() ? `- Usage: ${usage}` : `- 用量：${usage}`);
  out.push('');
  let lastRole = '';
  const tools = new Set<string>();
  for (const e of events) {
    if (e.kind === 'user_message') {
      out.push('---', '', `### 🧑 ${tx('user')} · ${fullTime(e.ts)}`, '', e.text, '');
      if (e.images?.length) out.push(isEn() ? `*(${e.images.length} image(s) attached)*` : `*（附 ${e.images.length} 张图片）*`, '');
      lastRole = 'user';
    } else if (e.kind === 'assistant_text') {
      if (lastRole !== 'assistant') out.push(`### 🤖 ${toolLabel}`, '');
      out.push(e.text, '');
      lastRole = 'assistant';
    } else if (e.kind === 'tool_call' && !tools.has(e.id)) {
      tools.add(e.id);
      if (lastRole !== 'assistant') out.push(`### 🤖 ${toolLabel}`, '');
      out.push(`> 🔧 ${e.name}${e.title ? `${isEn() ? ': ' : '：'}${e.title.replace(/\n/g, ' ')}` : ''}`, '');
      lastRole = 'assistant';
    } else if (e.kind === 'error') {
      out.push(`> ⚠️ ${e.message}`, '');
    }
  }
  return out.join('\n');
}

/** Download a text file in the browser / desktop app */
export function downloadText(name: string, text: string, type = 'text/markdown') {
  const url = URL.createObjectURL(new Blob([text], { type: `${type};charset=utf-8` }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 5000);
}

/** Replace characters not allowed in file names */
export function safeFileName(s: string): string {
  return (s.replace(/[\\/:*?"<>|\r\n]+/g, ' ').trim() || 'session').slice(0, 60);
}

/** Matches if the characters appear in order; higher scores rank first; returns -1 on no match */
export function fuzzyScore(text: string, q: string): number {
  const t = text.toLowerCase();
  const query = q.toLowerCase().trim();
  if (!query) return 0;
  const i = t.indexOf(query);
  if (i >= 0) return 1000 - i - t.length / 100;
  let pos = 0;
  for (const c of query) {
    if (c === ' ') continue;
    pos = t.indexOf(c, pos);
    if (pos < 0) return -1;
    pos++;
  }
  return 100 - t.length / 100;
}
