import { useEffect, useState } from 'react';
import { useBack } from '../back';
import { isEn, tr } from '../i18n';
import { errorToast, hostStatus, navigate, setState, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type DirEntry, type ModelOption, type SessionInfo, type SessionMode, type ToolKind } from '../types';
import { lsGet, lsSet, shortPath } from '../util';
import { useDrives } from './FilesView';
import { Icon, ToolBadge } from './Icon';
import { EFFORT_LABEL } from './ModelPicker';

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
        // 起始目录在这台主机上不存在时，退回用户主目录，不至于什么都选不了
        if (path !== '~' && !list) setPath('~');
        else setErr(String(e.message ?? e));
      });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, path]);
  return (
    <div className="dirpicker">
      <div className="dirpicker-head">
        <button className="icon-btn" disabled={!list?.parent} onClick={() => list?.parent && setPath(list.parent)} title={tr('上一级', 'Up')}>
          <Icon name="up" size={16} />
        </button>
        <span className="mono truncate grow">{list?.path ?? path}</span>
        {drives.length > 1 && (
          <select value="" onChange={(e) => e.target.value && setPath(e.target.value)} title={tr('切换磁盘', 'Switch Drive')}>
            <option value="">{tr('盘符', 'Drive')}</option>
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
          {tr('取消', 'Cancel')}
        </button>
        <button className="btn primary" disabled={!list} onClick={() => list && onPick(list.path)}>
          {tr('选择此目录', 'Select Directory')}
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

/** 各工具实际支持的审批模式，和 yi-agentd 的 `permission_modes` 保持一致 */
export function permModes(tool: ToolKind, mode: SessionMode) {
  const ids: Record<ToolKind, string[]> = {
    claude: ['default', 'acceptEdits', 'auto', 'plan', 'bypass'],
    codex: mode === 'structured' ? ['default', 'acceptEdits', 'auto', 'plan', 'bypass'] : ['default', 'auto', 'plan', 'bypass'],
    cursor: mode === 'structured' ? ['default', 'acceptEdits', 'plan', 'bypass'] : ['default', 'plan', 'bypass'],
    kiro: mode === 'structured' ? ['default', 'acceptEdits', 'plan', 'bypass'] : ['default', 'bypass'],
    qoder: ['default', 'acceptEdits', 'bypass'],
    codebuddy: ['default', 'acceptEdits', 'plan', 'bypass'],
    shell: [],
  };
  return getPermModesList().filter((m) => ids[tool].includes(m[0]));
}

/** 每个工具上次新建会话时的选择：界面、审批模式、模型、思考等级（存 localStorage `new-session-prefs`） */
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
  const [models, setModels] = useState<ModelOption[] | null>(null);
  const [customModel, setCustomModel] = useState(false);

  // 工具的可选模型：yi-agentd 有缓存就直接给，没有会临时起一次工具去问（几秒）
  useEffect(() => {
    setModels(null);
    setCustomModel(false);
    if (!ns || !hostId || tool === 'shell') return;
    let alive = true;
    rpc<ModelOption[]>(hostId, 'tools.models', { tool })
      .then((m) => {
        if (!alive) return;
        setModels(m);
        // 记住的模型不在这台主机的列表里（手动输入过、或换了主机）：改用输入框显示，不要被下拉框吞掉
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
    // 换个工具继续：默认用上次用的工具，正好是原工具就先选 claude / codex 里另一个，没装的话下面再换
    const last = lsGet<ToolKind>('last-tool', 'claude');
    const from = ns.handoff?.from;
    setTool((ns.tool as ToolKind) ?? (from && (last === from || last === 'shell') ? (from === 'claude' ? 'codex' : 'claude') : last));
    setPrompt(ns.handoff?.prompt ?? '');
    setCommand('');
    setPicking(false);
  }, [ns]);

  // 换工具（包括刚打开）时带出这个工具上次的选择
  useEffect(() => {
    if (!ns) return;
    const p = lsGet<Record<string, ToolPrefs>>(PREFS_KEY, {})[tool] ?? {};
    // 换个工具继续只走对话模式：第一条消息要靠对话模式发出去
    const m = ns.handoff ? 'structured' : (p.mode ?? lsGet<'structured' | 'terminal'>('last-mode', 'structured'));
    setMode(m);
    setPerm(p.perm && permModes(tool, m).some((x) => x[0] === p.perm) ? p.perm : 'default');
    setModel(p.model ?? '');
    setEffort(p.effort ?? '');
  }, [ns, tool]);

  // 默认选中的工具（上次用的、或 claude）在这台主机上没装：改选第一个装了的，免得选中一个灰掉的按钮
  const helloTools = hostStatus(hostId).hello?.tools;
  useEffect(() => {
    if (!ns || ns.resumeId || !helloTools?.length) return;
    const usable = (k: ToolKind) => !ns.handoff || (k !== ns.handoff.from && k !== 'shell');
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
  const effectiveMode = tool === 'shell' ? 'terminal' : mode;
  // 换个工具继续：选中的必须是原工具以外、这台主机装了的工具（只装了原工具时停在一个灰掉的按钮上）
  const handoffBlocked = !!ns.handoff && (tool === ns.handoff.from || tool === 'shell' || !tools.some((t) => t.kind === tool && t.available));

  // 当前模型可选的思考等级；记住的等级这个模型不支持时就不发
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
        prompt: effectiveMode === 'structured' ? prompt : undefined,
        permissionMode: permModes(tool, mode).some((m) => m[0] === perm) ? perm : 'default',
        model: model || undefined,
        effort: sendEffort || undefined,
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
      if (tool !== 'shell') {
        // 换个工具继续固定是对话模式，不当作用户选的界面记下来
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
          <h3>{ns.resumeId ? tr('继续会话', 'Resume Session') : ns.handoff ? tr('换个工具继续', 'Continue in Another Tool') : tr('新建会话', 'New Session')}</h3>
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
            {/* 换个工具继续：原会话的记录文件在原主机上，不能换主机 */}
            {connectedHosts.length > 1 && !ns.resumeId && !ns.handoff && (
              <label>
                {tr('主机', 'Host')}
                <select
                  value={hostId}
                  onChange={(e) => {
                    // 换主机后原来的目录在那边多半不存在，换成那台主机上次用的目录
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
              <span>{tr('工具', 'Tool')}</span>
              <div className="tool-grid">
                {tools.map((t) => (
                  <button
                    key={t.kind}
                    disabled={
                      !t.available || (!!ns.resumeId && t.kind !== ns.tool) || (!!ns.handoff && (t.kind === ns.handoff.from || t.kind === 'shell'))
                    }
                    className={`tool-opt ${tool === t.kind ? 'active' : ''}`}
                    onClick={() => setTool(t.kind)}
                    title={t.available ? (t.version ?? t.path ?? '') : tr('这台机器上没有安装', 'Not installed on this host')}
                  >
                    <ToolBadge tool={t.kind} size={18} />
                    <span className="tool-opt-label">
                      {TOOL_LABEL[t.kind]}
                      {!t.available && <span className="tool-opt-tag">{tr('未安装', 'Not installed')}</span>}
                    </span>
                  </button>
                ))}
              </div>
            </div>
            {tool !== 'shell' && !ns.handoff && (
              <div className="field">
                <span>{tr('界面', 'Interface')}</span>
                <div className="seg">
                  <button className={mode === 'structured' ? 'active' : ''} onClick={() => setMode('structured')}>
                    <Icon name="chat" size={14} /> {tr('对话', 'Chat')}
                  </button>
                  <button className={mode === 'terminal' ? 'active' : ''} onClick={() => setMode('terminal')}>
                    <Icon name="terminal" size={14} /> {tr('终端', 'Terminal')}
                  </button>
                </div>
              </div>
            )}
            <label>
              {tr('工作目录', 'Working Directory')}
              <div className="row">
                <input value={cwd} onChange={(e) => setCwd(e.target.value)} placeholder="~/project" />
                <button className="btn" onClick={() => setPicking(true)}>
                  {tr('浏览', 'Browse')}
                </button>
              </div>
              <span className="muted small">{shortPath(cwd, st.hello?.home)}</span>
            </label>
            {tool === 'shell' ? (
              <label>
                {tr('命令（留空则打开默认 shell）', 'Command (leave empty for default shell)')}
                <input value={command} onChange={(e) => setCommand(e.target.value)} placeholder={tr('例如 htop', 'e.g. htop')} />
              </label>
            ) : (
              <>
                <div className="field">
                  <span>{tr('审批模式', 'Permission Mode')}</span>
                  <div className="seg wrap">
                    {permModes(tool, mode).map(([v, l, d]) => (
                      <button key={v} className={perm === v ? 'active' : ''} onClick={() => setPerm(v)} title={d}>
                        {l}
                      </button>
                    ))}
                  </div>
                </div>
                <label>
                  {tr('模型', 'Model')}
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
                      <option value="">{tr('工具默认', 'Tool Default')}</option>
                      {models.map((m) => (
                        <option key={m.id} value={m.id}>
                          {m.name || m.id}
                          {m.description ? ` — ${m.description}` : ''}
                        </option>
                      ))}
                      <option value="__custom">{tr('其他（手动输入）…', 'Other (custom)…')}</option>
                    </select>
                  ) : (
                    <input
                      value={model}
                      onChange={(e) => setModel(e.target.value)}
                      placeholder={
                        models === null
                          ? tr('正在读取可选模型…（也可以直接输入）', 'Loading models… (or type model ID)')
                          : tr('留空使用工具默认模型', 'Leave empty for tool default model')
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
                      <span>{tr('思考等级', 'Reasoning Effort')}</span>
                      <div className="seg wrap">
                        <button className={!effort ? 'active' : ''} onClick={() => setEffort('')}>
                          {tr('默认', 'Default')}
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
                    {ns.handoff ? tr('第一条消息', 'Initial Prompt') : tr('第一条消息（可选）', 'Initial Prompt (optional)')}
                    <textarea rows={ns.handoff ? 10 : 3} value={prompt} onChange={(e) => setPrompt(e.target.value)} placeholder={tr('想让它做什么？', 'What would you like to do?')} />
                  </label>
                )}
              </>
            )}
            {handoffBlocked && tools.length > 0 && (
              <p className="muted small">{tr('这台主机上没有装其他可用的工具。', 'No other tool is installed on this host.')}</p>
            )}
            <div className="row end ns-actions">
              <button className="btn" onClick={close}>
                {tr('取消', 'Cancel')}
              </button>
              <button className="btn primary" disabled={busy || !cwd || handoffBlocked} onClick={create}>
                {busy ? tr('启动中…', 'Starting…') : ns.resumeId ? tr('继续', 'Resume') : tr('创建', 'Create')}
              </button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
