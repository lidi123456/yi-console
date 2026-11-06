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
const params = [
  { id: 'fast', name: 'Fast', description: '2.5x more expensive', currentValue: 'false', options: [{ value: 'false', name: 'Off' }, { value: 'true', name: 'Fast' }] },
  { id: reasoningId, ...(category ? { category } : {}), name: 'Reasoning', description: '', currentValue: 'medium', options: [{ value: 'medium', name: 'Medium' }, { value: 'high', name: 'High' }] },
];
let info = { id: 'fixture', tool: 'cursor', mode: 'structured', model: 'model-a', modelConfigOptions: params } as SessionInfo;
let refresh = () => {};
const calls: unknown[] = [];
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
    if (method === 'session.models') result = { current: info.model, models: [{ id: 'model-a', name: 'Model A' }, { id: 'model-b', name: 'Model B' }] };
    if (method === 'session.setModelConfig') {
      info = { ...info, modelConfigOptions: info.modelConfigOptions?.map((o) => o.id === params.configId ? { ...o, currentValue: params.value } : o) };
      refresh();
    }
    if (method === 'session.setModel') { info = { ...info, model: params.model, modelConfigOptions: [] }; refresh(); }
    queueMicrotask(() => this.onmessage?.({ data: JSON.stringify({ id: request.id, result }) }));
  }
}
Object.assign(window, { WebSocket: FixtureSocket, fixtureCalls: calls, fixtureLang: setLangPref, fixtureBack: handleBack });
setLangPref('en');
function Fixture() {
  useLang();
  const [, render] = useState(0);
  refresh = () => render((n) => n + 1);
  return <div style={{ position: 'fixed', bottom: 32, right: 32 }}><ModelPicker hostId="fixture" info={info} /></div>;
}
createRoot(document.getElementById('root')!).render(<Fixture />);
