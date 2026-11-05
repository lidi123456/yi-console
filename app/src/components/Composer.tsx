import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, isEn, useLang } from '../i18n';
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
import { QueueList } from './QueueList';

const t = getT('composer');

/** Matching for / commands: name prefix > word start in name > name contains > description contains > name subsequence; descriptions aren't fuzzy-matched, or everything would match */
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

/** Size limit for pasted / dropped regular files (same as yi-agentd's fs.upload) */
const FILE_MAX = 50 * 1024 * 1024;
/** Upload item counter (React key) */
let uploadSeq = 0;
/** Chunk size for chunked uploads: allows cancelling between chunks and showing progress */
const CHUNK = 1024 * 1024;
const CANCELLED = new Error('cancelled');

interface Upload {
  id: number;
  name: string;
  sent: number;
  total: number;
}

/** @-reference: quote paths that contain spaces */
const refOf = (path: string) => (/\s/.test(path) ? `@"${path}" ` : `@${path} `);
const baseName = (p: string) => p.split(/[\\/]/).pop() ?? p;

/**
 * In the desktop app, pasted files are local files (copied in Explorer / Finder): read their paths from the system clipboard;
 * they only count if each one matches an entry in `files` by name; otherwise (clipboard changed, can't read) return null and upload as usual
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
          t('unsupportedImageFormatUsePng', { v: f.type || f.name }),
        ),
      );
    if (f.size > IMAGE_MAX)
      return reject(
        new Error(
          t('imageExceeds5mbMbPlease', { v: (f.size / 1048576).toFixed(1) }),
        ),
      );
    const r = new FileReader();
    r.onload = () => {
      const url = String(r.result);
      resolve({ mimeType: f.type, data: url.slice(url.indexOf(',') + 1), url, name: f.name || t('pastedImage') });
    };
    r.onerror = () => reject(new Error(t('failedToReadImage')));
    r.readAsDataURL(f);
  });
}

/** An item in the / and @ menus */
interface PopItem {
  key: string;
  label: string;
  desc?: string;
  group: string;
  icon: string;
  apply: () => void;
}


