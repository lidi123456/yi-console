import { type ReactNode, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';
import { useBack } from '../back';
import { getT, isEn, useLang } from '../i18n';
import { errorToast, hostSnippets, navigate, openNewSession, sessionInfo, toast, useStore } from '../store';
import {
  addComposerUpload,
  armWaitSend,
  beginSend,
  beginUploadJob,
  cancelComposerUpload,
  clearWaitSend,
  completeSendSuccess,
  endSend,
  endUploadJob,
  focusComposer,
  getComposerSnapshot,
  insertOrAppendComposerText,
  isComposerUploadCancelled,
  registerActiveComposer,
  removeComposerUpload,
  setComposerImages,
  setComposerRefs,
  setComposerText,
  subscribeComposer,
  updateComposerUpload,
  type Attached,
  type Upload,
} from '../composerState';
import { clearSession, openFilesInPanel, showChanges } from './SessionActions';
import { isTauri, rpc } from '../transport';
import type { SessionInfo, SlashCommand, Snippet } from '../types';
import { fuzzyScore, isMobile, toB64 as toB64Str } from '../util';
import { DRAG_REF, extractDroppedEntries, planComposerDrop, resolveDroppedLocalPaths } from '../dropFiles';
import {
  ADD_CONTEXT_REF,
  REMOVE_REVIEW_CONTEXT_REF,
  formatLineRange,
  nextContextRefId,
  parseContextMessage,
  sameContextRef,
  serializeContextMessage,
  type AddContextRefOptions,
  type ContextRef,
  type ContextRefInput,
} from '../contextRefs';
import { ContextRefBar } from './ContextRefBar';
import { unadoptFindingByRef } from '../reviewState';

import { openImage } from './Lightbox';
import { imageMenu } from './ImageMenu';
import { Icon } from './Icon';
import { askText, openMenu } from './Overlay';
import { copiedRef, INSERT_COMPOSER, toB64 } from './FilePanel';
import { permLabel, permModes } from '../perm';
import { nextPerm, PermPicker } from './PermPicker';
import { ModelPicker, OPEN_MODEL } from './ModelPicker';
import { QueueList } from './QueueList';
import { bringSideChatToMain, deriveSideTitle, sideChatTitle, startSideChat } from '../sideChat';
import { noteOwnerMessage } from '../bots';

const t = getT('composer');

/** Refill a past user prompt (text + <yi-context> pills) into this session's input box */
export const RESTORE_COMPOSER = 'yi-console:restore-composer';
export function restoreToComposer(sessionId: string, text: string) {
  window.dispatchEvent(new CustomEvent(RESTORE_COMPOSER, { detail: { sessionId, text } }));
}

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

export type { Attached };

/** Size limit for pasted / dropped regular files (same as yi-agentd's fs.upload) */
export const FILE_MAX = 50 * 1024 * 1024;
/** Upload item counter (React key) */
let uploadSeq = 0;
/** Chunk size for chunked uploads: allows cancelling between chunks and showing progress */
const CHUNK = 1024 * 1024;
const CANCELLED = new Error('cancelled');

/**
 * Upload a file to the session's upload directory on the host in chunks and return its path there; null if
 * `cancelled()` turned true between chunks. On cancel or error, whatever was already uploaded is deleted
 */
export async function uploadChunked(
  hostId: string,
  sessionId: string,
  f: File,
  onProgress: (sent: number) => void,
  cancelled: () => boolean = () => false,
): Promise<string | null> {
  let path = '';
  try {
    for (let off = 0; off === 0 || off < f.size; off += CHUNK) {
      if (cancelled()) throw CANCELLED;
      const data = toB64(await f.slice(off, off + CHUNK).arrayBuffer());
      if (!path) path = (await rpc<{ path: string }>(hostId, 'session.attachFile', { sessionId, name: f.name, data })).path;
      else await rpc(hostId, 'session.attachAppend', { sessionId, path, data });
      onProgress(Math.min(f.size, off + CHUNK));
    }
    if (cancelled()) throw CANCELLED;
    return path;
  } catch (e) {
    if (path) rpc(hostId, 'fs.delete', { path }).catch(() => {});
    if (e === CANCELLED) return null;
    throw e;
  }
}

/** @-reference: quote paths that contain spaces */
export const refOf = (path: string) => (/\s/.test(path) ? `@"${path}" ` : `@${path} `);
const baseName = (p: string) => p.split(/[\\/]/).pop() ?? p;

/**
 * In the desktop app, pasted files are local files (copied in Explorer / Finder): read their paths from the system clipboard;
 * they only count if each one matches an entry in `files` by name; otherwise (clipboard changed, can't read) return null and upload as usual
 */
export async function localPaths(files: File[]): Promise<string[] | null> {
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

export function readImage(f: File): Promise<Attached> {
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
  desktopControls,
  placeholder,
  compact = false,
  openDroppedInPanel = false,
  topSlot,
  transformSend,
  onAfterSend,
}: {
  hostId: string;
  info: SessionInfo;
  connected: boolean;
  onPrewarm?: () => void;
  onInterrupt: () => void;
  onExport?: () => void;
  onRename?: () => void;
  desktopControls?: ReactNode;
  placeholder?: string;
  /** Compact variant used inside embedded side chats: keeps full composer UX while hiding full-page navigation commands */
  compact?: boolean;
  /** Open non-image files dropped onto this composer in the session's right-side file panel */
  openDroppedInPanel?: boolean;
  /** Optional chip/banner rendered above the textarea row (e.g. focused review finding tag) */
  topSlot?: ReactNode;
  /** Optional message body transformer executed right before sending (when no text-only bypass happens) */
  transformSend?: (text: string) => string;
  /** Optional hook invoked right after `session.send` succeeds */
  onAfterSend?: () => void;
}) {
  const key = `${hostId}::${info.id}`;
  const [, lang] = useLang();
  const snap = useSyncExternalStore(
    (cb) => subscribeComposer(key, cb),
    () => getComposerSnapshot(key),
  );
  const { text, imgs, refs, sending, uploads, waitSend } = snap;
  const setText = (next: string | ((cur: string) => string)) => setComposerText(key, next);
  const setImgs = (next: Attached[] | ((cur: Attached[]) => Attached[])) => setComposerImages(key, next);
  const setRefs = (next: ContextRef[] | ((cur: ContextRef[]) => ContextRef[])) => setComposerRefs(key, next);
  const hostKind = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.config.kind);
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
    setDismissed('');
    setDrag(false);
    setPermOpen(false);
    setCaret(0);
    setSel(0);
    setFiles(null);
  }, [key]);

  // "Add to chat" from the workspace files: append @path to the end of the draft
  useEffect(() => {
    const onInsert = (e: Event) => {
      const d = (e as CustomEvent<{ sessionId: string; text: string }>).detail;
      if (d.sessionId !== info.id) return;
      if (d.text) setText((t) => (t && !/\s$/.test(t) ? t + ' ' : t) + d.text);
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

  // Code / diff / review / terminal snippet referenced into chat: add as a compact pill above the input box
  useEffect(() => {
    const onAddRef = (e: Event) => {
      const d = (e as CustomEvent<{ sessionId: string; ref: ContextRefInput } & AddContextRefOptions>).detail;
      if (d.sessionId !== info.id) return;
      setRefs((cur) => (cur.some((x) => sameContextRef(x, d.ref)) ? cur : [...cur, { ...d.ref, id: nextContextRefId() }]));
      if (d.prompt) {
        setText((cur) => (cur.trim() ? cur : d.prompt!));
      }
      if (d.focus !== false) requestAnimationFrame(() => ta.current?.focus());
    };
    window.addEventListener(ADD_CONTEXT_REF, onAddRef);
    return () => window.removeEventListener(ADD_CONTEXT_REF, onAddRef);
  }, [info.id]);

  // Remove a review finding pill when un-adopted from the Review Side Pane or Diff inline card before send
  useEffect(() => {
    const onRmReviewRef = (e: Event) => {
      const d = (e as CustomEvent<{ sessionId: string; sideSessionId: string; findingId: string }>).detail;
      if (d.sessionId !== info.id) return;
      setRefs((cur) => cur.filter((x) => !(x.kind === 'review' && x.sideSessionId === d.sideSessionId && x.reviewFindingId === d.findingId)));
    };
    window.addEventListener(REMOVE_REVIEW_CONTEXT_REF, onRmReviewRef);
    return () => window.removeEventListener(REMOVE_REVIEW_CONTEXT_REF, onRmReviewRef);
  }, [info.id]);

  // Refill a past prompt from the prompt outline or sticky current-prompt bar
  useEffect(() => {
    const onRestore = (e: Event) => {
      const d = (e as CustomEvent<{ sessionId: string; text: string }>).detail;
      if (d.sessionId !== info.id) return;
      restoreMessageToComposer(d.text);
      requestAnimationFrame(() => {
        const el = ta.current;
        if (!el) return;
        el.focus();
        el.setSelectionRange(el.value.length, el.value.length);
        setCaret(el.value.length);
      });
    };
    window.addEventListener(RESTORE_COMPOSER, onRestore);
    return () => window.removeEventListener(RESTORE_COMPOSER, onRestore);
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

  const canSideChat = info.mode === 'structured' && info.tool !== 'shell' && info.tool !== 'files' && !info.sideChat;
  const hostSessions = useStore((s) => s.sessions[hostId]);
  const sideChats = useMemo(
    () => (hostSessions ? hostSessions.filter((x) => x.parentId === info.id && x.sideChat) : []),
    [hostSessions, info.id],
  );

  const items: PopItem[] = useMemo(() => {
    if (!popOpen) return [];
    if (at) {
      const q = at[1].toLowerCase();
      const sideItems: PopItem[] = canSideChat
        ? sideChats
            .filter((sc) => {
              if (!q) return true;
              return sideChatTitle(sc).toLowerCase().includes(q) || sc.tool.toLowerCase().includes(q);
            })
            .map((sc) => ({
              key: `side-${sc.id}`,
              label: sideChatTitle(sc),
              desc: sc.status === 'running' || sc.status === 'starting' ? t('sideChatRunningHint') : t('sideChatMentionHint'),
              group: t('sideChatsGroup'),
              icon: 'sideChat',
              apply: () => {
                replaceTrigger('');
                void bringSideChatToMain(hostId, info.id, sc);
              },
            }))
        : [];
      const fileItems: PopItem[] = (files ?? []).map((f) => ({
        key: f.path,
        label: f.path + (f.isDir ? '/' : ''),
        group: t('files'),
        icon: f.isDir ? 'folder' : 'file',
        apply: () => (f.isDir ? replaceTrigger(`@${f.path}/`, true) : replaceTrigger(`@${f.path} `)),
      }));
      return [...sideItems, ...fileItems];
    }
    const q = slash![1];
    const builtin: PopItem[] = [
      ...(canSideChat
        ? [
            {
              key: 'b-side',
              label: 'side',
              desc: t('sideChatSlashDesc'),
              group: 'Yi Console',
              icon: 'sideChat',
              apply: () => replaceTrigger('/side '),
            },
            {
              key: 'b-btw',
              label: 'btw',
              desc: t('sideChatBtwDesc'),
              group: 'Yi Console',
              icon: 'sideChat',
              apply: () => replaceTrigger('/btw '),
            },
          ]
        : []),
      { key: 'b-model', label: 'model', desc: t('switchModel'), group: 'Yi Console', icon: 'chat', apply: () => (clearSlash(), window.dispatchEvent(new CustomEvent(OPEN_MODEL, { detail: { sessionId: info.id } }))) },
      ...(info.review
        ? []
        : [
            { key: 'b-mode', label: 'mode', desc: t('permissionModeCurrent', { v: permLabel(info.permissionMode) }), group: 'Yi Console', icon: 'shield', apply: () => (clearSlash(), setPermOpen(true)) },
            ...permModes(info.tool, info.mode).map(([id, label, desc]) => ({
              key: `b-mode-${id}`,
              label: `mode ${id}`,
              desc: isEn() ? `${label}: ${desc}` : `${label}：${desc}`,
              group: 'Yi Console',
              icon: 'shield',
              apply: () => (clearSlash(), setPermMode(hostId, info, id)),
            })),
          ]),
      ...(!compact && info.tool === 'codex'
        ? [{ key: 'b-diff', label: 'diff', desc: t('viewWorkspaceChangesSameAs'), group: 'Yi Console', icon: 'edit' as const, apply: () => (clearSlash(), showChanges(hostId, info.id)) }]
        : []),
      ...(!compact
        ? [{ key: 'b-changes', label: 'changes', desc: t('viewFilesModifiedInThis'), group: 'Yi Console', icon: 'edit' as const, apply: () => (clearSlash(), showChanges(hostId, info.id)) }]
        : []),
      ...(onExport
        ? [{ key: 'b-export', label: 'export', desc: t('exportAsMarkdown'), group: 'Yi Console', icon: 'copy' as const, apply: () => (clearSlash(), onExport()) }]
        : []),
      ...(onRename
        ? [{ key: 'b-rename', label: 'rename', desc: t('renameSession'), group: 'Yi Console', icon: 'edit' as const, apply: () => (clearSlash(), onRename()) }]
        : []),
      ...(!compact
        ? [
            { key: 'b-new', label: 'new', desc: t('newSessionInTheSame'), group: 'Yi Console', icon: 'plus' as const, apply: () => (clearSlash(), openNewSession({ hostId, cwd: info.cwd, tool: info.tool })) },
            { key: 'b-files', label: 'files', desc: t('browseWorkingDirectory'), group: 'Yi Console', icon: 'folder' as const, apply: () => (clearSlash(), navigate({ kind: 'files', hostId, path: info.cwd })) },
            { key: 'b-search', label: 'search', desc: t('searchMessagesAcrossAllSessions'), group: 'Yi Console', icon: 'search' as const, apply: () => (clearSlash(), navigate({ kind: 'search' })) },
          ]
        : []),
    ];
    const insertIdx = Math.max(0, builtin.findIndex((b) => b.key === 'b-model') + 1 + (info.review ? 0 : 1 + permModes(info.tool, info.mode).length));
    if (info.mode !== 'terminal')
      builtin.splice(insertIdx, 0, {
        key: 'b-compact',
        label: 'compact',
        desc: t('compactContextSummarizeEarlierConversation'),
        group: 'Yi Console',
        icon: 'refresh',
        apply: () => (clearSlash(), rpc(hostId, 'session.compact', { sessionId: info.id }).then(() => toast(t('compactingContext')), errorToast)),
      });
    if (info.mode !== 'terminal')
      builtin.splice(insertIdx + 1, 0, {
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
  }, [popOpen, trigger, files, commands, snippets, info.permissionMode, running, lang, canSideChat, sideChats]);

  useEffect(() => setSel(0), [trigger]);
  const listRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    listRef.current?.querySelector('.active')?.scrollIntoView({ block: 'nearest' });
  }, [sel]);

  // ---------- Sending ----------
  /** Insert text at the cursor (padding with spaces on both sides) */
  const insertAtCaret = (s: string) => {
    const cur = getComposerSnapshot(key).text;
    const el = ta.current;
    const useSel = !!el && el.value === cur;
    const a = useSel ? Math.min(el.selectionStart ?? cur.length, cur.length) : cur.length;
    const b = useSel ? Math.min(el.selectionEnd ?? cur.length, cur.length) : cur.length;
    const pre = cur.slice(0, a);
    const ins = (pre && !/\s$/.test(pre) ? ' ' : '') + s;
    setComposerText(key, pre + ins + cur.slice(b));
    const pos = a + ins.length;
    requestAnimationFrame(() => {
      const curEl = ta.current;
      if (!curEl || !curEl.isConnected) return;
      curEl.focus();
      curEl.setSelectionRange(pos, pos);
      setCaret(pos);
    });
  };

  useEffect(
    () =>
      registerActiveComposer(key, {
        insertAtCaret: (s) => insertAtCaret(s),
        focus: () => ta.current?.focus(),
      }),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [key],
  );

  const runUploadJob = (from: string, fn: () => Promise<void>) => {
    beginUploadJob(from);
    return fn().finally(() => {
      const runner = endUploadJob(from);
      if (runner) void runner();
    });
  };

  /** Images are sent as images; other files are saved to this session's upload directory on the host and inserted as an @absolute-path reference */
  const attach = (fs: Iterable<File>) => {
    const from = key;
    return runUploadJob(from, () => attachNow(from, hostId, info.id, Array.from(fs)));
  };

  /** Upload one file in chunks and return its path on the host; returns null if cancelled. On cancel or error, delete whatever was already uploaded */
  const uploadFile = async (from: string, hId: string, sid: string, f: File, up: Upload): Promise<string | null> => {
    try {
      return await uploadChunked(
        hId,
        sid,
        f,
        (sent) => updateComposerUpload(from, up.id, sent),
        () => isComposerUploadCancelled(from, up.id),
      );
    } finally {
      removeComposerUpload(from, up.id);
    }
  };

  const cancelUpload = (id: number) => cancelComposerUpload(key, id);

  const attachNow = async (
    from: string,
    hId: string,
    sid: string,
    fs: File[],
    local?: (string | undefined)[],
    opts?: { openInPanel?: boolean; dirHints?: boolean[] },
  ) => {
    const refs: string[] = [];
    const opened: string[] = [];
    for (const [i, f] of fs.entries()) {
      // Local file: reference the original directly, no upload needed
      const at = local?.[i];
      if (at) {
        refs.push(refOf(at));
        opened.push(at);
        continue;
      }
      const up = IMAGE_TYPES.includes(f.type) ? null : { id: ++uploadSeq, name: f.name, sent: 0, total: f.size };
      try {
        if (IMAGE_TYPES.includes(f.type)) {
          const a = await readImage(f);
          setComposerImages(from, (cur) => [...cur, a]);
          continue;
        }
        if (opts?.dirHints?.[i]) throw new Error(t('isAFolderOrEmpty', { name: f.name }));
        if (f.size > FILE_MAX) throw new Error(t('exceeds50mb', { name: f.name }));
        if (!f.size && !f.type) throw new Error(t('isAFolderOrEmpty', { name: f.name }));
        if (up) addComposerUpload(from, up);
        const path = await uploadFile(from, hId, sid, f, up!);
        if (path) {
          refs.push(refOf(path));
          opened.push(path);
        } else clearWaitSend(from);
      } catch (e) {
        clearWaitSend(from);
        errorToast(e);
      } finally {
        if (up) removeComposerUpload(from, up.id);
      }
    }
    if (refs.length) insertOrAppendComposerText(from, refs.join(''));
    if (opts?.openInPanel && opened.length) openFilesInPanel(hId, sid, opened);
  };

  /** Dropping external files onto the composer: attach/@reference as usual, and open non-image files in the file panel */
  const dropFiles = (dt: DataTransfer) => {
    const entries = extractDroppedEntries(dt);
    if (!entries.length) return;
    const from = key;
    const hId = hostId;
    const sid = info.id;
    const isLocalHost = hostKind === 'local';
    const openInPanel = !!openDroppedInPanel;
    const fs = entries.map((e) => e.file);
    const hints = entries.map((e) => e.isDirHint);
    return runUploadJob(from, async () => {
      const resolved = isLocalHost ? await resolveDroppedLocalPaths(fs) : null;
      const plan = planComposerDrop(fs, {
        isLocalHost,
        resolved,
        dirHints: hints,
        imageTypes: IMAGE_TYPES,
        openInPanel,
      });
      await attachNow(from, hId, sid, fs, plan.localPaths, {
        openInPanel: plan.shouldOpenInPanel,
        dirHints: plan.dirHints,
      });
    });
  };

  /** Pasting files: in a local session, non-image local files on the clipboard are referenced by path; everything else is handled as usual */
  const pasteFiles = (fs: File[]) => {
    const from = key;
    const hId = hostId;
    const sid = info.id;
    const isLocalHost = hostKind === 'local';
    return runUploadJob(from, async () => {
      const others = fs.filter((f) => !IMAGE_TYPES.includes(f.type));
      const paths = isLocalHost ? await localPaths(others) : null;
      const local = fs.map((f) => (paths ? paths[others.indexOf(f)] : undefined));
      await attachNow(from, hId, sid, fs, local);
    });
  };

  const restoreMessageToComposer = (rawText: string) => {
    const parsed = parseContextMessage(rawText);
    if (parsed.refs.length) {
      const fresh = parsed.refs.map((r) => ({ ...r, id: nextContextRefId() }));
      setRefs((cur) => [...cur, ...fresh.filter((r) => !cur.some((x) => sameContextRef(x, r)))]);
    }
    if (parsed.text) setText((cur) => (cur.trim() ? `${cur}\n${parsed.text}` : parsed.text));
    ta.current?.focus();
  };

  const executeSend = async (
    from: string,
    hId: string,
    sid: string,
    fallbackInfo: SessionInfo,
    transform?: (text: string) => string,
    afterSend?: () => void,
  ) => {
    const curSnap = getComposerSnapshot(from);
    const curInfo = sessionInfo(hId, sid) ?? fallbackInfo;
    const canSide = curInfo.mode === 'structured' && curInfo.tool !== 'shell' && curInfo.tool !== 'files' && !curInfo.sideChat;
    const trimmed = curSnap.text.trim();
    const curImgs = curSnap.imgs;
    const curRefs = curSnap.refs;
    if ((!trimmed && curImgs.length === 0 && curRefs.length === 0) || curSnap.sending) return;
    // Typing /clear and pressing Enter (without picking from the menu) also uses Yi Console's clear, not sent to the tool
    if (trimmed === '/clear' && curImgs.length === 0 && curRefs.length === 0 && curInfo.mode !== 'terminal') {
      setComposerText(from, '');
      clearSession(hId, curInfo);
      return;
    }
    // /side [question] or /btw [question]: open a hidden read-only side chat in the right panel without interrupting the main turn
    const sideMatch = canSide && curImgs.length === 0 && curRefs.length === 0 ? /^\/(?:side|btw)(?:\s+([\s\S]*))?$/i.exec(trimmed) : null;
    if (sideMatch) {
      if (!beginSend(from)) return;
      let ok = false;
      try {
        await startSideChat({ hostId: hId, parent: curInfo, prompt: sideMatch[1]?.trim() });
        completeSendSuccess(from, { sentText: trimmed, sentImgs: [], sentRefs: [] });
        ok = true;
      } catch (e) {
        errorToast(e);
      } finally {
        if (!ok) endSend(from);
        focusComposer(from);
      }
      return;
    }
    if (!beginSend(from)) return;
    let ok = false;
    try {
      const transformed = transform ? transform(trimmed) : trimmed;
      let payload = transformed;
      if (curRefs.length > 0) {
        if (curInfo.mode === 'terminal') {
          // Terminal PTY has no chat UI to collapse <yi-context>: save snippets as files and pass @path references
          const atRefs: string[] = [];
          for (const r of curRefs) {
            const ext = r.kind === 'diff' || r.kind === 'review' ? 'diff' : 'txt';
            const name = `${baseName(r.path)}.L${formatLineRange(r.fromLine, r.toLine)}.${ext}`;
            const body = r.kind === 'review' && r.comment ? `# Review comment: ${r.comment}\n\n${r.content}` : r.content;
            const up = await rpc<{ path: string }>(hId, 'session.attachFile', { sessionId: sid, name, data: toB64Str(body) });
            atRefs.push(refOf(up.path));
          }
          payload = `${transformed ? transformed + ' ' : ''}${atRefs.join('')}`.trim();
        } else {
          payload = serializeContextMessage(transformed, curRefs);
        }
      }
      // Auto-name an untitled side chat from its first sent question so the tab label stays readable
      if (curInfo.sideChat && !curInfo.title?.trim() && trimmed) {
        const autoTitle = deriveSideTitle(trimmed);
        if (autoTitle) rpc(hId, 'session.rename', { sessionId: sid, title: autoTitle }).catch(() => {});
      }
      await rpc(hId, 'session.send', {
        sessionId: sid,
        text: payload,
        images: curImgs.map((i) => ({ mimeType: i.mimeType, data: i.data })),
        // Turn still running: Claude and Codex read it at their next step; other tools queue it until the turn ends
        queue: true,
      });
      afterSend?.();
      noteOwnerMessage(curInfo);
      completeSendSuccess(from, { sentText: trimmed, sentImgs: curImgs, sentRefs: curRefs });
      ok = true;
    } catch (e) {
      errorToast(e);
    } finally {
      if (!ok) endSend(from);
      focusComposer(from);
    }
  };

  const send = () => {
    const from = key;
    const hId = hostId;
    const sid = info.id;
    const fallbackInfo = info;
    const transform = transformSend;
    const afterSend = onAfterSend;
    // Files still uploading: spin for now; once done (and the references are in the input box) endUploadJob runs executeSend
    if (getComposerSnapshot(from).pendingUploads > 0) {
      armWaitSend(from, () => {
        void executeSend(from, hId, sid, fallbackInfo, transform, afterSend);
      });
      return;
    }
    return executeSend(from, hId, sid, fallbackInfo, transform, afterSend);
  };

  /** Recall a queued message into the input box (↑ in an empty input box) */
  const recallQueued = async (id: string) => {
    try {
      const r = await rpc<{ text: string }>(hostId, 'session.queue', { sessionId: info.id, id, action: 'remove' });
      restoreMessageToComposer(r.text);
    } catch (e) {
      errorToast(e);
    }
  };

  const cyclePerm = () => {
    if (info.review) return;
    setPermMode(hostId, info, nextPerm(info.tool, info.mode, info.permissionMode));
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    // WebKit can end composition before its confirming keydown; keyCode 229 still identifies the IME key.
    if (e.nativeEvent.isComposing || e.nativeEvent.keyCode === 229) return;
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
  const hasText = !!text.trim() || imgs.length > 0 || refs.length > 0;

  return (
    <>
      <QueueList
        hostId={hostId}
        info={info}
        onRecall={restoreMessageToComposer}
      />
      <div
        className={`composer ${drag ? 'drag' : ''}`}
        onDragOver={(e) => {
          if (e.dataTransfer.types.includes('Files') || e.dataTransfer.types.includes(DRAG_REF)) {
            e.preventDefault();
            e.dataTransfer.dropEffect = 'copy';
            if (connected) setDrag(true);
          }
        }}
        onDragLeave={(e) => {
          if (!e.currentTarget.contains(e.relatedTarget as Node | null)) setDrag(false);
        }}
        onDrop={(e) => {
          const ref = e.dataTransfer.getData(DRAG_REF);
          const hasFiles = e.dataTransfer.types.includes('Files') || e.dataTransfer.files.length > 0;
          if (!ref && !hasFiles) return;
          e.preventDefault();
          setDrag(false);
          if (!connected) return;
          // Dragged in from the file panel: insert @relative-path
          if (ref) {
            window.dispatchEvent(new CustomEvent(INSERT_COMPOSER, { detail: { sessionId: info.id, text: ref } }));
            return;
          }
          void dropFiles(e.dataTransfer);
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
                <button className="attach-x" title={t('remove')} onClick={() => setImgs((cur) => cur.filter((x) => x !== im))}>
                  <Icon name="x" size={12} />
                </button>
              </div>
            ))}
          </div>
        )}
        <ContextRefBar
          refs={refs}
          hostId={hostId}
          sessionId={info.id}
          cwd={info.cwd}
          sending={sending}
          onRemove={(id) => {
            const removed = refs.find((x) => x.id === id);
            setRefs((l) => l.filter((x) => x.id !== id));
            if (removed?.kind === 'review') unadoptFindingByRef(hostId, removed);
          }}
          onEditReview={async (r) => {
            const next = (
              await askText({
                title: t('editReviewComment'),
                label: `${r.path}:${formatLineRange(r.fromLine, r.toLine)}`,
                value: r.comment ?? '',
                placeholder: t('reviewCommentPlaceholder'),
              })
            )?.trim();
            if (next) setRefs((l) => l.map((x) => (x.id === r.id ? { ...x, comment: next } : x)));
          }}
          onSendReviews={() => void send()}
          onClearReviews={() => {
            for (const r of refs) {
              if (r.kind === 'review') unadoptFindingByRef(hostId, r);
            }
            setRefs((l) => l.filter((x) => x.kind !== 'review'));
          }}
        />
        {topSlot}
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
                  : placeholder
                    ? placeholder
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
              onPrewarm?.();
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
            style={{ height: isMobile() && mobH ? mobH : Math.min(200, 18 + text.split('\n').length * 22) }}
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
          {info.review ? (
            <span className="tool-chip readonly-lock" title={t('reviewReadonlyModeLocked')}>
              <Icon name="shield" size={12} />
              <span>{t('readonlyLockedLabel')}</span>
            </span>
          ) : (
            <PermPicker
              tool={info.tool}
              mode={info.mode}
              value={info.permissionMode}
              onChange={(m) => setPermMode(hostId, info, m)}
              open={permOpen}
              setOpen={setPermOpen}
            />
          )}
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
          {desktopControls}
          <span className="composer-spacer" />
          <ModelPicker hostId={hostId} info={info} />
          {running && !hasText && !sending && !waitSend ? (
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
                  : sending
                    ? t('sending')
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
