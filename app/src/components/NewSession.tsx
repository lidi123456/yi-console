import { useEffect, useState } from 'react';
import { useBack } from '../back';
import { getT, isEn } from '../i18n';
import { errorToast, hostStatus, navigate, redetectTools, setState, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type DirEntry, type ModelOption, type SessionInfo, type SessionMode, type ToolKind } from '../types';
import { lsGet, lsSet, shortPath } from '../util';
import { useDrives } from './FilesView';
import { Icon, ToolBadge } from './Icon';
import { EFFORT_LABEL } from './ModelPicker';

const tx = getT('newSession');

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

const PERM_MODES_ZH: [string, string, string][] = [
  ['default', '默认', '按工具自身规则询问审批'],
  ['acceptEdits', '自动接受编辑', '改文件不再询问'],
  ['auto', '自动审批', '由工具自己判断放不放行，有风险的操作仍会拦下'],
  ['plan', '只读规划', '只分析不改动'],
  ['bypass', '全部放行', '不询问任何操作，谨慎使用'],
];

const PERM_MODES_EN: [string, string, string][] = [
  ['default', 'Default', 'Ask for approval according to tool rules'],
  ['acceptEdits', 'Accept Edits', 'Auto-approve file edits'],
  ['auto', 'Auto', 'Let the tool decide; risky actions still require approval'],
  ['plan', 'Plan', 'Read-only planning without making changes'],
  ['bypass', 'Bypass All', 'Skip all permission prompts (use with caution)'],
];

export function getPermModesList(): [string, string, string][] {
  return isEn() ? PERM_MODES_EN : PERM_MODES_ZH;
}

export const PERM_MODES = PERM_MODES_ZH;

/** Permission modes each tool actually supports; keep in sync with yi-agentd's `permission_modes` */
export function permModes(tool: ToolKind, mode: SessionMode) {
  const ids: Record<ToolKind, string[]> = {
    claude: ['default', 'acceptEdits', 'auto', 'plan', 'bypass'],
    codex: mode === 'structured' ? ['default', 'acceptEdits', 'auto', 'plan', 'bypass'] : ['default', 'auto', 'plan', 'bypass'],
    cursor: mode === 'structured' ? ['default', 'acceptEdits', 'plan', 'bypass'] : ['default', 'plan', 'bypass'],
    kiro: mode === 'structured' ? ['default', 'acceptEdits', 'plan', 'bypass'] : ['default', 'bypass'],
    qoder: ['default', 'acceptEdits', 'bypass'],
    codebuddy: ['default', 'acceptEdits', 'plan', 'bypass'],
    shell: [],
    files: [],
  };
  return getPermModesList().filter((m) => ids[tool].includes(m[0]));
}

/** Last choices per tool when creating a session: UI, permission mode, model, thinking level (stored in localStorage `new-session-prefs`) */
type ToolPrefs = { mode?: 'structured' | 'terminal'; perm?: string; model?: string; effort?: string };
const PREFS_KEY = 'new-session-prefs';

