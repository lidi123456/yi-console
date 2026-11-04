import { useEffect, useState } from 'react';
import { getT, isEn } from '../i18n';
import { Icon } from './Icon';
import { errorToast, invalidateSnippets, redetectTools, toast, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type DaemonSettings, type Snippet, type StatusLineSettings, type ToolKind } from '../types';
import { DEFAULT_STATUS_LINE, getStatusFields, invalidateStatusLine, TEMPLATE_VARS } from './StatusLine';

const t = getT('settingsView');

export function SettingsView({ hostId }: { hostId: string }) {
  const host = useStore((s) => s.hosts.find((h) => h.config.id === hostId));
  const [s, setS] = useState<DaemonSettings | null>(null);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    rpc<DaemonSettings>(hostId, 'settings.get')
      .then((v) =>
        // Include every field first (idle release, tmux, etc.), otherwise saving resets them to defaults
        setS({
          ...v,
          tools: v.tools ?? {},
          statusLine: { ...DEFAULT_STATUS_LINE, ...(v.statusLine ?? {}) },
          snippets: v.snippets ?? [],
        }),
      )
      .catch(errorToast);
  }, [hostId]);

  if (!s) return <div className="page muted">{t('loading')}</div>;

  const setTool = (k: string, p: Partial<{ command: string; extraArgs: string[] }>) =>
    setS({ ...s, tools: { ...s.tools, [k]: { command: '', extraArgs: [], ...(s.tools[k] as object | undefined), ...p } } });

  const save = async () => {
    setSaving(true);
    try {
      // Drop empty tool overrides
      const tools = Object.fromEntries(
        Object.entries(s.tools).filter(([, v]) => v.command.trim() || v.extraArgs.length),
      );
      const snippets = s.snippets.filter((x) => x.name.trim() && x.text.trim()).map((x) => ({ name: x.name.trim(), text: x.text }));
      await rpc(hostId, 'settings.set', { ...s, tools, snippets });
      invalidateSnippets(hostId);
      await redetectTools(hostId);
      invalidateStatusLine(hostId);
      toast(t('saved'));
    } catch (e) {
      errorToast(e);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="page">
      <div className="page-head">
        <h2>{host?.config.name} · {t('settings')}</h2>
        <button className="btn primary" disabled={saving} onClick={save}>
          {t('save')}
        </button>
      </div>
      <div className="card">
        <div className="card-title">{t('sessionProcesses')}</div>
        <p className="muted small">
          {t('aSessionKeepsATool')}
        </p>
        <div className="form">
          <label>
            {t('releaseAfterIdle')}
            <select
              value={s.idleReleaseMinutes ?? 30}
              onChange={(e) => setS({ ...s, idleReleaseMinutes: Number(e.target.value) })}
            >
              {[10, 30, 60, 180, 0].map((m) => (
                <option key={m} value={m}>
                  {m === 0 ? t('never') : m < 60 ? t('min', { m }) : t('h', { v: m / 60 })}
                </option>
              ))}
            </select>
          </label>
          <label>
            {t('maxRunningToolProcesses')}
            <select
              value={s.maxLiveProcesses ?? 5}
              onChange={(e) => setS({ ...s, maxLiveProcesses: Number(e.target.value) })}
            >
              {[2, 3, 5, 8, 12, 0].map((n) => (
                <option key={n} value={n}>
                  {n === 0 ? t('unlimited') : t('text', { n })}
                </option>
              ))}
            </select>
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={s.terminalTmux ?? true}
              onChange={(e) => setS({ ...s, terminalTmux: e.target.checked })}
            />
            {t('runTerminalSessionsInTmux')}
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={s.autoResume ?? true}
              onChange={(e) => setS({ ...s, autoResume: e.target.checked })}
            />
            {t('continueAfterUsageLimitsWhen')}
          </label>
        </div>
      </div>
      <SnippetsCard list={s.snippets} onChange={(snippets) => setS({ ...s, snippets })} />
      <StatusLineCard v={s.statusLine} onChange={(statusLine) => setS({ ...s, statusLine })} />
      <div className="card">
        <div className="card-title">{t('toolPaths')}</div>
        <p className="muted small">{t('lookedUpOnPathAnd')}</p>
        <div className="form">
          {(['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy', 'shell'] as ToolKind[]).map((k) => {
            const detected = host?.status.hello?.tools.find((t) => t.kind === k);
            return (
              <label key={k}>
                {TOOL_LABEL[k]}
                <input
                  value={s.tools[k]?.command ?? ''}
                  onChange={(e) => setTool(k, { command: e.target.value })}
                  placeholder={detected?.path ?? t('notFound')}
                />
                <input
                  value={(s.tools[k]?.extraArgs ?? []).join(' ')}
                  onChange={(e) => setTool(k, { extraArgs: e.target.value.split(' ').filter(Boolean) })}
                  placeholder={t('extraArgsSpaceSeparatedOptional')}
                />
              </label>
            );
          })}
        </div>
      </div>
    </div>
  );
}

const SAMPLE_CMD = `#!/bin/sh
# stdin 是一段 JSON（和 Claude Code statusLine 的格式一样），输出的第一行显示在状态栏
input=$(cat)
model=$(echo "$input" | jq -r '.model.display_name')
dir=$(echo "$input" | jq -r '.workspace.current_dir')
printf '\\033[36m%s\\033[0m in %s' "$model" "\${dir##*/}"`;

