import { useEffect, useMemo, useRef, useState } from 'react';
import { useBack } from '../back';
import { getT, useLang } from '../i18n';
import { permModes } from '../perm';
import { errorToast, hostStatus, redetectTools, replaceView, useStore, type NewSessionParams } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type DirEntry, type ModelOption, type SessionInfo, type SessionMode, type ToolKind } from '../types';
import { isMobile, lsGet, lsSet, shortPath } from '../util';
import { type Attached, FILE_MAX, IMAGE_TYPES, localPaths, readImage, refOf, uploadChunked } from './Composer';
import { useDrives } from './FilesView';
import { Icon, ToolBadge } from './Icon';
import { openImage } from './Lightbox';
import { ModelMenu } from './ModelPicker';
import { askText, openMenu, type MenuItem } from './Overlay';
import { nextPerm, PermPicker } from './PermPicker';

const tx = getT('newSession');
const tc = getT('composer');

export function DirPicker({
  hostId,
  start,
  onPick,
  onClose,
}: {
  hostId: string;
  start: string;
  onPick: (p: string) => void;
  onClose: () => void;
}) {
  const [path, setPath] = useState(start || '~');
  const [list, setList] = useState<{ path: string; parent: string | null; entries: DirEntry[] } | null>(null);
  const [err, setErr] = useState('');
  const drives = useDrives(hostId);
  useEffect(() => {
    setPath(start || '~');
  }, [hostId, start]);
  useEffect(() => {
    rpc(hostId, 'fs.list', { path })
      .then((r) => {
        setList(r);
        setErr('');
      })
      .catch((e) => {
        // If the start directory doesn't exist on this host, fall back to the home directory so there's still something to pick
        if (path !== '~' && !list) setPath('~');
        else setErr(String(e.message ?? e));
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, path]);
  // New folder in the current directory, then go into it so "Select this directory" picks it (like the native
  // folder dialogs; a nested name such as a/b creates both levels, as in VS Code)
  const newFolder = async () => {
    if (!list) return;
    const name = (await askText({ title: tx('newFolder'), label: list.path, placeholder: tx('folderName') }))?.trim();
    if (!name) return;
    const sep = list.path.includes('\\') && !list.path.includes('/') ? '\\' : '/';
    try {
      const r = await rpc<{ path: string }>(hostId, 'fs.mkdir', { path: list.path.endsWith(sep) ? list.path + name : list.path + sep + name });
      setPath(r.path);
    } catch (e) {
      setErr(String((e as Error).message ?? e));
    }
  };
  return (
    <div className="dirpicker">
      <div className="dirpicker-head">
        <button className="icon-btn" disabled={!list?.parent} onClick={() => list?.parent && setPath(list.parent)} title={tx('up')}>
          <Icon name="up" size={16} />
        </button>
        <span className="mono truncate grow">{list?.path ?? path}</span>
        {drives.length > 1 && (
          <select value="" onChange={(e) => e.target.value && setPath(e.target.value)} title={tx('switchDrive')}>
            <option value="">{tx('drive')}</option>
            {drives.map((d) => (
              <option key={d} value={d}>
                {d}
              </option>
            ))}
          </select>
        )}
        <button className="icon-btn" disabled={!list} onClick={newFolder} title={tx('newFolder')}>
          <Icon name="new-folder" size={16} />
        </button>
      </div>
      {err && <div className="error-text">{err}</div>}
      <div className="dirpicker-list">
        {list?.entries
          .filter((e) => e.isDir)
          .map((e) => (
            <button key={e.path} onClick={() => setPath(e.path)}>
              <Icon name="folder" size={14} /> {e.name}
            </button>
          ))}
      </div>
      <div className="row end">
        <button className="btn" onClick={onClose}>
          {tx('cancel')}
        </button>
        <button className="btn primary" disabled={!list} onClick={() => list && onPick(list.path)}>
          {tx('selectDirectory')}
        </button>
      </div>
    </div>
  );
}

/** Last choices per tool when creating a session: UI, permission mode, model, thinking level (stored in localStorage `new-session-prefs`) */
type ToolPrefs = { mode?: 'structured' | 'terminal'; perm?: string; model?: string; effort?: string };
const PREFS_KEY = 'new-session-prefs';
const RECENT_KEY = 'recent-dirs';
const RECENT_MAX = 8;
/** At most this many directories in the directory menu (recent ones first, then those of existing sessions) */
const DIR_MENU_MAX = 10;

/** Directories used recently on a host, newest first (localStorage `recent-dirs`; falls back to the single `recent-cwd` older versions kept) */
function recentDirs(hostId: string): string[] {
  const all = lsGet<Record<string, string[]>>(RECENT_KEY, {});
  if (all[hostId]) return all[hostId];
  const old = lsGet<Record<string, string>>('recent-cwd', {})[hostId];
  return old ? [old] : [];
}

function rememberDir(hostId: string, dir: string) {
  const all = lsGet<Record<string, string[]>>(RECENT_KEY, {});
  all[hostId] = [dir, ...recentDirs(hostId).filter((d) => d !== dir)].slice(0, RECENT_MAX);
  lsSet(RECENT_KEY, all);
}

/** The tool selected when the page opens */
function initialTool(ns: NewSessionParams): ToolKind {
  // Continue with another tool: default to the last tool used; if that's the original tool, pick the other of claude / codex first (switched below if not installed)
  const last = lsGet<ToolKind>('last-tool', 'claude');
  const from = ns.handoff?.from;
  let pick = (ns.tool as ToolKind) ?? (from && (last === from || last === 'shell' || last === 'files') ? (from === 'claude' ? 'codex' : 'claude') : last);
  const avoid = ns.handoff?.avoid ?? [];
  if (ns.handoff && avoid.includes(pick)) {
    // Waiting for its own limit to reset: the first installed tool that isn't, if any
    const alt = hostStatus(ns.hostId).hello?.tools.find(
      (t) => t.available && t.kind !== from && t.kind !== 'shell' && t.kind !== 'files' && !avoid.includes(t.kind),
    );
    if (alt) pick = alt.kind;
  }
  return pick;
}

/** A file added to the first message: images are sent with it in chat mode, everything else is uploaded after the session exists */
interface Pending {
  id: number;
  file: File;
  img?: Attached;
}
let pendingSeq = 0;

const baseName = (p: string) => p.split(/[\\/]/).filter(Boolean).pop() || p;

/** Open a menu under the button that was clicked */
const menuUnder = (e: React.MouseEvent<HTMLElement>, items: (MenuItem | '-')[]) => {
  const r = e.currentTarget.getBoundingClientRect();
  openMenu({ clientX: r.left, clientY: r.bottom + 4 }, items);
};

/**
 * The new session page: the input box for the first message is the main thing; where to run (host, directory) and
 * what to run (tool, UI) are chips above it, permission mode and model sit in its bottom bar as in a session
 */
export function NewSessionPage({ params }: { params: NewSessionParams }) {
  // The page is keyed by its parameters (App.tsx), so they never change while it's mounted; keep one object for effect deps
  const [ns] = useState(params);
  const [, lang] = useLang();
  const hosts = useStore((s) => s.hosts);
  const hostSessions = useStore((s) => s.sessions);
  const [hostId, setHostId] = useState(ns.hostId);
  const [cwd, setCwd] = useState(() => ns.cwd ?? recentDirs(ns.hostId)[0] ?? '~');
  const [tool, setTool] = useState<ToolKind>(() => initialTool(ns));
  const [mode, setMode] = useState<'structured' | 'terminal'>('structured');
  const [perm, setPerm] = useState('default');
  const [model, setModel] = useState('');
  const [effort, setEffort] = useState('');
  const [text, setText] = useState(ns.handoff?.prompt ?? '');
  const [items, setItems] = useState<Pending[]>([]);
  const [picking, setPicking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [progress, setProgress] = useState<{ name: string; sent: number; total: number } | null>(null);
  const [detecting, setDetecting] = useState(false);
  const [models, setModels] = useState<ModelOption[] | null>(null);
  const [permOpen, setPermOpen] = useState(false);
  const [drag, setDrag] = useState(false);
  const ta = useRef<HTMLTextAreaElement>(null);
  const cmdInput = useRef<HTMLInputElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const imageInput = useRef<HTMLInputElement>(null);

  const host = hosts.find((h) => h.config.id === hostId);
  const st = host?.status ?? {};
  const connected = st.status === 'connected';
  const tools = st.hello?.tools ?? [];
  const home = st.hello?.home;
  const connectedHosts = hosts.filter((h) => h.status.status === 'connected');
  // Terminal and Files sessions don't talk to an AI tool: no UI choice, permission mode, model or first message
  const aiTool = tool !== 'shell' && tool !== 'files';
  const effectiveMode: SessionMode = tool === 'shell' ? 'terminal' : tool === 'files' ? 'structured' : mode;
  // Continue with another tool: the selection must be an installed tool other than the original (if only the original is installed, it stays on a greyed-out chip)
  const handoffBlocked = !!ns.handoff && (tool === ns.handoff.from || !aiTool || !tools.some((t) => t.kind === tool && t.available));
  // Continuing a history session keeps it as it was: no first message
  const hasInput = tool !== 'files' && !ns.resumeId;
  const canAttach = hasInput && aiTool;

  // The tool's available models: returned directly if yi-agentd has them cached, otherwise it starts the tool once to ask (a few seconds)
  useEffect(() => {
    setModels(null);
    if (!hostId || !aiTool || !connected) return;
    let alive = true;
    rpc<ModelOption[]>(hostId, 'tools.models', { tool })
      .then((m) => alive && setModels(m))
      .catch(() => alive && setModels([]));
    return () => {
      alive = false;
    };
  }, [hostId, tool, aiTool, connected]);

  // When the tool changes (including on open), bring back that tool's last choices
  useEffect(() => {
    const p = lsGet<Record<string, ToolPrefs>>(PREFS_KEY, {})[tool] ?? {};
    // Continue with another tool only uses chat mode: the first message has to be sent through chat mode
    const m = ns.handoff ? 'structured' : (p.mode ?? lsGet<'structured' | 'terminal'>('last-mode', 'structured'));
    setMode(m);
    // Continue with another tool keeps the original session's permission mode when this tool has it, else default
    const want = ns.handoff ? ns.handoff.perm || 'default' : p.perm;
    setPerm(want && permModes(tool, m).some((x) => x[0] === want) ? want : 'default');
    setModel(p.model ?? '');
    setEffort(p.effort ?? '');
  }, [ns, tool]);

  // The default tool (last used, or claude) isn't installed on this host: pick the first installed one instead of leaving a greyed-out chip selected
  useEffect(() => {
    if (ns.resumeId || !tools.length) return;
    const usable = (k: ToolKind) => !ns.handoff || (k !== ns.handoff.from && k !== 'shell' && k !== 'files');
    if (usable(tool) && tools.some((t) => t.kind === tool && t.available)) return;
    const first = tools.find((t) => t.available && usable(t.kind));
    if (first) setTool(first.kind);
  }, [ns, hostId, tools, tool]);

  useEffect(() => {
    if (isMobile()) return;
    const el = ta.current ?? cmdInput.current;
    el?.focus();
    el?.setSelectionRange(el.value.length, el.value.length);
  }, [tool, hostId]);

  useBack(picking, () => setPicking(false));

  // The list shown as "tool default" in the model menu: the tool's own default entry if it has one (Claude), else one that
  // stands for no --model, with the thinking levels of the first model (what the tool would use)
  const modelList = useMemo(() => {
    if (!models) return null;
    if (models.some((m) => m.id === 'default')) return models;
    return [{ id: '', name: tx('toolDefault'), efforts: models[0]?.efforts, defaultEffort: models[0]?.defaultEffort }, ...models];
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [models, lang]);
  // Thinking levels available for the current model; don't send a remembered level the model doesn't support
  const curModel = modelList?.find((x) => x.id === model) ?? (!model ? modelList?.[0] : undefined);
  const effortList = curModel?.efforts ?? [];
  const sendEffort = effort && (models === null || effortList.includes(effort)) ? effort : '';

  // Directory menu: directories used recently from this page, then those of this host's sessions (newest first)
  const dirs = useMemo(() => {
    const fromSessions = (hostSessions[hostId] ?? [])
      .filter((s) => !s.parentId)
      .sort((a, b) => b.updatedAt - a.updatedAt)
      .map((s) => s.cwd);
    return [...new Set([...recentDirs(hostId), ...fromSessions])].slice(0, DIR_MENU_MAX);
  }, [hostId, hostSessions]);

  const switchHost = (id: string) => {
    if (id === hostId) return;
    // After switching hosts the old directory probably doesn't exist there; use the directory last used on that host
    setHostId(id);
    setCwd(recentDirs(id)[0] ?? '~');
  };

  const switchMode = (m: 'structured' | 'terminal') => {
    setMode(m);
    // Some tools have fewer permission modes in the terminal UI
    if (!permModes(tool, m).some((x) => x[0] === perm)) setPerm('default');
  };

  const addFiles = async (fs: File[]) => {
    const local = host?.config.kind === 'local' ? await localPaths(fs.filter((f) => !IMAGE_TYPES.includes(f.type))) : null;
    const refs: string[] = [];
    for (const f of fs) {
      try {
        if (IMAGE_TYPES.includes(f.type)) {
          const img = await readImage(f);
          setItems((l) => [...l, { id: ++pendingSeq, file: f, img }]);
          continue;
        }
        // A local session can use the original file on this machine: reference its path, nothing to upload
        const at = local?.find((p) => baseName(p) === f.name);
        if (at) {
          refs.push(refOf(at));
          continue;
        }
        if (f.size > FILE_MAX) throw new Error(tc('exceeds50mb', { name: f.name }));
        if (!f.size && !f.type) throw new Error(tc('isAFolderOrEmpty', { name: f.name }));
        setItems((l) => [...l, { id: ++pendingSeq, file: f }]);
      } catch (e) {
        errorToast(e);
      }
    }
    if (refs.length) setText((cur) => cur + (cur && !/\s$/.test(cur) ? ' ' : '') + refs.join(''));
  };

  const redetect = async () => {
    setDetecting(true);
    try {
      await redetectTools(hostId);
    } catch (e) {
      errorToast(e);
    } finally {
      setDetecting(false);
    }
  };

  const create = async () => {
    if (busy || !cwd.trim() || handoffBlocked || !connected) return;
    setBusy(true);
    const chat = aiTool && effectiveMode === 'structured';
    const prompt = hasInput && aiTool ? text.trim() : '';
    // Chat mode sends images with the message; files (and images in terminal mode) are uploaded into the new session
    // first and referenced as @paths, so then the message goes out with session.send once they are there
    const images = chat ? items.filter((i) => i.img).map((i) => ({ mimeType: i.img!.mimeType, data: i.img!.data })) : [];
    const uploads = aiTool && hasInput ? items.filter((i) => !(chat && i.img)) : [];
    const later = uploads.length > 0;
    let info: SessionInfo | null = null;
    let message = prompt;
    try {
      info = await rpc<SessionInfo>(hostId, 'session.create', {
        tool,
        cwd: cwd.trim(),
        mode: effectiveMode,
        prompt: later ? undefined : prompt || undefined,
        images: later || !images.length ? undefined : images,
        permissionMode: aiTool && permModes(tool, mode).some((m) => m[0] === perm) ? perm : 'default',
        model: aiTool ? model || undefined : undefined,
        effort: aiTool ? sendEffort || undefined : undefined,
        command: tool === 'shell' ? text.trim() || undefined : undefined,
        resumeId: ns.resumeId,
        handoffFrom: ns.handoff?.fromId,
        title: ns.title,
        cols: 120,
        rows: 32,
      });
      rememberDir(hostId, info.cwd);
      lsSet('last-tool', tool);
      if (aiTool) {
        // Continue with another tool always uses chat mode; don't record that as the user's chosen UI
        if (!ns.handoff) lsSet('last-mode', mode);
        const all = lsGet<Record<string, ToolPrefs>>(PREFS_KEY, {});
        all[tool] = { mode: ns.handoff ? all[tool]?.mode : mode, perm: ns.handoff ? all[tool]?.perm : perm, model, effort: sendEffort };
        lsSet(PREFS_KEY, all);
      }
      if (later) {
        const refs: string[] = [];
        for (const it of uploads) {
          setProgress({ name: it.file.name, sent: 0, total: it.file.size });
          const path = await uploadChunked(hostId, info.id, it.file, (sent) => setProgress((p) => p && { ...p, sent }));
          if (path) refs.push(refOf(path).trim());
        }
        setProgress(null);
        message = [prompt, ...refs].filter(Boolean).join(' ');
        await rpc(hostId, 'session.send', { sessionId: info.id, text: message, images, queue: true });
      }
      replaceView({ kind: 'session', hostId, sessionId: info.id });
    } catch (e) {
      errorToast(e);
      setProgress(null);
      if (info) {
        // The session exists but the first message didn't go out: open it with the message in its input box
        try {
          if (message) sessionStorage.setItem(`draft-${hostId}::${info.id}`, message);
        } catch {
          /* storage unavailable */
        }
        replaceView({ kind: 'session', hostId, sessionId: info.id });
      }
    } finally {
      setBusy(false);
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.nativeEvent.isComposing) return;
    if (e.key === 'Tab' && e.shiftKey && aiTool) {
      e.preventDefault();
      setPerm(nextPerm(tool, mode, perm));
      return;
    }
    if (e.key === 'Enter' && !e.shiftKey && !isMobile()) {
      e.preventDefault();
      create();
    }
  };

  const toolDisabled = (k: ToolKind, available: boolean) =>
    !available || (!!ns.resumeId && k !== ns.tool) || (!!ns.handoff && (k === ns.handoff.from || k === 'shell' || k === 'files'));
  const hostLocked = !!ns.resumeId || !!ns.handoff || connectedHosts.length < 2;
  const toolLocked = !!ns.resumeId;
  const curTool = tools.find((t) => t.kind === tool);

  const title = ns.resumeId
    ? tx('resumeSession')
    : ns.handoff
      ? tx('continueInAnotherTool')
      : tool === 'shell'
        ? tx('titleShell')
        : tool === 'files'
          ? tx('titleFiles')
          : tx('whatShouldItDo');
  const placeholder = !connected
    ? tx('hostNotConnected')
    : tool === 'shell'
      ? tx('commandLeaveEmptyForDefault')
      : effectiveMode === 'terminal'
        ? tx('firstMessageTerminal')
        : ns.handoff
          ? tx('initialPrompt')
          : tx('describeTask');
  const goLabel = busy
    ? aiTool || tool === 'shell'
      ? tx('starting')
      : tx('creating')
    : ns.resumeId
      ? tx('resume')
      : tool === 'files' || tool === 'shell'
        ? tx('open')
        : tx('create');
  const canGo = !busy && !!cwd.trim() && !handoffBlocked && connected;

  return (
    <div className="page ns-page">
      <div className="ns-center">
        <h2 className="ns-title">{title}</h2>
        <div className="ns-chips">
          <button
            type="button"
            className="ns-chip"
            disabled={hostLocked}
            title={tx('host')}
            onClick={(e) =>
              menuUnder(
                e,
                connectedHosts.map((h) => ({
                  label: h.config.name,
                  icon: h.config.kind === 'local' ? 'laptop' : 'server',
                  active: h.config.id === hostId,
                  run: () => switchHost(h.config.id),
                })),
              )
            }
          >
            <Icon name={host?.config.kind === 'local' ? 'laptop' : 'server'} size={14} />
            <span className="truncate">{host?.config.name ?? hostId}</span>
            {!hostLocked && <Icon name="down" size={11} />}
          </button>
          <button
            type="button"
            className="ns-chip"
            title={cwd}
            onClick={(e) =>
              menuUnder(e, [
                ...dirs.map((d) => ({ label: shortPath(d, home), icon: 'folder', active: d === cwd, run: () => setCwd(d) })),
                '-',
                { label: tx('browse'), icon: 'folder', run: () => setPicking(true) },
                {
                  label: tx('enterPath'),
                  icon: 'edit',
                  run: async () => {
                    const v = (await askText({ title: tx('workingDirectory'), value: cwd, placeholder: '~/project' }))?.trim();
                    if (v) setCwd(v);
                  },
                },
              ])
            }
          >
            <Icon name="folder" size={14} />
            <span className="truncate">{baseName(shortPath(cwd, home))}</span>
            <Icon name="down" size={11} />
          </button>
          <button
            type="button"
            className="ns-chip"
            disabled={toolLocked}
            title={curTool?.version ?? curTool?.path ?? tx('tool')}
            onClick={(e) =>
              menuUnder(e, [
                ...tools.map((t) => ({
                  label: t.available ? TOOL_LABEL[t.kind] : tx('toolNotInstalled', { tool: TOOL_LABEL[t.kind] }),
                  tool: t.kind,
                  active: t.kind === tool,
                  disabled: toolDisabled(t.kind, t.available),
                  run: () => setTool(t.kind),
                })),
                '-',
                { label: detecting ? tx('redetecting') : tx('redetectTools'), icon: 'refresh', disabled: detecting, run: redetect },
              ])
            }
          >
            <ToolBadge tool={tool} size={14} />
            <span className="truncate">{TOOL_LABEL[tool]}</span>
            {detecting && <span className="spinner mini" />}
            {!toolLocked && <Icon name="down" size={11} />}
          </button>
          {aiTool && !ns.handoff && (
            <button
              type="button"
              className="ns-chip"
              title={tx('interface')}
              onClick={(e) =>
                menuUnder(e, [
                  { label: tx('chat'), icon: 'chat', active: mode === 'structured', run: () => switchMode('structured') },
                  { label: tx('terminal'), icon: 'terminal', active: mode === 'terminal', run: () => switchMode('terminal') },
                ])
              }
            >
              <Icon name={mode === 'terminal' ? 'terminal' : 'chat'} size={14} />
              <span>{mode === 'terminal' ? tx('terminal') : tx('chat')}</span>
              <Icon name="down" size={11} />
            </button>
          )}
          {tool === 'files' && (
            <button type="button" className="btn primary ns-open" disabled={!canGo} onClick={create}>
              {goLabel}
            </button>
          )}
        </div>
        {tool === 'files' ? null : tool === 'shell' ? (
          // A command is one line: a prompt, the command and an Open button
          <div className="composer ns-composer ns-shell">
            <span className="ns-prompt mono" aria-hidden="true">
              $
            </span>
            <input
              ref={cmdInput}
              className="mono"
              value={text}
              placeholder={placeholder}
              disabled={!connected || busy}
              onChange={(e) => setText(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Enter' && !e.nativeEvent.isComposing) {
                  e.preventDefault();
                  create();
                }
              }}
            />
            <button type="button" className="btn primary ns-open" disabled={!canGo} onClick={create}>
              {busy && <span className="spinner mini" />}
              {goLabel}
            </button>
          </div>
        ) : (
          <div
            className={`composer ns-composer ${drag ? 'drag' : ''}`}
            onDragOver={(e) => {
              if (canAttach && e.dataTransfer.types.includes('Files')) {
                e.preventDefault();
                setDrag(true);
              }
            }}
            onDragLeave={() => setDrag(false)}
            onDrop={(e) => {
              e.preventDefault();
              setDrag(false);
              if (canAttach) addFiles(Array.from(e.dataTransfer.files));
            }}
          >
            {progress && (
              <div className="upload-row" aria-live="polite">
                <span className="upload-chip" title={progress.name}>
                  <span className="spinner mini" />
                  <span className="truncate">{tc('uploading', { name: progress.name })}</span>
                  {progress.total > 0 && <span className="upload-pct">{Math.floor((progress.sent / progress.total) * 100)}%</span>}
                </span>
              </div>
            )}
            {canAttach && items.some((i) => !i.img) && (
              <div className="upload-row">
                {items
                  .filter((i) => !i.img)
                  .map((i) => (
                    <span key={i.id} className="upload-chip" title={i.file.name}>
                      <Icon name="file" size={12} />
                      <span className="truncate">{i.file.name}</span>
                      <button className="upload-x" title={tc('remove')} aria-label={tc('remove')} disabled={busy} onClick={() => setItems((l) => l.filter((x) => x.id !== i.id))}>
                        <Icon name="x" size={12} />
                      </button>
                    </span>
                  ))}
              </div>
            )}
            {canAttach && items.some((i) => i.img) && (
              <div className="attach-row">
                {items
                  .filter((i) => i.img)
                  .map((i) => (
                    <div key={i.id} className="attach" title={tc('doubleClickToZoom', { name: i.img!.name })}>
                      <img src={i.img!.url} alt="" onDoubleClick={() => openImage(i.img!.url)} />
                      <button className="attach-x" title={tc('remove')} disabled={busy} onClick={() => setItems((l) => l.filter((x) => x.id !== i.id))}>
                        <Icon name="x" size={12} />
                      </button>
                    </div>
                  ))}
              </div>
            )}
            {hasInput && (
              <div className="composer-row">
                <textarea
                  ref={ta}
                  value={text}
                  rows={1}
                  placeholder={placeholder}
                  disabled={!connected || busy}
                  onChange={(e) => setText(e.target.value)}
                  onKeyDown={onKeyDown}
                  onPaste={(e) => {
                    const fs = Array.from(e.clipboardData.files);
                    if (fs.length && canAttach) {
                      e.preventDefault();
                      addFiles(fs);
                    }
                  }}
                  style={{ height: Math.min(ns.handoff ? 320 : 240, 22 + text.split('\n').length * 20) }}
                />
              </div>
            )}
            <div className="composer-tools">
              {canAttach && (
                <>
                  <button
                    type="button"
                    className="icon-btn add-btn"
                    title={tc('attachImagesOrFilesOr')}
                    disabled={!connected || busy}
                    onClick={(e) => {
                      if (!isMobile()) return fileInput.current?.click();
                      const r = e.currentTarget.getBoundingClientRect();
                      openMenu({ clientX: r.left, clientY: r.top - 96 }, [
                        { label: tc('photoLibrary'), icon: 'image', run: () => imageInput.current?.click() },
                        { label: tc('file'), icon: 'file', run: () => fileInput.current?.click() },
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
                      if (e.target.files) addFiles(Array.from(e.target.files));
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
                      if (e.target.files) addFiles(Array.from(e.target.files));
                      e.target.value = '';
                    }}
                  />
                </>
              )}
              {aiTool && (
                <PermPicker tool={tool} mode={mode} value={perm} onChange={setPerm} open={permOpen} setOpen={setPermOpen} />
              )}
              <span className="composer-spacer" />
              {aiTool && (
                <ModelMenu
                  tool={tool}
                  model={model}
                  effort={sendEffort}
                  models={modelList}
                  resetKey={`${hostId}/${tool}`}
                  onModel={(id) => {
                    setModel(id);
                    // A level the new model doesn't have is dropped when sending; clear it so the button doesn't show it
                    const m = modelList?.find((x) => x.id === id);
                    if (effort && m?.efforts && !m.efforts.includes(effort)) setEffort('');
                  }}
                  onEffort={setEffort}
                  onCustom={async () => {
                    const v = (await askText({ title: tx('model'), value: model, placeholder: tx('leaveEmptyForToolDefault') }))?.trim();
                    if (v !== undefined) setModel(v);
                  }}
                />
              )}
              <button
                type="button"
                className={`send-btn ${busy ? 'loading' : ''}`}
                disabled={!canGo}
                onClick={create}
                title={goLabel}
                aria-label={goLabel}
              >
                {busy ? <span className="spinner send-spin" /> : <Icon name="arrowUp" size={18} />}
              </button>
            </div>
          </div>
        )}
        {handoffBlocked && tools.length > 0 && <p className="muted small ns-note">{tx('noOtherToolIsInstalled')}</p>}
      </div>
      {picking && (
        <div className="modal-bg" onClick={() => setPicking(false)}>
          <div className="modal" onClick={(e) => e.stopPropagation()}>
            <div className="modal-head">
              <h3>{tx('workingDirectory')}</h3>
              <button className="icon-btn" onClick={() => setPicking(false)} aria-label={tx('cancel')}>
                <Icon name="x" />
              </button>
            </div>
            <DirPicker
              hostId={hostId}
              start={cwd}
              onPick={(p) => {
                setCwd(p);
                setPicking(false);
              }}
              onClose={() => setPicking(false)}
            />
          </div>
        </div>
      )}
    </div>
  );
}