export function NewSessionDialog() {
  const ns = useStore((s) => s.newSession);
  const hosts = useStore((s) => s.hosts);
  const [hostId, setHostId] = useState('');
  const [tool, setTool] = useState<ToolKind>('claude');
  const [mode, setMode] = useState<'structured' | 'terminal'>('structured');
  const [cwd, setCwd] = useState('');
  const [prompt, setPrompt] = useState('');
  const [perm, setPerm] = useState('default');
  const [model, setModel] = useState('');
  const [effort, setEffort] = useState('');
  const [command, setCommand] = useState('');
  const [picking, setPicking] = useState(false);
  const [busy, setBusy] = useState(false);
  const [detecting, setDetecting] = useState(false);
  const [models, setModels] = useState<ModelOption[] | null>(null);
  const [customModel, setCustomModel] = useState(false);

  // The tool's available models: returned directly if yi-agentd has them cached, otherwise it starts the tool once to ask (a few seconds)
  useEffect(() => {
    setModels(null);
    setCustomModel(false);
    if (!ns || !hostId || tool === 'shell' || tool === 'files') return;
    let alive = true;
    rpc<ModelOption[]>(hostId, 'tools.models', { tool })
      .then((m) => {
        if (!alive) return;
        setModels(m);
        // The remembered model isn't in this host's list (typed by hand, or a different host): show it in the text input instead of letting the dropdown swallow it
        setModel((cur) => {
          if (cur && m.length > 0 && !m.some((x) => x.id === cur)) setCustomModel(true);
          return cur;
        });
      })
      .catch(() => alive && setModels([]));
    return () => {
      alive = false;
    };
  }, [ns, hostId, tool]);

  useEffect(() => {
    if (!ns) return;
    setHostId(ns.hostId);
    const recent = lsGet<Record<string, string>>('recent-cwd', {});
    setCwd(ns.cwd ?? recent[ns.hostId] ?? '~');
    // Continue with another tool: default to the last tool used; if that's the original tool, pick the other of claude / codex first (switched below if not installed)
    const last = lsGet<ToolKind>('last-tool', 'claude');
    const from = ns.handoff?.from;
    setTool((ns.tool as ToolKind) ?? (from && (last === from || last === 'shell' || last === 'files') ? (from === 'claude' ? 'codex' : 'claude') : last));
    setPrompt(ns.handoff?.prompt ?? '');
    setCommand('');
    setPicking(false);
  }, [ns]);

  // When the tool changes (including on open), bring back that tool's last choices
  useEffect(() => {
    if (!ns) return;
    const p = lsGet<Record<string, ToolPrefs>>(PREFS_KEY, {})[tool] ?? {};
    // Continue with another tool only uses chat mode: the first message has to be sent through chat mode
    const m = ns.handoff ? 'structured' : (p.mode ?? lsGet<'structured' | 'terminal'>('last-mode', 'structured'));
    setMode(m);
    setPerm(p.perm && permModes(tool, m).some((x) => x[0] === p.perm) ? p.perm : 'default');
    setModel(p.model ?? '');
    setEffort(p.effort ?? '');
  }, [ns, tool]);

  // The default tool (last used, or claude) isn't installed on this host: pick the first installed one instead of leaving a greyed-out button selected
  const helloTools = hostStatus(hostId).hello?.tools;
  useEffect(() => {
    if (!ns || ns.resumeId || !helloTools?.length) return;
    const usable = (k: ToolKind) => !ns.handoff || (k !== ns.handoff.from && k !== 'shell' && k !== 'files');
    if (usable(tool) && helloTools.some((t) => t.kind === tool && t.available)) return;
    const first = helloTools.find((t) => t.available && usable(t.kind));
    if (first) setTool(first.kind);
  }, [ns, hostId, helloTools, tool]);

  useBack(!!ns, () => setState({ newSession: null }));
  useBack(!!ns && picking, () => setPicking(false));
  if (!ns) return null;
  const st = hostStatus(hostId);
  const tools = st.hello?.tools ?? [];
  const connectedHosts = hosts.filter((h) => h.status.status === 'connected');
  const close = () => setState({ newSession: null });
  // Terminal and Files sessions don't talk to an AI tool: no UI choice, permission mode, model or prompt
  const aiTool = tool !== 'shell' && tool !== 'files';
  const effectiveMode = tool === 'shell' ? 'terminal' : tool === 'files' ? 'structured' : mode;
  // Continue with another tool: the selection must be an installed tool other than the original (if only the original is installed, it stays on a greyed-out button)
  const handoffBlocked = !!ns.handoff && (tool === ns.handoff.from || !aiTool || !tools.some((t) => t.kind === tool && t.available));

  // Thinking levels available for the current model; don't send a remembered level the model doesn't support
  const curModel = models?.find((x) => x.id === model) ?? (!model ? models?.[0] : undefined);
  const effortList = curModel?.efforts ?? [];
  const sendEffort = effort && (models === null || effortList.includes(effort)) ? effort : '';

  const create = async () => {
    setBusy(true);
    try {
      const info = await rpc<SessionInfo>(hostId, 'session.create', {
        tool,
        cwd,
        mode: effectiveMode,
        prompt: aiTool && effectiveMode === 'structured' ? prompt : undefined,
        permissionMode: aiTool && permModes(tool, mode).some((m) => m[0] === perm) ? perm : 'default',
        model: aiTool ? model || undefined : undefined,
        effort: aiTool ? sendEffort || undefined : undefined,
        command: tool === 'shell' ? command || undefined : undefined,
        resumeId: ns.resumeId,
        title: ns.title,
        cols: 120,
        rows: 32,
      });
      const recent = lsGet<Record<string, string>>('recent-cwd', {});
      recent[hostId] = info.cwd;
      lsSet('recent-cwd', recent);
      lsSet('last-tool', tool);
      if (aiTool) {
        // Continue with another tool always uses chat mode; don't record that as the user's chosen UI
        if (!ns.handoff) lsSet('last-mode', mode);
        const all = lsGet<Record<string, ToolPrefs>>(PREFS_KEY, {});
        all[tool] = { mode: ns.handoff ? all[tool]?.mode : mode, perm, model, effort: sendEffort };
        lsSet(PREFS_KEY, all);
      }
      close();
      navigate({ kind: 'session', hostId, sessionId: info.id });
    } catch (e) {
      errorToast(e);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="modal-bg" onClick={close}>
      <div className="modal ns-modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h3>{ns.resumeId ? tx('resumeSession') : ns.handoff ? tx('continueInAnotherTool') : tx('newSession')}</h3>
          <button className="icon-btn" onClick={close}>
            <Icon name="x" />
          </button>
        </div>
        {picking ? (
          <DirPicker
            hostId={hostId}
            start={cwd}
            onPick={(p) => {
              setCwd(p);
              setPicking(false);
            }}
            onClose={() => setPicking(false)}
          />
        ) : (
          <div className="form">
            {/* Continue with another tool: the original session's log is on the original host, so the host can't change */}
            {connectedHosts.length > 1 && !ns.resumeId && !ns.handoff && (
              <label>
                {tx('host')}
                <select
                  value={hostId}
                  onChange={(e) => {
                    // After switching hosts the old directory probably doesn't exist there; use the directory last used on that host
                    const id = e.target.value;
                    setHostId(id);
                    setCwd(lsGet<Record<string, string>>('recent-cwd', {})[id] ?? '~');
                    setPicking(false);
                  }}
                >
                  {connectedHosts.map((h) => (
                    <option key={h.config.id} value={h.config.id}>
                      {h.config.name}
                    </option>
                  ))}
                </select>
              </label>
            )}
            <div className="field">
              <span className="field-head">
                {tx('tool')}
                <button
                  type="button"
                  className="link-btn tool-redetect"
                  disabled={detecting || !hostId}
                  title={tx('redetectToolsHint')}
                  onClick={async () => {
                    setDetecting(true);
                    try {
                      await redetectTools(hostId);
                    } catch (e) {
                      errorToast(e);
                    } finally {
                      setDetecting(false);
                    }
                  }}
                >
                  {detecting ? <span className="spinner" /> : <Icon name="refresh" size={12} />}
                  {detecting ? tx('redetecting') : tx('redetectTools')}
                </button>
              </span>
              <div className="tool-grid">
                {tools.map((t) => (
                  <button
                    key={t.kind}
                    disabled={
                      !t.available || (!!ns.resumeId && t.kind !== ns.tool) || (!!ns.handoff && (t.kind === ns.handoff.from || t.kind === 'shell' || t.kind === 'files'))
                    }
                    className={`tool-opt ${tool === t.kind ? 'active' : ''}`}
                    onClick={() => setTool(t.kind)}
                    title={t.available ? (t.version ?? t.path ?? '') : tx('notInstalledOnThisHost')}
                  >
                    <ToolBadge tool={t.kind} size={18} />
                    <span className="tool-opt-label">
                      {TOOL_LABEL[t.kind]}
                      {!t.available && <span className="tool-opt-tag">{tx('notInstalled')}</span>}
                    </span>
                  </button>
                ))}
              </div>
            </div>
            {aiTool && !ns.handoff && (
              <div className="field">
                <span>{tx('interface')}</span>
                <div className="seg">
                  <button className={mode === 'structured' ? 'active' : ''} onClick={() => setMode('structured')}>
                    <Icon name="chat" size={14} /> {tx('chat')}
                  </button>
                  <button className={mode === 'terminal' ? 'active' : ''} onClick={() => setMode('terminal')}>
                    <Icon name="terminal" size={14} /> {tx('terminal')}
                  </button>
                </div>
              </div>
            )}
            <label>
              {tx('workingDirectory')}
              <div className="row">
                <input value={cwd} onChange={(e) => setCwd(e.target.value)} placeholder="~/project" />
                <button className="btn" onClick={() => setPicking(true)}>
                  {tx('browse')}
                </button>
              </div>
              <span className="muted small">{shortPath(cwd, st.hello?.home)}</span>
            </label>
            {tool === 'files' ? null : tool === 'shell' ? (
              <label>
                {tx('commandLeaveEmptyForDefault')}
                <input value={command} onChange={(e) => setCommand(e.target.value)} placeholder={tx('eGHtop')} />
              </label>
            ) : (
              <>
                <div className="field">
                  <span>{tx('permissionMode')}</span>
                  <div className="seg wrap">
                    {permModes(tool, mode).map(([v, l, d]) => (
                      <button key={v} className={perm === v ? 'active' : ''} onClick={() => setPerm(v)} title={d}>
                        {l}
                      </button>
                    ))}
                  </div>
                </div>
                <label>
                  {tx('model')}
                  {models && models.length > 0 && !customModel ? (
                    <select
                      value={model}
                      onChange={(e) => {
                        if (e.target.value === '__custom') {
                          setCustomModel(true);
                          setModel('');
                        } else setModel(e.target.value);
                      }}
                    >
                      <option value="">{tx('toolDefault')}</option>
                      {models.map((m) => (
                        <option key={m.id} value={m.id}>
                          {m.name || m.id}
                          {m.description ? ` — ${m.description}` : ''}
                        </option>
                      ))}
                      <option value="__custom">{tx('otherCustom')}</option>
                    </select>
                  ) : (
                    <input
                      value={model}
                      onChange={(e) => setModel(e.target.value)}
                      placeholder={
                        models === null
                          ? tx('loadingModelsOrTypeModel')
                          : tx('leaveEmptyForToolDefault')
                      }
                    />
                  )}
                </label>
                {(() => {
                  const m = curModel;
                  const list = effortList;
                  if (!list.length) return null;
                  return (
                    <div className="field">
                      <span>{tx('reasoningEffort')}</span>
                      <div className="seg wrap">
                        <button className={!effort ? 'active' : ''} onClick={() => setEffort('')}>
                          {tx('default')}
                          {m?.defaultEffort
                            ? isEn()
                              ? ` (${EFFORT_LABEL[m.defaultEffort] ?? m.defaultEffort})`
                              : `（${EFFORT_LABEL[m.defaultEffort] ?? m.defaultEffort}）`
                            : ''}
                        </button>
                        {list.map((e) => (
                          <button key={e} className={effort === e ? 'active' : ''} onClick={() => setEffort(e)}>
                            {EFFORT_LABEL[e] ?? e}
                          </button>
                        ))}
                      </div>
                    </div>
                  );
                })()}
                {effectiveMode === 'structured' && !ns.resumeId && (
                  <label>
                    {ns.handoff ? tx('initialPrompt') : tx('initialPromptOptional')}
                    <textarea rows={ns.handoff ? 10 : 3} value={prompt} onChange={(e) => setPrompt(e.target.value)} placeholder={tx('whatWouldYouLikeTo')} />
                  </label>
                )}
              </>
            )}
            {handoffBlocked && tools.length > 0 && (
              <p className="muted small">{tx('noOtherToolIsInstalled')}</p>
            )}
            <div className="row end ns-actions">
              <button className="btn" onClick={close}>
                {tx('cancel')}
              </button>
              <button className="btn primary" disabled={busy || !cwd || handoffBlocked} onClick={create}>
                {busy ? (aiTool || tool === 'shell' ? tx('starting') : tx('creating')) : ns.resumeId ? tx('resume') : tx('create')}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
