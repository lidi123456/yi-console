import { useEffect, useState } from 'react';
import { isEn, tr } from '../i18n';
import { Icon } from './Icon';
import { errorToast, invalidateSnippets, toast, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type DaemonSettings, type Snippet, type StatusLineSettings, type ToolKind } from '../types';
import { DEFAULT_STATUS_LINE, getStatusFields, invalidateStatusLine, TEMPLATE_VARS } from './StatusLine';

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

  if (!s) return <div className="page muted">{tr('加载中…', 'Loading…')}</div>;

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
      await rpc(hostId, 'tools.detect');
      invalidateStatusLine(hostId);
      toast(tr('已保存', 'Saved'));
    } catch (e) {
      errorToast(e);
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="page">
      <div className="page-head">
        <h2>{host?.config.name} · {tr('设置', 'Settings')}</h2>
        <button className="btn primary" disabled={saving} onClick={save}>
          {tr('保存', 'Save')}
        </button>
      </div>
      <div className="card">
        <div className="card-title">{tr('会话进程', 'Session processes')}</div>
        <p className="muted small">
          {tr(
            '会话发过消息后，背后会开着一个工具进程，各占 100～250MB 内存。闲置超过设定时间，或同时开着的进程超过上限时（先结束最久没用的），这个进程会被结束；聊天记录都在，下次发消息会自动接着原来的会话继续（要多等几秒启动）。正在回复的会话不会被结束。终端模式的会话不受这两项管理，不用时请在会话菜单里「停止进程」。',
            'A session keeps a tool process (100–250MB of memory) once you send a message. When it has been idle past the set time, or more processes are open than the limit (least recently used first), the process stops; the chat stays, and the next message resumes the same session (startup takes a few seconds). Sessions that are replying are never stopped. Terminal-mode sessions are not managed by these settings; use "Stop Process" in the session menu when done.',
          )}
        </p>
        <div className="form">
          <label>
            {tr('闲置多久后释放', 'Release after idle')}
            <select
              value={s.idleReleaseMinutes ?? 30}
              onChange={(e) => setS({ ...s, idleReleaseMinutes: Number(e.target.value) })}
            >
              {[10, 30, 60, 180, 0].map((m) => (
                <option key={m} value={m}>
                  {m === 0 ? tr('不释放', 'Never') : m < 60 ? tr(`${m} 分钟`, `${m} min`) : tr(`${m / 60} 小时`, `${m / 60} h`)}
                </option>
              ))}
            </select>
          </label>
          <label>
            {tr('同时运行的工具进程上限', 'Max running tool processes')}
            <select
              value={s.maxLiveProcesses ?? 5}
              onChange={(e) => setS({ ...s, maxLiveProcesses: Number(e.target.value) })}
            >
              {[2, 3, 5, 8, 12, 0].map((n) => (
                <option key={n} value={n}>
                  {n === 0 ? tr('不限', 'Unlimited') : tr(`${n} 个`, `${n}`)}
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
            {tr(
              '终端会话放在 tmux 里运行：守护进程重启、升级后程序不中断，打开会话自动接上（需要这台主机装了 tmux 3.2 以上，Windows 不支持）',
              'Run terminal sessions in tmux so they survive daemon restarts and upgrades, and reconnect when opened (needs tmux 3.2+; not supported on Windows)',
            )}
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={s.autoResume ?? true}
              onChange={(e) => setS({ ...s, autoResume: e.target.checked })}
            />
            {tr(
              '额度用完后自动继续：Claude Code、Codex 在对话界面因额度用完停下时，等额度恢复后自动发送“继续”（恢复时间超过一天或连续多次碰到额度时不会自动继续）',
              'Continue after usage limits: when Claude Code or Codex stops on its usage limit in the chat UI, send "continue" automatically once the limit resets (not when it resets more than a day out, or after hitting the limit several times in a row)',
            )}
          </label>
        </div>
      </div>
      <SnippetsCard list={s.snippets} onChange={(snippets) => setS({ ...s, snippets })} />
      <StatusLineCard v={s.statusLine} onChange={(statusLine) => setS({ ...s, statusLine })} />
      <div className="card">
        <div className="card-title">{tr('工具路径', 'Tool paths')}</div>
        <p className="muted small">{tr('默认在 PATH 和常见安装目录里查找。找不到或想用特定版本时在这里指定。', 'Looked up on PATH and common install locations. Set a path here to pin a version.')}</p>
        <div className="form">
          {(['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy', 'shell'] as ToolKind[]).map((k) => {
            const detected = host?.status.hello?.tools.find((t) => t.kind === k);
            return (
              <label key={k}>
                {TOOL_LABEL[k]}
                <input
                  value={s.tools[k]?.command ?? ''}
                  onChange={(e) => setTool(k, { command: e.target.value })}
                  placeholder={detected?.path ?? tr('未找到', 'Not found')}
                />
                <input
                  value={(s.tools[k]?.extraArgs ?? []).join(' ')}
                  onChange={(e) => setTool(k, { extraArgs: e.target.value.split(' ').filter(Boolean) })}
                  placeholder={tr('额外参数（空格分隔，可选）', 'Extra args (space-separated, optional)')}
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
      <div className="card-title">{tr('会话状态栏', 'Session status line')}</div>
      <p className="muted small">
        {tr(
          '显示在会话输入框下面。可以只用内置字段，也可以配一条命令自己输出（和 Claude Code 的 statusLine 一样），两者会拼在一起显示。这里的设置只对这台主机上的会话生效。',
          'Shown under the session input. Use built-in fields, a command (same idea as Claude Code statusLine), or both. Applies only to sessions on this host.',
        )}
      </p>
      <div className="field">
        <span>{tr('内置字段（勾选并调整顺序）', 'Built-in fields (check and reorder)')}</span>
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
                  <button className="icon-btn" title={tr('上移', 'Move up')} onClick={() => move(id, -1)} disabled={v.items[0] === id}>
                    ↑
                  </button>
                  <button className="icon-btn" title={tr('下移', 'Move down')} onClick={() => move(id, 1)} disabled={v.items[v.items.length - 1] === id}>
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
          {tr('自定义格式（可选，填了就代替上面的勾选）', 'Custom format (optional; replaces the checkboxes)')}
          <input
            value={v.template}
            onChange={(e) => onChange({ ...v, template: e.target.value })}
            placeholder="{model} · {branch}{dirty} · {context} · {cost}"
          />
          <span className="muted small">{tr('可用变量：', 'Variables: ')}{TEMPLATE_VARS.map((k) => `{${k}}`).join(' ')}</span>
        </label>
        <label>
          {tr('状态栏命令（可选）', 'Status command (optional)')}
          <input
            className="mono"
            value={v.command}
            onChange={(e) => onChange({ ...v, command: e.target.value })}
            placeholder={tr('例如 ~/.yi-console/statusline.sh 或 npx -y ccusage statusline', 'e.g. ~/.yi-console/statusline.sh or npx -y ccusage statusline')}
          />
          <span className="muted small">
            {tr(
              '在会话目录里执行，stdin 收到 JSON（session_id、cwd、model、workspace、cost、context_window，以及 yi_console 下的 tool、usage、git 等），取输出的第一行，支持 ANSI 颜色，超过 3 秒算失败。Windows 上用 cmd 执行。',
              'Runs in the session directory. stdin is JSON (session_id, cwd, model, workspace, cost, context_window, plus tool, usage, git under yi_console). The first output line is shown, ANSI colors included. Over 3 seconds counts as failure. On Windows it runs via cmd.',
            )}
            <button className="link-btn small" onClick={() => setSample(!sample)}>
              {sample ? tr('收起示例', 'Hide sample') : tr('看示例脚本', 'Show sample script')}
            </button>
          </span>
          {sample && <pre className="sl-sample mono small">{SAMPLE_CMD}</pre>}
        </label>
        <label className="check">
          <input type="checkbox" checked={v.useClaude} onChange={(e) => onChange({ ...v, useClaude: e.target.checked })} />
          {tr(
            '没填命令时，Claude Code 会话沿用 ~/.claude/settings.json 里的 statusLine（Windows 上用 Git Bash 执行）',
            'When no command is set, Claude Code sessions use statusLine from ~/.claude/settings.json (Git Bash on Windows)',
          )}
        </label>
      </div>
    </div>
  );
}

function SnippetsCard({ list, onChange }: { list: Snippet[]; onChange: (v: Snippet[]) => void }) {
  const set = (i: number, p: Partial<Snippet>) => onChange(list.map((x, j) => (j === i ? { ...x, ...p } : x)));
  return (
    <div className="card">
      <div className="card-title">{tr('提示词片段', 'Prompt snippets')}</div>
      <p className="muted small">
        {tr(
          '常用的指令存在这里，在这台主机的任意会话输入框开头打 / 就能选到，选中后插入输入框，可以再改。保存在这台机器的 yi-agentd 上，手机和电脑连上来都能用。',
          'Saved commands for this host. Type / at the start of any session input to insert one, then edit it. Stored on this machine’s yi-agentd, so phones and computers share them.',
        )}
      </p>
      {list.map((x, i) => (
        <div key={i} className="snippet-row">
          <div className="row">
            <input value={x.name} placeholder={tr('名称，比如 review', 'Name, e.g. review')} onChange={(e) => set(i, { name: e.target.value.replace(/\s+/g, '-') })} />
            <button className="icon-btn" title={tr('删除', 'Delete')} onClick={() => onChange(list.filter((_, j) => j !== i))}>
              <Icon name="trash" size={15} />
            </button>
          </div>
          <textarea rows={3} value={x.text} placeholder={tr('片段内容', 'Snippet text')} onChange={(e) => set(i, { text: e.target.value })} />
        </div>
      ))}
      <div className="row">
        <button className="btn small" onClick={() => onChange([...list, { name: '', text: '' }])}>
          <Icon name="plus" size={14} /> {tr('添加片段', 'Add snippet')}
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
            {tr('用几个示例开始', 'Start with examples')}
          </button>
        )}
      </div>
    </div>
  );
}
