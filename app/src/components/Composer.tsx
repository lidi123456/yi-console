import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { isEn, tr, useLang } from '../i18n';
import { errorToast, hostSnippets, navigate, setState, toast, useStore } from '../store';
import { clearSession, showChanges } from './SessionActions';
import { isTauri, rpc } from '../transport';
import type { SessionInfo, SlashCommand, Snippet } from '../types';
import { fuzzyScore, isMobile } from '../util';

import { openImage } from './Lightbox';
import { imageMenu } from './ImageMenu';
import { Icon } from './Icon';
import { openMenu } from './Overlay';
import { copiedRef, DRAG_REF, INSERT_COMPOSER, toB64 } from './FilePanel';
import { getPermModesList, permModes } from './NewSession';
import { ModelPicker, OPEN_MODEL } from './ModelPicker';

/** / 命令的匹配：命令名前缀 > 名里的单词开头 > 名里包含 > 说明里包含 > 名的字母顺序匹配；说明不做模糊匹配，免得什么都沾边 */
function cmdScore(label: string, desc: string, q: string) {
  const name = label.toLowerCase();
  const query = q.toLowerCase();
  if (name.startsWith(query)) return 5000 - name.length;
  if (new RegExp(`[\\s:_-]${query.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}`).test(name)) return 4000 - name.length;
  const i = name.indexOf(query);
  if (i >= 0) return 3000 - i;
  if (desc.toLowerCase().includes(query)) return 1000;
  return query.length >= 2 && fuzzyScore(name, query) >= 0 ? 500 - name.length : -1;
}

const IMAGE_MAX = 5 * 1024 * 1024;
export const IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'];

interface Attached {
  mimeType: string;
  data: string;
  url: string;
  name: string;
}

/** 粘贴 / 拖进来的普通文件上限（和 yi-agentd 的 fs.upload 一致） */
const FILE_MAX = 50 * 1024 * 1024;
/** 上传条目的编号（React key） */
let uploadSeq = 0;
/** 分块上传每块的大小：块与块之间可以取消，也能显示进度 */
const CHUNK = 1024 * 1024;
const CANCELLED = new Error('cancelled');

interface Upload {
  id: number;
  name: string;
  sent: number;
  total: number;
}

/** @引用：路径带空格时加引号 */
const refOf = (path: string) => (/\s/.test(path) ? `@"${path}" ` : `@${path} `);
const baseName = (p: string) => p.split(/[\\/]/).pop() ?? p;

/**
 * 桌面版里粘贴的是本机的文件（资源管理器 / 访达里复制的）：从系统剪贴板读出它们的路径，
 * 按文件名对上 `files` 里的每一个才算数，对不上（剪贴板变了、读不到）返回 null，照常上传
 */
async function localPaths(files: File[]): Promise<string[] | null> {
  if (!isTauri || isMobile() || !files.length) return null;
  try {
    const { invoke } = await import('@tauri-apps/api/core');
    const paths = await invoke<string[]>('clipboard_files');
    const out = files.map((f) => paths.find((p) => baseName(p) === f.name));
    return out.every(Boolean) ? (out as string[]) : null;
  } catch {
    return null;
  }
}

function readImage(f: File): Promise<Attached> {
  return new Promise((resolve, reject) => {
    if (!IMAGE_TYPES.includes(f.type))
      return reject(
        new Error(
          tr(
            `不支持的图片格式：${f.type || f.name}，请用 PNG / JPEG / GIF / WebP`,
            `Unsupported image format: ${f.type || f.name}. Use PNG / JPEG / GIF / WebP`,
          ),
        ),
      );
    if (f.size > IMAGE_MAX)
      return reject(
        new Error(
          tr(
            `图片超过 5MB（${(f.size / 1048576).toFixed(1)}MB），请压缩后再发`,
            `Image exceeds 5MB (${(f.size / 1048576).toFixed(1)}MB). Please compress before sending.`,
          ),
        ),
      );
    const r = new FileReader();
    r.onload = () => {
      const url = String(r.result);
      resolve({ mimeType: f.type, data: url.slice(url.indexOf(',') + 1), url, name: f.name || tr('粘贴的图片', 'Pasted image') });
    };
    r.onerror = () => reject(new Error(tr('读取图片失败', 'Failed to read image')));
    r.readAsDataURL(f);
  });
}

/** / 和 @ 菜单里的一项 */
interface PopItem {
  key: string;
  label: string;
  desc?: string;
  group: string;
  icon: string;
  apply: () => void;
}


/** 工具自己切到我们不认识的模式（比如 Claude 的 dontAsk）时，照原名显示 */
export function permLabel(mode?: string | null) {
  return getPermModesList().find((m) => m[0] === (mode || 'default'))?.[1] ?? mode!;
}