function StatusLineCard({ v, onChange }: { v: StatusLineSettings; onChange: (v: StatusLineSettings) => void }) {
  const [sample, setSample] = useState(false);
  const on = (id: string) => v.items.includes(id);
  const toggle = (id: string) =>
    onChange({ ...v, items: on(id) ? v.items.filter((x) => x !== id) : [...v.items, id] });
  const move = (id: string, d: -1 | 1) => {
    const a = [...v.items];
    const i = a.indexOf(id);
    const j = i + d;
    if (i < 0 || j < 0 || j >= a.length) return;
    [a[i], a[j]] = [a[j], a[i]];
    onChange({ ...v, items: a });
  };
  // Selected fields first in order, unselected ones after
  const statusFields = getStatusFields();
  const fields = [
    ...v.items.map((id) => statusFields.find((f) => f[0] === id)).filter(Boolean),
    ...statusFields.filter((f) => !v.items.includes(f[0])),
  ] as [string, string, string][];
  return (
    <div className="card">
      <div className="card-title">{t('sessionStatusLine')}</div>
      <p className="muted small">
        {t('shownUnderTheSessionInput')}
      </p>
      <div className="field">
        <span>{t('builtInFieldsCheckAnd')}</span>
        <div className="sl-fields">
          {fields.map(([id, name, desc]) => (
            <div key={id} className={`sl-field ${on(id) ? 'on' : ''}`}>
              <label className="check">
                <input type="checkbox" checked={on(id)} onChange={() => toggle(id)} />
                <span>{name}</span>
                <span className="muted small">{desc}</span>
              </label>
              {on(id) && (
                <span className="sl-move">
                  <button className="icon-btn" title={t('moveUp')} onClick={() => move(id, -1)} disabled={v.items[0] === id}>
                    ↑
                  </button>
                  <button className="icon-btn" title={t('moveDown')} onClick={() => move(id, 1)} disabled={v.items[v.items.length - 1] === id}>
                    ↓
                  </button>
                </span>
              )}
            </div>
          ))}
        </div>
      </div>
      <div className="form">
        <label>
          {t('customFormatOptionalReplacesThe')}
          <input
            value={v.template}
            onChange={(e) => onChange({ ...v, template: e.target.value })}
            placeholder="{model} · {branch}{dirty} · {context} · {cost}"
          />
          <span className="muted small">{t('variables')}{TEMPLATE_VARS.map((k) => `{${k}}`).join(' ')}</span>
        </label>
        <label>
          {t('statusCommandOptional')}
          <input
            className="mono"
            value={v.command}
            onChange={(e) => onChange({ ...v, command: e.target.value })}
            placeholder={t('eGYiConsoleStatusline')}
          />
          <span className="muted small">
            {t('runsInTheSessionDirectory')}
            <button className="link-btn small" onClick={() => setSample(!sample)}>
              {sample ? t('hideSample') : t('showSampleScript')}
            </button>
          </span>
          {sample && <pre className="sl-sample mono small">{SAMPLE_CMD}</pre>}
        </label>
        <label className="check">
          <input type="checkbox" checked={v.useClaude} onChange={(e) => onChange({ ...v, useClaude: e.target.checked })} />
          {t('whenNoCommandIsSet')}
        </label>
      </div>
    </div>
  );
}

function SnippetsCard({ list, onChange }: { list: Snippet[]; onChange: (v: Snippet[]) => void }) {
  const set = (i: number, p: Partial<Snippet>) => onChange(list.map((x, j) => (j === i ? { ...x, ...p } : x)));
  return (
    <div className="card">
      <div className="card-title">{t('promptSnippets')}</div>
      <p className="muted small">
        {t('savedCommandsForThisHost')}
      </p>
      {list.map((x, i) => (
        <div key={i} className="snippet-row">
          <div className="row">
            <input value={x.name} placeholder={t('nameEGReview')} onChange={(e) => set(i, { name: e.target.value.replace(/\s+/g, '-') })} />
            <button className="icon-btn" title={t('delete')} onClick={() => onChange(list.filter((_, j) => j !== i))}>
              <Icon name="trash" size={15} />
            </button>
          </div>
          <textarea rows={3} value={x.text} placeholder={t('snippetText')} onChange={(e) => set(i, { text: e.target.value })} />
        </div>
      ))}
      <div className="row">
        <button className="btn small" onClick={() => onChange([...list, { name: '', text: '' }])}>
          <Icon name="plus" size={14} /> {t('addSnippet')}
        </button>
        {list.length === 0 && (
          <button
            className="btn small"
            onClick={() =>
              onChange([
                { name: 'review', text: isEn() ? 'Review this change: list bugs, edge cases, and simplifications. Do not edit yet.' : '帮我 review 这次的改动：指出 bug、边界情况和可以简化的地方，先列问题，不要直接改。' },
                { name: 'test', text: isEn() ? 'Add unit tests for the code just changed, covering the happy path and edge cases, then run them.' : '给刚才改动的代码补上单元测试，覆盖正常路径和边界情况，然后运行测试。' },
                { name: 'explain', text: isEn() ? 'Explain how this code works, including the main data flow and call relationships.' : '解释一下这段代码是怎么工作的，关键的数据流和调用关系是什么。' },
              ])
            }
          >
            {t('startWithExamples')}
          </button>
        )}
      </div>
    </div>
  );
}
