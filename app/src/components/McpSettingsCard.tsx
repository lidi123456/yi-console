import { useState } from 'react';
import { useBack } from '../back';
import { getT } from '../i18n';
import { TOOL_LABEL, type McpServerConfig, type ToolKind } from '../types';
import { Icon } from './Icon';
import { askConfirm } from './Overlay';

const t = getT('mcpSettings');
const tools: ToolKind[] = ['claude', 'codex', 'cursor', 'kiro', 'qoder', 'codebuddy'];
const blank = (): McpServerConfig => ({
  name: '',
  enabled: true,
  transport: 'stdio',
  command: '',
  args: [],
  env: {},
  url: '',
  headers: {},
  tools: [],
});

export function McpSettingsCard({ servers, onChange }: { servers: McpServerConfig[]; onChange: (servers: McpServerConfig[]) => void }) {
  const [editing, setEditing] = useState<number | null>(null);
  return (
    <div className="card">
      <div className="row">
        <div className="card-title" style={{ flex: 1 }}>
          {t('title')}
        </div>
        <button className="btn small" onClick={() => setEditing(-1)}>
          {t('add')}
        </button>
      </div>
      <p className="muted small">{t('description')}</p>
      {!servers.length && <p className="muted small">{t('empty')}</p>}
      {servers.map((server, index) => (
        <div className="row mcp-server-row" key={server.name || index}>
          <label className="check">
            <input
              type="checkbox"
              checked={server.enabled}
              onChange={(e) => onChange(servers.map((s, i) => (i === index ? { ...s, enabled: e.target.checked } : s)))}
            />
            {server.name}
          </label>
          <span className="muted small" style={{ flex: 1 }}>
            {server.transport === 'http' ? t('http') : t('stdio')}
          </span>
          <button className="btn small" onClick={() => setEditing(index)}>
            {t('edit')}
          </button>
          <button
            className="btn small"
            onClick={async () => {
              if (await askConfirm({ title: t('remove'), message: t('removeQuestion', { name: server.name }) }))
                onChange(servers.filter((_, i) => i !== index));
            }}
          >
            {t('remove')}
          </button>
        </div>
      ))}
      {editing !== null && (
        <Editor
          initial={editing < 0 ? blank() : servers[editing]}
          close={() => setEditing(null)}
          save={(server) => {
            if (servers.some((s, i) => i !== editing && s.name === server.name)) throw new Error(t('duplicate'));
            onChange(editing < 0 ? [...servers, server] : servers.map((s, i) => (i === editing ? server : s)));
            setEditing(null);
          }}
        />
      )}
    </div>
  );
}

function Editor({ initial, close, save }: { initial: McpServerConfig; close: () => void; save: (server: McpServerConfig) => void }) {
  const [draft, setDraft] = useState(initial);
  const [args, setArgs] = useState(JSON.stringify(initial.args, null, 2));
  const [env, setEnv] = useState(JSON.stringify(initial.env, null, 2));
  const [headers, setHeaders] = useState(JSON.stringify(initial.headers, null, 2));
  const [all, setAll] = useState(!initial.tools.length);
  const [error, setError] = useState('');
  useBack(true, close);
  const stringMap = (text: string): Record<string, string> => {
    const value = JSON.parse(text);
    if (!value || Array.isArray(value) || typeof value !== 'object' || Object.values(value).some((v) => typeof v !== 'string'))
      throw new Error(t('stringMap'));
    return value;
  };
  const apply = () => {
    try {
      if (!/^[a-zA-Z0-9_-]{1,64}$/.test(draft.name) || draft.name === 'yi_computer_use') throw new Error(t('invalidName'));
      if (draft.transport === 'stdio' && !draft.command.trim()) throw new Error(t('commandRequired'));
      if (draft.transport === 'http' && !/^https?:\/\//.test(draft.url)) throw new Error(t('urlRequired'));
      if (!all && !draft.tools.length) throw new Error(t('chooseTool'));
      const parsedArgs: unknown = JSON.parse(args);
      if (!Array.isArray(parsedArgs) || parsedArgs.some((value) => typeof value !== 'string')) throw new Error(t('stringArray'));
      save({
        ...draft,
        tools: all ? [] : draft.tools,
        args: parsedArgs as string[],
        env: stringMap(env),
        headers: stringMap(headers),
      });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  return (
    <div className="modal-bg" onMouseDown={close}>
      <section
        className="modal mcp-editor"
        role="dialog"
        aria-modal="true"
        aria-label={t('editor')}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === 'Escape') close();
          if (e.key === 'Tab') {
            const fields = Array.from(
              e.currentTarget.querySelectorAll<HTMLElement>(
                'button:not(:disabled), input:not(:disabled), textarea:not(:disabled), select:not(:disabled)',
              ),
            );
            if (e.shiftKey && document.activeElement === fields[0]) {
              e.preventDefault();
              fields[fields.length - 1]?.focus();
            } else if (!e.shiftKey && document.activeElement === fields[fields.length - 1]) {
              e.preventDefault();
              fields[0]?.focus();
            }
          }
        }}
      >
        <div className="row">
          <strong style={{ flex: 1 }}>{t('editor')}</strong>
          <button className="icon-btn" aria-label={t('close')} onClick={close}>
            <Icon name="x" />
          </button>
        </div>
        <div className="form">
          <label>
            {t('name')}
            <input autoFocus value={draft.name} onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
          </label>
          <label>
            {t('transport')}
            <select value={draft.transport} onChange={(e) => setDraft({ ...draft, transport: e.target.value as 'stdio' | 'http' })}>
              <option value="stdio">{t('stdio')}</option>
              <option value="http">{t('http')}</option>
            </select>
          </label>
          {draft.transport === 'stdio' ? (
            <>
              <label>
                {t('command')}
                <input value={draft.command} onChange={(e) => setDraft({ ...draft, command: e.target.value })} />
              </label>
              <label>
                {t('args')}
                <textarea rows={3} value={args} onChange={(e) => setArgs(e.target.value)} />
              </label>
              <label>
                {t('env')}
                <textarea rows={3} value={env} onChange={(e) => setEnv(e.target.value)} />
              </label>
            </>
          ) : (
            <>
              <label>
                {t('url')}
                <input value={draft.url} onChange={(e) => setDraft({ ...draft, url: e.target.value })} />
              </label>
              <label>
                {t('headers')}
                <textarea rows={3} value={headers} onChange={(e) => setHeaders(e.target.value)} />
              </label>
            </>
          )}
          <p className="small muted">{t('secrets')}</p>
          <label className="check">
            <input type="checkbox" checked={all} onChange={(e) => setAll(e.target.checked)} />
            {t('allTools')}
          </label>
          {!all && (
            <div className="mcp-tool-scopes">
              {tools.map((tool) => (
                <label className="check" key={tool}>
                  <input
                    type="checkbox"
                    checked={draft.tools.includes(tool)}
                    onChange={(e) =>
                      setDraft({ ...draft, tools: e.target.checked ? [...draft.tools, tool] : draft.tools.filter((v) => v !== tool) })
                    }
                  />
                  {TOOL_LABEL[tool]}
                </label>
              ))}
            </div>
          )}
        </div>
        {error && (
          <p className="error-text" role="alert">
            {error}
          </p>
        )}
        <div className="dlg-actions">
          <button className="btn" onClick={close}>
            {t('cancel')}
          </button>
          <button className="btn primary" onClick={apply}>
            {t('apply')}
          </button>
        </div>
      </section>
    </div>
  );
}