function PermPicker({
  hostId,
  info,
  open,
  setOpen,
}: {
  hostId: string;
  info: SessionInfo;
  open: boolean;
  setOpen: (v: boolean) => void;
}) {
  const cur = info.permissionMode || 'default';
  const wrapRef = useRef<HTMLDivElement>(null);
  useBack(open, () => setOpen(false));
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('pointerdown', onDown);
    return () => document.removeEventListener('pointerdown', onDown);
  }, [open]);
  return (
    <div className="menu-wrap" ref={wrapRef}>
      <button
        className={`perm-btn perm-${cur}`}
        title={tr('审批模式（Shift+Tab 切换）', 'Permission mode (Shift+Tab to cycle)')}
        onClick={() => setOpen(!open)}
      >
        <Icon name="shield" size={14} />
        <span className="perm-label">{permLabel(cur)}</span>
        <Icon name="down" size={11} />
      </button>
      {open && (
        <div className="menu menu-up perm-menu" onMouseLeave={() => setOpen(false)}>
          {permModes(info.tool, info.mode).map(([id, label, desc]) => (
            <button
              key={id}
              className={id === cur ? 'active' : ''}
              onClick={() => {
                setOpen(false);
                setPermMode(hostId, info, id);
              }}
            >
              <span className="model-name">
                <span className={`perm-dot perm-${id}`} />
                {label}
                {id === cur && <Icon name="check" size={13} />}
              </span>
              <span className="muted small">{desc}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

async function setPermMode(hostId: string, info: SessionInfo, mode: string) {
  if ((info.permissionMode || 'default') === mode) return;
  try {
    await rpc(hostId, 'session.setPermissionMode', { sessionId: info.id, mode });
    const busy = info.status === 'running' || info.status === 'waiting-permission';
    if (mode === 'bypass' && info.tool === 'claude' && info.status !== 'exited' && info.status !== 'error') {
      toast(
        busy
          ? tr(
              '已切到全部放行：Claude 要重启进程才能生效，这一轮结束后自动重启',
              'Switched to Bypass All: Claude process will restart automatically after this turn',
            )
          : tr('已切到全部放行，Claude 进程已重启', 'Switched to Bypass All (Claude process restarted)'),
      );
    } else toast(tr(`审批模式：${permLabel(mode)}`, `Permission mode: ${permLabel(mode)}`));
  } catch (e) {
    errorToast(e);
  }
}

export function Composer({
  hostId,
  info,
  connected,
  onPrewarm,
  onInterrupt,
  onExport,
  onRename,
}: {
  hostId: string;
  info: SessionInfo;
  connected: boolean;
  onPrewarm: () => void;
  onInterrupt: () => void;
  onExport: () => void;
  onRename: () => void;
}) {
  const key = `${hostId}::${info.id}`;
  const [, lang] = useLang();
  const [text, setText] = useState('');
  /** 正在发送的是哪个会话的消息：发送中切到别的会话，那边的输入框不该转圈，回来后也要按发送结果处理 */
  const [sendingKey, setSendingKey] = useState<string | null>(null);
  const sending = sendingKey === key;
  /** 正在传到主机上的文件（session.attachFile 还没回来），输入框上方显示进度，发送要等它们传完 */
  const [uploads, setUploads] = useState<Upload[]>([]);
  const pendingUploads = useRef(new Set<Promise<void>>());
  /** 点了取消的上传：传完手上这一块就停下，删掉主机上已经传的部分 */
  const cancelledUploads = useRef(new Set<number>());
  /** 上传任务（含取消了还在收尾的）结束时加一，让等着发送的 effect 重新检查 */
  const [uploadsDone, setUploadsDone] = useState(0);
  const hostKind = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.config.kind);
  /** 上传期间按了发送：传完自动发 */
  const [waitSend, setWaitSend] = useState(false);
  const [imgs, setImgs] = useState<Attached[]>([]);
  const [drag, setDrag] = useState(false);
  const [permOpen, setPermOpen] = useState(false);
  const [caret, setCaret] = useState(0);
  const [sel, setSel] = useState(0);
  const [dismissed, setDismissed] = useState('');
  const [commands, setCommands] = useState<SlashCommand[]>([]);
  const [snippets, setSnippets] = useState<Snippet[]>([]);
  const [files, setFiles] = useState<{ path: string; isDir: boolean }[] | null>(null);
  const ta = useRef<HTMLTextAreaElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  // 手机上「+」先问要图片还是文件：accept=image/* 时系统打开相册（照片选择器），不带 accept 才进文件管理
  const imageInput = useRef<HTMLInputElement>(null);

  const running = info.status === 'running' || info.status === 'starting' || info.status === 'waiting-permission';
  const busy = info.status === 'running' || info.status === 'waiting-permission';
  const queued = info.queued ?? [];
  // Claude、Codex 回复中发的消息直接交给工具，它在下一次工具调用后就会读到；其他工具等本轮结束
  const steer = (info.tool === 'claude' || info.tool === 'codex') && queued.length === 0;

  useEffect(() => {
    setImgs([]);
    setWaitSend(false);
  }, [key]);
  /** 当前显示的是哪个会话（异步回调里判断用户是不是已经切走了） */
  const keyRef = useRef<string | null>(key);
  keyRef.current = key;
  // 切会话时整个会话页可能重新挂载：卸载后旧实例的回调也要知道「已经不在这个会话了」
  useEffect(() => {
    keyRef.current = key;
    return () => {
      keyRef.current = null;
    };
  }, [key]);
  const draftOf = (k: string) => {
    try {
      return sessionStorage.getItem(`draft-${k}`) ?? '';
    } catch {
      return '';
    }
  };
  const setDraftOf = (k: string, v: string) => {
    try {
      if (v) sessionStorage.setItem(`draft-${k}`, v);
      else sessionStorage.removeItem(`draft-${k}`);
    } catch {
      /* 忽略 */
    }
  };

  // 草稿按会话保存
  const draftKey = `draft-${key}`;
  useEffect(() => {
    try {
      setText(sessionStorage.getItem(draftKey) ?? '');
    } catch {
      setText('');
    }
  }, [draftKey]);
  // 刚切换会话的这一次 text 还是上一个会话的，不能存到新会话名下；等读出新草稿的那次渲染再存
  const savedFor = useRef(draftKey);
  useEffect(() => {
    if (savedFor.current !== draftKey) {
      savedFor.current = draftKey;
      return;
    }
    setDraftOf(key, text);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text, draftKey]);

  // 工作区文件里「添加到对话」：把 @路径 接到草稿末尾
  useEffect(() => {
    const onInsert = (e: Event) => {
      const d = (e as CustomEvent<{ sessionId: string; text: string }>).detail;
      if (d.sessionId !== info.id) return;
      setText((t) => (t && !/\s$/.test(t) ? t + ' ' : t) + d.text);
      requestAnimationFrame(() => {
        const el = ta.current;
        if (!el) return;
        el.focus();
        el.setSelectionRange(el.value.length, el.value.length);
        setCaret(el.value.length);
      });
    };
    window.addEventListener(INSERT_COMPOSER, onInsert);
    return () => window.removeEventListener(INSERT_COMPOSER, onInsert);
  }, [info.id]);

  // ---------- / 和 @ 弹出菜单 ----------
  const before = text.slice(0, caret);
  const slash = /^\/(\S*)$/.exec(before);
  const at = slash ? null : /(?:^|\s)@([^\s@]*)$/.exec(before);
  const trigger = slash ? `/${slash[1]}` : at ? `@${at[1]}` : '';
  const popOpen = !!trigger && trigger !== dismissed && connected;
  useBack(popOpen, () => setDismissed(trigger));

  // 工具的斜杠命令和片段：打出 / 时取一次
  useEffect(() => {
    if (!slash) return;
    rpc<SlashCommand[]>(hostId, 'session.commands', { sessionId: info.id })
      .then(setCommands)
      .catch(() => setCommands([]));
    hostSnippets(hostId).then(setSnippets).catch(() => setSnippets([]));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [!!slash, hostId, info.id]);

  // @ 文件：防抖后在会话目录里找
  const atQuery = at?.[1];
  useEffect(() => {
    if (atQuery === undefined) return;
    let alive = true;
    const t = setTimeout(() => {
      rpc<{ path: string; isDir: boolean }[]>(hostId, 'fs.search', { root: info.cwd, query: atQuery, limit: 30 })
        .then((r) => alive && setFiles(r))
        .catch(() => alive && setFiles([]));
    }, 120);
    return () => {
      alive = false;
      clearTimeout(t);
    };
  }, [atQuery, hostId, info.cwd]);

  /** 把光标前的触发词换成 rep */
  const replaceTrigger = (rep: string, keepOpen = false) => {
    const start = caret - trigger.length;
    const next = text.slice(0, start) + rep + text.slice(caret);
    setText(next);
    const pos = start + rep.length;
    setCaret(pos);
    if (!keepOpen) setDismissed('');
    requestAnimationFrame(() => {
      ta.current?.focus();
      ta.current?.setSelectionRange(pos, pos);
    });
  };

  const clearSlash = () => replaceTrigger('');

  const items: PopItem[] = useMemo(() => {
    if (!popOpen) return [];
    if (at) {
      return (files ?? []).map((f) => ({
        key: f.path,
        label: f.path + (f.isDir ? '/' : ''),
        group: tr('文件', 'Files'),
        icon: f.isDir ? 'folder' : 'file',
        apply: () => (f.isDir ? replaceTrigger(`@${f.path}/`, true) : replaceTrigger(`@${f.path} `)),
      }));
    }
    const q = slash![1];
    const builtin: PopItem[] = [
      { key: 'b-model', label: 'model', desc: tr('切换模型', 'Switch model'), group: 'Yi Console', icon: 'chat', apply: () => (clearSlash(), window.dispatchEvent(new Event(OPEN_MODEL))) },
      { key: 'b-mode', label: 'mode', desc: tr(`审批模式（现在：${permLabel(info.permissionMode)}）`, `Permission mode (current: ${permLabel(info.permissionMode)})`), group: 'Yi Console', icon: 'shield', apply: () => (clearSlash(), setPermOpen(true)) },
      ...permModes(info.tool, info.mode).map(([id, label, desc]) => ({
        key: `b-mode-${id}`,
        label: `mode ${id}`,
        desc: isEn() ? `${label}: ${desc}` : `${label}：${desc}`,
        group: 'Yi Console',
        icon: 'shield',
        apply: () => (clearSlash(), setPermMode(hostId, info, id)),
      })),
      ...(info.tool === 'codex'
        ? [{ key: 'b-diff', label: 'diff', desc: tr('查看工作区改动（同 /changes）', 'View workspace changes (same as /changes)'), group: 'Yi Console', icon: 'edit' as const, apply: () => (clearSlash(), showChanges(hostId, info.id)) }]
        : []),
      { key: 'b-changes', label: 'changes', desc: tr('查看这个会话改了哪些文件', 'View files modified in this session'), group: 'Yi Console', icon: 'edit', apply: () => (clearSlash(), showChanges(hostId, info.id)) },
      { key: 'b-export', label: 'export', desc: tr('导出为 Markdown', 'Export as Markdown'), group: 'Yi Console', icon: 'copy', apply: () => (clearSlash(), onExport()) },
      { key: 'b-rename', label: 'rename', desc: tr('重命名会话', 'Rename session'), group: 'Yi Console', icon: 'edit', apply: () => (clearSlash(), onRename()) },
      { key: 'b-new', label: 'new', desc: tr('在同一目录新开一个会话', 'New session in the same directory'), group: 'Yi Console', icon: 'plus', apply: () => (clearSlash(), setState({ newSession: { hostId, cwd: info.cwd, tool: info.tool } })) },
      { key: 'b-files', label: 'files', desc: tr('浏览工作目录', 'Browse working directory'), group: 'Yi Console', icon: 'folder', apply: () => (clearSlash(), navigate({ kind: 'files', hostId, path: info.cwd })) },
      { key: 'b-search', label: 'search', desc: tr('在所有会话的消息里搜索', 'Search messages across all sessions'), group: 'Yi Console', icon: 'search', apply: () => (clearSlash(), navigate({ kind: 'search' })) },
    ];
    if (info.mode !== 'terminal')
      builtin.splice(2 + permModes(info.tool, info.mode).length, 0, {
        key: 'b-compact',
        label: 'compact',
        desc: tr('压缩上下文：总结之前的对话，释放上下文空间', 'Compact context: summarize earlier conversation to free context window'),
        group: 'Yi Console',
        icon: 'refresh',
        apply: () => (clearSlash(), rpc(hostId, 'session.compact', { sessionId: info.id }).then(() => toast(tr('正在压缩上下文…', 'Compacting context…')), errorToast)),
      });
    if (info.mode !== 'terminal')
      builtin.splice(3 + permModes(info.tool, info.mode).length, 0, {
        key: 'b-clear',
        label: 'clear',
        desc: tr('清空对话：删掉聊天记录、用量清零，新开一个工具会话', 'Clear conversation: delete messages, reset usage, and start fresh'),
        group: 'Yi Console',
        icon: 'trash',
        apply: () => (clearSlash(), clearSession(hostId, info)),
      });
    if (running) builtin.push({ key: 'b-stop', label: 'stop', desc: tr('中断当前回合（Esc）', 'Interrupt current turn (Esc)'), group: 'Yi Console', icon: 'stop', apply: () => (clearSlash(), onInterrupt()) });
    const snip: PopItem[] = snippets.map((s, i) => ({
      key: `s-${i}`,
      label: s.name,
      desc: s.text.replace(/\s+/g, ' ').slice(0, 80),
      group: tr('提示词片段', 'Snippets'),
      icon: 'file',
      apply: () => replaceTrigger(s.text),
    }));
    const tool: PopItem[] = commands.filter((c) => !builtin.some((b) => b.label === c.name)).map((c) => ({
      key: `t-${c.name}`,
      label: c.name + (c.hint ? ` ${c.hint}` : ''),
      desc: c.description,
      group: tr('工具命令', 'Tool Commands'),
      icon: 'terminal',
      apply: () => replaceTrigger(`/${c.name} `),
    }));
    const all = [...snip, ...builtin, ...tool];
    if (!q) return all;
    return all
      .map((it) => ({ it, s: cmdScore(it.label, it.desc ?? '', q) }))
      .filter((x) => x.s >= 0)
      .sort((a, b) => b.s - a.s)
      .map((x) => x.it);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [popOpen, trigger, files, commands, snippets, info.permissionMode, running, lang]);

  useEffect(() => setSel(0), [trigger]);
  const listRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    listRef.current?.querySelector('.active')?.scrollIntoView({ block: 'nearest' });
  }, [sel]);

  // ---------- 发送 ----------
  /** 在光标处插入文字（前后补空格） */
  const insertAtCaret = (s: string) => {
    const el = ta.current;
    const a = el?.selectionStart ?? text.length;
    const b = el?.selectionEnd ?? text.length;
    const pre = text.slice(0, a);
    const ins = (pre && !/\s$/.test(pre) ? ' ' : '') + s;
    setText(pre + ins + text.slice(b));
    const pos = a + ins.length;
    requestAnimationFrame(() => {
      el?.focus();
      el?.setSelectionRange(pos, pos);
      setCaret(pos);
    });
  };
  /** 图片作为图片发；其他文件存到主机上这个会话的上传目录，插入 @绝对路径 引用 */
  const attach = (fs: Iterable<File>) => {
    const job = attachNow(Array.from(fs));
    pendingUploads.current.add(job);
    return job.finally(() => {
      pendingUploads.current.delete(job);
      setUploadsDone((n) => n + 1);
    });
  };
  /** 分块传一个文件，返回主机上的路径；取消了返回 null。中途取消或出错都删掉已经传上去的部分 */
  const uploadFile = async (f: File, up: Upload): Promise<string | null> => {
    const sid = info.id;
    let path = '';
    try {
      for (let off = 0; off === 0 || off < f.size; off += CHUNK) {
        if (cancelledUploads.current.has(up.id)) throw CANCELLED;
        const data = toB64(await f.slice(off, off + CHUNK).arrayBuffer());
        if (!path) path = (await rpc<{ path: string }>(hostId, 'session.attachFile', { sessionId: sid, name: f.name, data })).path;
        else await rpc(hostId, 'session.attachAppend', { sessionId: sid, path, data });
        const sent = Math.min(f.size, off + CHUNK);
        setUploads((l) => l.map((x) => (x.id === up.id ? { ...x, sent } : x)));
      }
      if (cancelledUploads.current.has(up.id)) throw CANCELLED;
      return path;
    } catch (e) {
      if (path) rpc(hostId, 'fs.delete', { path }).catch(() => {});
      if (e === CANCELLED) return null;
      throw e;
    } finally {
      cancelledUploads.current.delete(up.id);
    }
  };
  const cancelUpload = (id: number) => {
    cancelledUploads.current.add(id);
    setUploads((l) => l.filter((x) => x.id !== id));
    // 之前按了发送在等上传：取消后不自动发，剩下的内容让用户自己决定
    setWaitSend(false);
  };
  const attachNow = async (fs: File[], local?: (string | undefined)[]) => {
    const from = key;
    const refs: string[] = [];
    for (const [i, f] of fs.entries()) {
      // 本机文件：直接引用原文件，不用传
      const at = local?.[i];
      if (at) {
        refs.push(refOf(at));
        continue;
      }
      const up = IMAGE_TYPES.includes(f.type) ? null : { id: ++uploadSeq, name: f.name, sent: 0, total: f.size };
      try {
        if (IMAGE_TYPES.includes(f.type)) {
          const a = await readImage(f);
          setImgs((l) => [...l, a]);
          continue;
        }
        if (f.size > FILE_MAX) throw new Error(tr(`${f.name} 超过 50MB，太大了`, `${f.name} exceeds 50MB`));
        if (!f.size && !f.type) throw new Error(tr(`${f.name} 是文件夹或空文件，没法引用`, `${f.name} is a folder or empty file`));
        if (up) setUploads((l) => [...l, up]);
        const path = await uploadFile(f, up!);
        if (path) refs.push(refOf(path));
      } catch (e) {
        errorToast(e);
      } finally {
        if (up) setUploads((l) => l.filter((x) => x.id !== up.id));
      }
    }
    if (!refs.length) return;
    if (keyRef.current === from) insertAtCaret(refs.join(''));
    else {
      // 传的过程中切到了别的会话：引用接到原会话的草稿末尾
      const d = draftOf(from);
      setDraftOf(from, d + (d && !/\s$/.test(d) ? ' ' : '') + refs.join(''));
    }
  };
  /** 粘贴文件：本机会话里，剪贴板上本机文件的非图片部分直接引用路径，其他照常处理 */
  const pasteFiles = (fs: File[]) => {
    const job = (async () => {
      const others = fs.filter((f) => !IMAGE_TYPES.includes(f.type));
      const paths = hostKind === 'local' ? await localPaths(others) : null;
      const local = fs.map((f) => (paths ? paths[others.indexOf(f)] : undefined));
      await attachNow(fs, local);
    })();
    pendingUploads.current.add(job);
    return job.finally(() => {
      pendingUploads.current.delete(job);
      setUploadsDone((n) => n + 1);
    });
  };

  const send = async () => {
    // 还有文件在传：先转圈，传完（引用插进输入框后）由下面的 effect 再调 send
    if (pendingUploads.current.size) {
      setWaitSend(true);
      return;
    }
    const t = text.trim();
    if ((!t && imgs.length === 0) || sending) return;
    // 直接打 /clear 回车（没从菜单里选）也走 Yi Console 的清空，不发给工具
    if (t === '/clear' && imgs.length === 0 && info.mode !== 'terminal') {
      setText('');
      clearSession(hostId, info);
      return;
    }
    const from = key;
    setSendingKey(from);
    try {
      await rpc(hostId, 'session.send', {
        sessionId: info.id,
        text: t,
        images: imgs.map((i) => ({ mimeType: i.mimeType, data: i.data })),
        // 这一轮还没结束时：Claude、Codex 由工具在下一步读到，其他工具排队到本轮结束
        queue: true,
      });
      // 发出去了才清空。发送中切到了别的会话：只清原会话的草稿，不动现在这个会话的输入框
      if (keyRef.current === from) {
        setText((cur) => (cur.trim() === t ? '' : cur));
        setImgs([]);
      }
      if (draftOf(from).trim() === t) setDraftOf(from, '');
    } catch (e) {
      errorToast(e);
    } finally {
      setSendingKey((k) => (k === from ? null : k));
      if (keyRef.current === from) ta.current?.focus();
    }
  };

  useEffect(() => {
    if (!waitSend || uploads.length || pendingUploads.current.size) return;
    // 等 attach 把引用插进 text 的那次渲染之后再发
    const h = requestAnimationFrame(() => {
      setWaitSend(false);
      send();
    });
    return () => cancelAnimationFrame(h);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [waitSend, uploads.length, uploadsDone, text]);

  const queueAction = async (id: string, action: 'remove' | 'sendNow') => {
    try {
      const r = await rpc<{ text: string }>(hostId, 'session.queue', { sessionId: info.id, id, action });
      if (action === 'remove') {
        // 撤回的放回输入框，方便改了再发
        setText((t) => (t.trim() ? `${t}\n${r.text}` : r.text));
        ta.current?.focus();
      }
    } catch (e) {
      errorToast(e);
    }
  };

  const cyclePerm = () => {
    const ids = permModes(info.tool, info.mode).map((m) => m[0]);
    const i = ids.indexOf(info.permissionMode || 'default');
    setPermMode(hostId, info, ids[(i + 1) % ids.length]);
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.nativeEvent.isComposing) return;
    if (popOpen && items.length > 0) {
      if (e.key === 'ArrowDown') {
        e.preventDefault();
        setSel((sel + 1) % items.length);
        return;
      }
      if (e.key === 'ArrowUp') {
        e.preventDefault();
        setSel((sel - 1 + items.length) % items.length);
        return;
      }
      if (e.key === 'Enter' || e.key === 'Tab') {
        e.preventDefault();
        items[sel]?.apply();
        return;
      }
    }
    if (popOpen && e.key === 'Escape') {
      e.preventDefault();
      setDismissed(trigger);
      return;
    }
    if (e.key === 'Escape' && running) {
      e.preventDefault();
      onInterrupt();
      return;
    }
    if (e.key === 'Tab' && e.shiftKey) {
      e.preventDefault();
      cyclePerm();
      return;
    }
    // 输入框是空的时按 ↑：把最后一条排队的消息撤回来改
    if (e.key === 'ArrowUp' && !text && queued.length > 0) {
      e.preventDefault();
      queueAction(queued[queued.length - 1].id, 'remove');
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !isMobile()) {
      e.preventDefault();
      send();
    }
  };

  // 手机上按实际排版的行数长高（长句自动折行也算），最多 6 行、也不超过可见高度的 35%（弹出输入法时更矮），再多就在框里滚动
  const [mobH, setMobH] = useState(0);
  useLayoutEffect(() => {
    const el = ta.current;
    if (!el || !isMobile()) return;
    const prev = el.style.height;
    el.style.height = '0px';
    const need = el.scrollHeight;
    el.style.height = prev;
    const max = Math.max(28, Math.min(6 * 24 + 4, Math.round(window.innerHeight * 0.35)));
    setMobH(Math.min(Math.max(28, need), max));
  }, [text]);
  const syncCaret = () => setCaret(ta.current?.selectionStart ?? 0);
  const hasText = !!text.trim() || imgs.length > 0;

  return (
    <>
      {queued.length > 0 && (
        <div className="queue">
          {queued.map((q, i) => (
            <div key={q.id} className="queue-item">
              <Icon name="history" size={14} />
              <span className="queue-text truncate" title={q.text}>
                {q.text || tr('（图片）', '(Image)')}
                {q.images > 0 && <span className="muted"> · {tr(`${q.images} 张图片`, `${q.images} image(s)`)}</span>}
              </span>
              <span className="muted small hide-mobile">{i === 0 ? tr('本轮结束后发送', 'Send after current turn') : tr('排队中', 'Queued')}</span>
              <button className="btn small" title={tr('中断当前回合，马上发这条', 'Interrupt current turn and send now')} onClick={() => queueAction(q.id, 'sendNow')}>
                {tr('立即发送', 'Send Now')}
              </button>
              <button className="icon-btn" title={tr('撤回到输入框（输入框为空时按 ↑ 也行）', 'Recall to input (or press ↑ when input is empty)')} onClick={() => queueAction(q.id, 'remove')}>
                <Icon name="x" size={14} />
              </button>
            </div>
          ))}
        </div>
      )}
      <div
        className={`composer ${drag ? 'drag' : ''}`}
        onDragOver={(e) => {
          if (e.dataTransfer.types.includes('Files') || e.dataTransfer.types.includes(DRAG_REF)) {
            e.preventDefault();
            setDrag(true);
          }
        }}
        onDragLeave={() => setDrag(false)}
        onDrop={(e) => {
          e.preventDefault();
          setDrag(false);
          // 从文件面板拖进来的：插入 @相对路径
          const ref = e.dataTransfer.getData(DRAG_REF);
          if (ref) {
            window.dispatchEvent(new CustomEvent(INSERT_COMPOSER, { detail: { sessionId: info.id, text: ref } }));
            return;
          }
          attach(e.dataTransfer.files);
        }}
      >
        {popOpen && (items.length > 0 || (at && files === null)) && (
          <div className="pop" ref={listRef}>
            {at && files === null && <div className="muted small pad-x">{tr('搜索中…', 'Searching…')}</div>}
            {items.slice(0, 80).map((it, i) => (
              <div key={it.key}>
                {(i === 0 || items[i - 1].group !== it.group) && <div className="pop-group">{it.group}</div>}
                <button
                  className={`pop-item ${i === sel ? 'active' : ''}`}
                  onMouseEnter={() => setSel(i)}
                  onMouseDown={(e) => {
                    e.preventDefault();
                    it.apply();
                  }}
                >
                  <Icon name={it.icon} size={14} />
                  <span className={`pop-label ${at ? 'mono' : ''}`}>{at ? it.label : `/${it.label}`}</span>
                  {it.desc && <span className="pop-desc truncate">{it.desc}</span>}
                </button>
              </div>
            ))}
          </div>
        )}
        {popOpen && at && files?.length === 0 && <div className="pop muted small pad-x">{tr('没有匹配的文件', 'No matching files')}</div>}
        {uploads.length > 0 && (
          <div className="upload-row" aria-live="polite">
            {uploads.map((u) => (
              <span key={u.id} className="upload-chip" title={u.name}>
                <span className="spinner mini" />
                <span className="truncate">{tr(`正在上传 ${u.name}`, `Uploading ${u.name}`)}</span>
                {u.total > CHUNK && <span className="upload-pct">{Math.floor((u.sent / u.total) * 100)}%</span>}
                <button className="upload-x" title={tr('取消上传', 'Cancel upload')} aria-label={tr('取消上传', 'Cancel upload')} onClick={() => cancelUpload(u.id)}>
                  <Icon name="x" size={12} />
                </button>
              </span>
            ))}
          </div>
        )}
        {imgs.length > 0 && (
          <div className="attach-row">
            {imgs.map((im, i) => (
              <div key={i} className="attach" title={tr(`${im.name}（双击放大）`, `${im.name} (double-click to zoom)`)}>
                <img src={im.url} alt="" onDoubleClick={() => openImage(im.url)} onContextMenu={(e) => imageMenu(e, im.url)} />
                <button className="attach-x" title={tr('移除', 'Remove')} onClick={() => setImgs((l) => l.filter((_, j) => j !== i))}>
                  <Icon name="x" size={12} />
                </button>
              </div>
            ))}
          </div>
        )}
        <div className="composer-row">
          <textarea
            ref={ta}
            value={text}
            rows={1}
            placeholder={
              !connected
                ? tr('主机未连接', 'Host not connected')
                : busy
                  ? steer
                    ? tr('继续输入，AI 会在下一步读到；Esc 中断', 'Type a follow-up (AI will read it on the next step); Esc to stop')
                    : tr('继续输入，本轮结束后自动发送；Esc 中断', 'Type a message to queue for after this turn; Esc to stop')
                  : info.status === 'exited' || info.status === 'error'
                    ? tr('会话已结束，发送消息会自动恢复', 'Session exited. Send a message to resume automatically.')
                    : isMobile()
                      ? tr('输入消息，/ 命令，@ 文件', 'Message, / commands, @ files')
                      : tr('输入消息，Enter 发送；/ 命令和片段，@ 引用文件，Ctrl+K 命令面板', 'Type a message, Enter to send; / commands & snippets, @ files, Ctrl+K palette')
            }
            disabled={!connected}
            onChange={(e) => {
              setText(e.target.value);
              setCaret(e.target.selectionStart ?? e.target.value.length);
              onPrewarm();
            }}
            onSelect={syncCaret}
            onClick={syncCaret}
            onBlur={() => setTimeout(() => setDismissed(trigger), 150)}
            onFocus={() => setDismissed('')}
            onPaste={(e) => {
              // 系统里复制的文件（资源管理器 Ctrl+C）、截图：图片附上，其他文件引用
              const fs = Array.from(e.clipboardData.files);
              if (fs.length) {
                e.preventDefault();
                pasteFiles(fs);
                return;
              }
              // 文件面板里「复制」的文件：粘贴成 @引用
              const plain = e.clipboardData.getData('text/plain');
              if (copiedRef && plain === copiedRef.plain) {
                e.preventDefault();
                insertAtCaret(copiedRef.ref);
              }
            }}
            onKeyDown={onKeyDown}
            style={{ height: isMobile() && mobH ? mobH : Math.min(200, 22 + text.split('\n').length * 20) }}
          />
        </div>
        <div className="composer-tools">
          <button
            className="icon-btn add-btn"
            title={tr(
              '添加图片或文件（也可以粘贴或拖进来）：图片单张不超过 5MB，其他文件作为 @引用，不超过 50MB',
              'Attach images or files (or paste/drag & drop): images up to 5MB, other files referenced via @ up to 50MB',
            )}
            disabled={!connected}
            onClick={(e) => {
              if (!isMobile()) return fileInput.current?.click();
              const r = e.currentTarget.getBoundingClientRect();
              openMenu({ clientX: r.left, clientY: r.top - 96 }, [
                { label: tr('相册图片', 'Photo Library'), icon: 'image', run: () => imageInput.current?.click() },
                { label: tr('文件', 'File'), icon: 'file', run: () => fileInput.current?.click() },
              ]);
            }}
          >
            <Icon name="plus" size={16} />
          </button>
          <input
            ref={fileInput}
            type="file"
            multiple
            hidden
            onChange={(e) => {
              if (e.target.files) attach(Array.from(e.target.files));
              e.target.value = '';
            }}
          />
          <input
            ref={imageInput}
            type="file"
            accept="image/*"
            multiple
            hidden
            onChange={(e) => {
              if (e.target.files) attach(Array.from(e.target.files));
              e.target.value = '';
            }}
          />
          <PermPicker hostId={hostId} info={info} open={permOpen} setOpen={setPermOpen} />
          <button
            className="tool-chip"
            title={tr('斜杠命令、提示词片段', 'Slash commands & snippets')}
            onClick={() => {
              setText('/' + text.replace(/^\//, ''));
              setCaret(1);
              setDismissed('');
              requestAnimationFrame(() => {
                ta.current?.focus();
                ta.current?.setSelectionRange(1, 1);
              });
            }}
          >
            /
          </button>
          <button
            className="tool-chip"
            title={tr('引用文件', 'Reference file')}
            onClick={() => {
              const pos = ta.current?.selectionStart ?? text.length;
              const pre = text.slice(0, pos);
              const ins = pre && !/\s$/.test(pre) ? ' @' : '@';
              setText(pre + ins + text.slice(pos));
              const np = pos + ins.length;
              setCaret(np);
              setDismissed('');
              requestAnimationFrame(() => {
                ta.current?.focus();
                ta.current?.setSelectionRange(np, np);
              });
            }}
          >
            @
          </button>
          <span className="composer-spacer" />
          <ModelPicker hostId={hostId} info={info} />
          {running && !hasText ? (
            <button className="send-btn stop" onClick={onInterrupt} title={tr('中断（Esc）', 'Stop (Esc)')}>
              <Icon name="stop" size={16} />
            </button>
          ) : (
            <button
              className={`send-btn ${busy && !steer ? 'queue-send' : ''} ${sending || waitSend ? 'loading' : ''}`}
              disabled={(!hasText && !uploads.length) || sending || waitSend || !connected}
              onClick={send}
              title={
                waitSend
                  ? tr('文件传完后自动发送', 'Will send after the upload finishes')
                  : busy
                  ? steer
                    ? tr('发给 AI：它会在下一步读到', 'Send to AI (read on next step)')
                    : tr('排队：本轮结束后发送', 'Queue (send after current turn)')
                  : tr('发送', 'Send')
              }
            >
              {sending || waitSend ? <span className="spinner send-spin" /> : <Icon name={busy && !steer ? 'history' : 'arrowUp'} size={18} />}
            </button>
          )}
        </div>
      </div>
    </>
  );
}
