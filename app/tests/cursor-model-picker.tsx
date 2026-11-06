import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import { ModelPicker } from '../src/components/ModelPicker';
import { setLangPref, useLang } from '../src/i18n';
import { handleBack } from '../src/back';
import type { SessionInfo } from '../src/types';
import '../src/styles.css';

const query = new URLSearchParams(location.search);
const reasoningId = query.get('reasoningId') ?? 'reasoning';
const category = query.get('category');
const tool = query.get('tool') ?? 'cursor';
const capabilities = query.get('capabilities');
const params = [
  { id: 'context', name: 'Context', description: '', currentValue: '256k', options: [{ value: '256k', name: '256K' }, { value: '500k', name: '500K' }] },
  { id: 'fast', name: 'Fast', description: '2.5x more expensive', currentValue: 'false', options: [{ value: 'false', name: 'Off' }, { value: 'true', name: 'Fast' }] },
  { id: reasoningId, ...(category ? { category } : {}), name: 'Reasoning', description: '', currentValue: 'medium', options: [{ value: 'medium', name: 'Medium' }, { value: 'high', name: 'High' }] },
];
if (capabilities === 'single') for (const option of params) option.options = option.options.slice(0, 1);
const catalog = [
  { id: 'model-a', name: 'Grok 4.7', ...(tool === 'codex' || tool === 'claude' ? { efforts: ['low', 'medium', 'high'], defaultEffort: 'medium' } : {}) },
  { id: 'model-b', name: 'Auto' },
  ...['Grok 4.6', 'Composer 2.5', 'Gemini 4 Argon', 'GPT-5.6 Sol', 'GPT-5.5', 'Claude Opus 4.8', 'A model with a very long display name that wraps inside the menu'].map((name, i) => ({ id: `extra-${i}`, name })),
];
let info = { id: 'fixture', tool, mode: 'structured', model: 'model-a', effort: tool === 'cursor' ? null : 'medium', modelConfigOptions: capabilities === 'none' ? [] : params } as SessionInfo;
let refresh = () => {};
const calls: unknown[] = [];
const paramsForModel = () => params;
const cached = new Map([['model-a', params]]);
class FixtureSocket {
  readyState = 0;
  onopen?: () => void;
  onmessage?: (event: { data: string }) => void;
  constructor() { setTimeout(() => { this.readyState = 1; this.onopen?.(); }); }
  send(raw: string) {
    const request = JSON.parse(raw);
    const { method, params } = request.args;
    calls.push({ method, params });
    let result: unknown = null;
    if (method === 'session.models') result = { current: info.model, models: catalog };
    if (method === 'session.setModelConfig' && params.configId === 'fast' && query.has('failFast')) {
      queueMicrotask(() => this.onmessage?.({ data: JSON.stringify({ id: request.id, error: 'Rejected Fast change for testing' }) }));
      return;
    }
    if (method === 'session.setEffort') { info = { ...info, effort: params.effort }; refresh(); }
    if (method === 'session.setModelConfig') {
      info = { ...info, modelConfigOptions: info.modelConfigOptions?.map((o) => o.id === params.configId ? { ...o, currentValue: params.value } : o) };
      refresh();
    }
    if (method === 'session.setModel') {
      if (query.has('delayed')) {
        const target = params.model;
        // The daemon answers from its cache at once; only an unknown model waits for its parameters
        const known = cached.get(target);
        info = { ...info, model: target, modelConfigPending: !known, modelConfigOptions: known ?? [] }; refresh();
        if (!known) setTimeout(() => {
          const options = target === 'model-b' ? [] : paramsForModel();
          cached.set(target, options);
          if (info.model === target) { info = { ...info, modelConfigPending: false, modelConfigOptions: options }; refresh(); }
        }, 1500);
      } else { info = { ...info, model: params.model, modelConfigOptions: [] }; refresh(); }
    }
    queueMicrotask(() => this.onmessage?.({ data: JSON.stringify({ id: request.id, result }) }));
  }
}
Object.assign(window, { WebSocket: FixtureSocket, fixtureCalls: calls, fixtureLang: setLangPref, fixtureBack: handleBack });
setLangPref('en');
if (query.has('dark')) document.documentElement.dataset.theme = 'dark';
function Fixture() {
  useLang();
  const [, render] = useState(0);
  refresh = () => render((n) => n + 1);
  return <div style={{ position: 'fixed', bottom: 32, right: query.has('left') ? undefined : 32, left: query.has('left') ? 380 : undefined }}><ModelPicker hostId="fixture" info={info} /></div>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
