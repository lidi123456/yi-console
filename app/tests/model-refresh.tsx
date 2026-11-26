import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { ModelPicker } from '../src/components/ModelPicker';
import { HostTools } from '../src/components/HostTools';
import { setLangPref, useLang } from '../src/i18n';
import { useStore } from '../src/store';
import type { SessionInfo, ToolInfo } from '../src/types';
import '../src/styles.css';

const query = new URLSearchParams(location.search);
const view = query.get('view') ?? 'picker';
const efforts = { efforts: ['low', 'medium', 'high'], defaultEffort: 'medium' };
const catalog = [
  { id: 'gpt-5.6-sol', name: 'GPT-5.6 Sol', ...efforts },
  { id: 'gpt-5.5', name: 'GPT-5.5', ...efforts },
  { id: 'gpt-5.5-mini', name: 'GPT-5.5 mini', ...efforts },
  { id: 'gpt-5.4-codex', name: 'GPT-5.4 Codex', ...efforts },
];
const added = [
  { id: 'gpt-5.7', name: 'GPT-5.7', ...efforts },
  { id: 'gpt-5.7-mini', name: 'GPT-5.7 mini', ...efforts },
];
let models = catalog;
const info = { id: 'fixture', tool: 'codex', mode: 'structured', model: 'gpt-5.6-sol', effort: 'medium', modelConfigOptions: [] } as unknown as SessionInfo;
const tools: ToolInfo[] = [
  { kind: 'claude', available: true, path: 'C:\\Users\\me\\.local\\bin\\claude.exe', version: '2.3.1 (Claude Code)', structured: true, accountCount: 2, activeAccountLabel: 'work' },
  { kind: 'codex', available: true, path: 'C:\\Users\\me\\AppData\\Roaming\\npm\\codex.cmd', version: 'codex-cli 0.171.0', structured: true, accountCount: 1 },
  { kind: 'cursor', available: true, path: 'C:\\Users\\me\\AppData\\Local\\cursor-agent\\cursor-agent.cmd', version: '2026.11.20', structured: true },
  { kind: 'kiro', available: false, path: null, version: null, structured: true },
];
const delay = Number(query.get('delay') ?? 1200);
class FixtureSocket {
  readyState = 0;
  onopen?: () => void;
  onmessage?: (event: { data: string }) => void;
  constructor() { setTimeout(() => { this.readyState = 1; this.onopen?.(); }); }
  send(raw: string) {
    const request = JSON.parse(raw);
    const { method, params } = request.args;
    let result: unknown = null;
    let wait = 0;
    if (method === 'session.models') {
      if (params.refresh) { models = [...catalog, ...added]; wait = delay; }
      result = { current: info.model, models, ...(params.refresh ? { refreshed: true } : {}) };
    }
    if (method === 'tools.latest') result = { tools: { claude: { latest: '2.3.1', canUpdate: true }, codex: { latest: '0.172.0', canUpdate: true }, cursor: { latest: '2026.11.20', canUpdate: true } }, jobs: {} };
    if (method === 'tools.models') { result = { models: [...catalog, ...added], added: params.tool === 'codex' ? 2 : 0, changed: params.tool === 'codex' }; wait = delay; }
    setTimeout(() => this.onmessage?.({ data: JSON.stringify({ id: request.id, result }) }), wait);
  }
}
Object.assign(window, { WebSocket: FixtureSocket });
setLangPref(query.get('lang') === 'en' ? 'en' : 'zh');
if (query.has('dark')) document.documentElement.dataset.theme = 'dark';
localStorage.setItem('host-tools-open:fixture', 'true');

function Toasts() {
  const toasts = useStore((s) => s.toasts);
  return <div className="toasts">{toasts.map((t) => <div key={t.id} className={`toast toast-${t.kind}`}><span>{t.text}</span></div>)}</div>;
}

function Fixture() {
  useLang();
  const [, render] = useState(0);
  void render;
  if (view === 'host') {
    return (
      <div style={{ padding: 24, maxWidth: 860 }}>
        <div className="card host-card"><div className="card-title">本机</div><HostTools hostId="fixture" tools={tools} /></div>
        <Toasts />
      </div>
    );
  }
  return <div style={{ position: 'fixed', bottom: 32, right: 32 }}><ModelPicker hostId="fixture" info={info} /></div>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