/** When the tool switches itself to a mode we don't know (e.g. Claude's dontAsk), show its raw name */
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
        title={t('permissionModeShiftTabTo')}
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
          ? t('switchedToBypassAllClaude')
          : t('switchedToBypassAllClaude2'),
      );
    } else toast(t('permissionMode', { v: permLabel(mode) }));
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
  /** Which session's message is being sent: if we switch sessions mid-send, the other input box shouldn't spin, and coming back must still handle the result */
  const [sendingKey, setSendingKey] = useState<string | null>(null);
  const sending = sendingKey === key;
  /** Files being sent to the host (session.attachFile hasn't returned); progress is shown above the input box and sending waits for them */
  const [uploads, setUploads] = useState<Upload[]>([]);
  const pendingUploads = useRef(new Set<Promise<void>>());
  /** Uploads that were cancelled: stop after the current chunk and delete what was already uploaded to the host */
  const cancelledUploads = useRef(new Set<number>());
  /** Incremented when an upload task (including cancelled ones still cleaning up) finishes, so the effect waiting to send re-checks */
  const [uploadsDone, setUploadsDone] = useState(0);
  const hostKind = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.config.kind);
  /** Send was pressed during upload: send automatically once uploads finish */
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
  // On mobile, "+" first asks image or file: with accept=image/* the system opens the photo picker; only without accept does it open the file manager
  const imageInput = useRef<HTMLInputElement>(null);

  const running = info.status === 'running' || info.status === 'starting' || info.status === 'waiting-permission';
  const busy = info.status === 'running' || info.status === 'waiting-permission';
  const queued = info.queued ?? [];
  // For Claude and Codex, messages sent mid-reply go straight to the tool, which reads them after its next tool call; other tools wait for the turn to end
  const steer = (info.tool === 'claude' || info.tool === 'codex') && queued.length === 0;

  useEffect(() => {
    setImgs([]);
    setWaitSend(false);
  }, [key]);
  /** Which session is currently shown (lets async callbacks tell whether the user has switched away) */
  const keyRef = useRef<string | null>(key);
  keyRef.current = key;
  // Switching sessions may remount the whole session page: callbacks from the unmounted instance also need to know they're "no longer on this session"
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
      /* ignore */
    }
  };

  // Drafts are saved per session
  const draftKey = `draft-${key}`;
  useEffect(() => {
    try {
      setText(sessionStorage.getItem(draftKey) ?? '');
    } catch {
      setText('');
    }
  }, [draftKey]);
  // Right after switching sessions, text still belongs to the previous session and mustn't be saved under the new one; save from the render that loaded the new draft
  const savedFor = useRef(draftKey);
  useEffect(() => {
    if (savedFor.current !== draftKey) {
      savedFor.current = draftKey;
      return;
    }
    setDraftOf(key, text);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text, draftKey]);

  // "Add to chat" from the workspace files: append @path to the end of the draft
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

  // ---------- / and @ popup menus ----------
  const before = text.slice(0, caret);
  const slash = /^\/(\S*)$/.exec(before);
  const at = slash ? null : /(?:^|\s)@([^\s@]*)$/.exec(before);
  const trigger = slash ? `/${slash[1]}` : at ? `@${at[1]}` : '';
  const popOpen = !!trigger && trigger !== dismissed && connected;
  useBack(popOpen, () => setDismissed(trigger));

  // The tool's slash commands and snippets: fetched once when / is typed
  useEffect(() => {
    if (!slash) return;
    rpc<SlashCommand[]>(hostId, 'session.commands', { sessionId: info.id })
      .then(setCommands)
      .catch(() => setCommands([]));
    hostSnippets(hostId).then(setSnippets).catch(() => setSnippets([]));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [!!slash, hostId, info.id]);

  // @ files: search the session directory after a debounce
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

  /** Replace the trigger word before the cursor with rep */
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
        group: t('files'),
        icon: f.isDir ? 'folder' : 'file',
        apply: () => (f.isDir ? replaceTrigger(`@${f.path}/`, true) : replaceTrigger(`@${f.path} `)),
      }));
    }
    const q = slash![1];
    const builtin: PopItem[] = [
      { key: 'b-model', label: 'model', desc: t('switchModel'), group: 'Yi Console', icon: 'chat', apply: () => (clearSlash(), window.dispatchEvent(new Event(OPEN_MODEL))) },
      { key: 'b-mode', label: 'mode', desc: t('permissionModeCurrent', { v: permLabel(info.permissionMode) }), group: 'Yi Console', icon: 'shield', apply: () => (clearSlash(), setPermOpen(true)) },
      ...permModes(info.tool, info.mode).map(([id, label, desc]) => ({
        key: `b-mode-${id}`,
        label: `mode ${id}`,
        desc: isEn() ? `${label}: ${desc}` : `${label}：${desc}`,
        group: 'Yi Console',
        icon: 'shield',
        apply: () => (clearSlash(), setPermMode(hostId, info, id)),
      })),
      ...(info.tool === 'codex'
        ? [{ key: 'b-diff', label: 'diff', desc: t('viewWorkspaceChangesSameAs'), group: 'Yi Console', icon: 'edit' as const, apply: () => (clearSlash(), showChanges(hostId, info.id)) }]
        : []),
      { key: 'b-changes', label: 'changes', desc: t('viewFilesModifiedInThis'), group: 'Yi Console', icon: 'edit', apply: () => (clearSlash(), showChanges(hostId, info.id)) },
      { key: 'b-export', label: 'export', desc: t('exportAsMarkdown'), group: 'Yi Console', icon: 'copy', apply: () => (clearSlash(), onExport()) },
      { key: 'b-rename', label: 'rename', desc: t('renameSession'), group: 'Yi Console', icon: 'edit', apply: () => (clearSlash(), onRename()) },
      { key: 'b-new', label: 'new', desc: t('newSessionInTheSame'), group: 'Yi Console', icon: 'plus', apply: () => (clearSlash(), setState({ newSession: { hostId, cwd: info.cwd, tool: info.tool } })) },
      { key: 'b-files', label: 'files', desc: t('browseWorkingDirectory'), group: 'Yi Console', icon: 'folder', apply: () => (clearSlash(), navigate({ kind: 'files', hostId, path: info.cwd })) },
      { key: 'b-search', label: 'search', desc: t('searchMessagesAcrossAllSessions'), group: 'Yi Console', icon: 'search', apply: () => (clearSlash(), navigate({ kind: 'search' })) },
    ];
    if (info.mode !== 'terminal')
      builtin.splice(2 + permModes(info.tool, info.mode).length, 0, {
        key: 'b-compact',
        label: 'compact',
        desc: t('compactContextSummarizeEarlierConversation'),
        group: 'Yi Console',
        icon: 'refresh',
        apply: () => (clearSlash(), rpc(hostId, 'session.compact', { sessionId: info.id }).then(() => toast(t('compactingContext')), errorToast)),
      });
    if (info.mode !== 'terminal')
      builtin.splice(3 + permModes(info.tool, info.mode).length, 0, {
        key: 'b-clear',
        label: 'clear',
        desc: t('clearConversationDeleteMessagesReset'),
        group: 'Yi Console',
        icon: 'trash',
        apply: () => (clearSlash(), clearSession(hostId, info)),
      });
    if (running) builtin.push({ key: 'b-stop', label: 'stop', desc: t('interruptCurrentTurnEsc'), group: 'Yi Console', icon: 'stop', apply: () => (clearSlash(), onInterrupt()) });
    const snip: PopItem[] = snippets.map((s, i) => ({
      key: `s-${i}`,
      label: s.name,
      desc: s.text.replace(/\s+/g, ' ').slice(0, 80),
      group: t('snippets'),
      icon: 'file',
      apply: () => replaceTrigger(s.text),
    }));
    const tool: PopItem[] = commands.filter((c) => !builtin.some((b) => b.label === c.name)).map((c) => ({
      key: `t-${c.name}`,
      label: c.name + (c.hint ? ` ${c.hint}` : ''),
      desc: c.description,
      group: t('toolCommands'),
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

  // ---------- Sending ----------
  /** Insert text at the cursor (padding with spaces on both sides) */
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
  /** Images are sent as images; other files are saved to this session's upload directory on the host and inserted as an @absolute-path reference */
  const attach = (fs: Iterable<File>) => {
    const job = attachNow(Array.from(fs));
    pendingUploads.current.add(job);
    return job.finally(() => {
      pendingUploads.current.delete(job);
      setUploadsDone((n) => n + 1);
    });
  };
  /** Upload one file in chunks and return its path on the host; returns null if cancelled. On cancel or error, delete whatever was already uploaded */
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
    // Send was pressed earlier and is waiting for uploads: after a cancel, don't send automatically; let the user decide about what's left
    setWaitSend(false);
  };
  const attachNow = async (fs: File[], local?: (string | undefined)[]) => {
    const from = key;
    const refs: string[] = [];
    for (const [i, f] of fs.entries()) {
      // Local file: reference the original directly, no upload needed
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
        if (f.size > FILE_MAX) throw new Error(t('exceeds50mb', { name: f.name }));
        if (!f.size && !f.type) throw new Error(t('isAFolderOrEmpty', { name: f.name }));
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
      // Switched to another session during upload: append the reference to the original session's draft
      const d = draftOf(from);
      setDraftOf(from, d + (d && !/\s$/.test(d) ? ' ' : '') + refs.join(''));
    }
  };
  /** Pasting files: in a local session, non-image local files on the clipboard are referenced by path; everything else is handled as usual */
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
    // Files still uploading: spin for now; once done (and the references are in the input box) the effect below calls send again
    if (pendingUploads.current.size) {
      setWaitSend(true);
      return;
    }
    const t = text.trim();
    if ((!t && imgs.length === 0) || sending) return;
    // Typing /clear and pressing Enter (without picking from the menu) also uses Yi Console's clear, not sent to the tool
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
        // Turn still running: Claude and Codex read it at their next step; other tools queue it until the turn ends
        queue: true,
      });
      // Only clear once sent. If we switched sessions mid-send, only clear the original session's draft and leave the current input box alone
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
    // Send in the render after attach has inserted the references into text
    const h = requestAnimationFrame(() => {
      setWaitSend(false);
      send();
    });
    return () => cancelAnimationFrame(h);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [waitSend, uploads.length, uploadsDone, text]);

  /** Recall a queued message into the input box (↑ in an empty input box) */
  const recallQueued = async (id: string) => {
    try {
      const r = await rpc<{ text: string }>(hostId, 'session.queue', { sessionId: info.id, id, action: 'remove' });
      setText((t) => (t.trim() ? `${t}\n${r.text}` : r.text));
      ta.current?.focus();
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
    // ↑ in an empty input box: recall the last queued message for editing
    if (e.key === 'ArrowUp' && !text && queued.length > 0) {
      e.preventDefault();
      recallQueued(queued[queued.length - 1].id);
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !isMobile()) {
      e.preventDefault();
      send();
    }
  };

  // On mobile, grow with the actual number of laid-out lines (including wrapped long lines), up to 6 lines and at most 35% of the visible height (less when the IME is open); beyond that, scroll inside the box
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
      <QueueList
        hostId={hostId}
        info={info}
        onRecall={(r) => {
          // Recalled messages go back into the input box so they can be edited and resent
          setText((t) => (t.trim() ? `${t}\n${r}` : r));
          ta.current?.focus();
        }}
      />
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
          // Dragged in from the file panel: insert @relative-path
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
            {at && files === null && <div className="muted small pad-x">{t('searching')}</div>}
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
        {popOpen && at && files?.length === 0 && <div className="pop muted small pad-x">{t('noMatchingFiles')}</div>}
        {uploads.length > 0 && (
          <div className="upload-row" aria-live="polite">
            {uploads.map((u) => (
              <span key={u.id} className="upload-chip" title={u.name}>
                <span className="spinner mini" />
                <span className="truncate">{t('uploading', { name: u.name })}</span>
                {u.total > CHUNK && <span className="upload-pct">{Math.floor((u.sent / u.total) * 100)}%</span>}
                <button className="upload-x" title={t('cancelUpload')} aria-label={t('cancelUpload')} onClick={() => cancelUpload(u.id)}>
                  <Icon name="x" size={12} />
                </button>
              </span>
            ))}
          </div>
        )}
        {imgs.length > 0 && (
          <div className="attach-row">
            {imgs.map((im, i) => (
              <div key={i} className="attach" title={t('doubleClickToZoom', { name: im.name })}>
                <img src={im.url} alt="" onDoubleClick={() => openImage(im.url)} onContextMenu={(e) => imageMenu(e, im.url)} />
                <button className="attach-x" title={t('remove')} onClick={() => setImgs((l) => l.filter((_, j) => j !== i))}>
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
                ? t('hostNotConnected')
                : busy
                  ? steer
                    ? t('typeAFollowUpAi')
                    : t('typeAMessageToQueue')
                  : info.status === 'exited' || info.status === 'error'
                    ? t('sessionExitedSendAMessage')
                    : isMobile()
                      ? t('messageCommandsFiles')
                      : t('typeAMessageEnterTo')
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
              // Files copied in the system (Ctrl+C in Explorer) and screenshots: attach images, reference other files
              const fs = Array.from(e.clipboardData.files);
              if (fs.length) {
                e.preventDefault();
                pasteFiles(fs);
                return;
              }
              // Files copied from the file panel: paste as an @reference
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
            title={t('attachImagesOrFilesOr')}
            disabled={!connected}
            onClick={(e) => {
              if (!isMobile()) return fileInput.current?.click();
              const r = e.currentTarget.getBoundingClientRect();
              openMenu({ clientX: r.left, clientY: r.top - 96 }, [
                { label: t('photoLibrary'), icon: 'image', run: () => imageInput.current?.click() },
                { label: t('file'), icon: 'file', run: () => fileInput.current?.click() },
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
            title={t('slashCommandsSnippets')}
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
            title={t('referenceFile')}
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
            <button className="send-btn stop" onClick={onInterrupt} title={t('stopEsc')}>
              <Icon name="stop" size={16} />
            </button>
          ) : (
            <button
              className={`send-btn ${busy && !steer ? 'queue-send' : ''} ${sending || waitSend ? 'loading' : ''}`}
              disabled={(!hasText && !uploads.length) || sending || waitSend || !connected}
              onClick={send}
              title={
                waitSend
                  ? t('willSendAfterTheUpload')
                  : busy
                  ? steer
                    ? t('sendToAiReadOn')
                    : t('queueSendAfterCurrentTurn')
                  : t('send')
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
