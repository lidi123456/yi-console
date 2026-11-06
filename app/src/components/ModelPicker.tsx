import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { getT, isEn } from '../i18n';
import { errorToast } from '../store';
import { rpc } from '../transport';
import type { ModelOption, SessionInfo } from '../types';
import { Icon } from './Icon';
import { isMobile } from '../util';

const t = getT('modelPicker');

/** Open the model menu (used by /model in the input box) */
export const OPEN_MODEL = 'ah:open-model';

/** Look up by id, then by the actual model (Claude aliases); "Default" and "Opus" may point at the same model, prefer the more specific one */
function findModel(current: string, models: ModelOption[]) {
  const same = models.filter((o) => o.resolved === current);
  return models.find((o) => o.id === current) ?? same.find((o) => o.id !== 'default') ?? same[0];
}

/** Model name: use the display name if it's in the list */
function modelLabel(current: string | null | undefined, models: ModelOption[]) {
  if (!current) return t('defaultModel');
  return findModel(current, models)?.name || current;
}

const EFFORT_LABEL_ZH: Record<string, string> = {
  none: '无',
  'extra-high': '很高',
  minimal: '最少',
  low: '低',
  medium: '中',
  high: '高',
  xhigh: '很高',
  max: '最高',
  ultra: '极限',
};

const EFFORT_LABEL_EN: Record<string, string> = {
  none: 'None',
  'extra-high': 'Extra High',
  minimal: 'Minimal',
  low: 'Low',
  medium: 'Medium',
  high: 'High',
  xhigh: 'X-High',
  max: 'Max',
  ultra: 'Ultra',
};

export const EFFORT_LABEL: Record<string, string> = new Proxy(EFFORT_LABEL_ZH, {
  get(target, prop: string) {
    if (isEn() && prop in EFFORT_LABEL_EN) return EFFORT_LABEL_EN[prop];
    return target[prop];
  },
});

const effortLabel = (e: string) => EFFORT_LABEL[e] ?? e;

/** Thinking levels supported by the current model */
function currentModel(current: string | null | undefined, models: ModelOption[]) {
  if (!current) return models[0];
  return findModel(current, models);
}

export function ModelPicker({ hostId, info }: { hostId: string; info: SessionInfo }) {
  const [open, setOpen] = useState(false);
  const [models, setModels] = useState<ModelOption[] | null>(null);
  const [q, setQ] = useState('');
  const wrapRef = useRef<HTMLDivElement>(null);
  const sheetRef = useRef<HTMLDivElement>(null);
  /** On mobile it's a bottom sheet (attached to body); on desktop a dropdown above the input box */
  const sheet = open && isMobile();
  useBack(open, () => setOpen(false));
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      const t = e.target as Node;
      if (!wrapRef.current?.contains(t) && !sheetRef.current?.contains(t)) setOpen(false);
    };
    document.addEventListener('pointerdown', onDown);
    return () => document.removeEventListener('pointerdown', onDown);
  }, [open]);
  const load = () =>
    rpc<{ current: string | null; models: ModelOption[] }>(hostId, 'session.models', { sessionId: info.id })
      .then((r) => setModels(r.models))
      .catch(() => setModels([]));
  useEffect(() => {
    setModels(null);
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, info.id, info.model]);
  // /model typed in the input box
  useEffect(() => {
    const f = () => {
      load();
      setOpen(true);
    };
    window.addEventListener(OPEN_MODEL, f);
    return () => window.removeEventListener(OPEN_MODEL, f);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, info.id]);
  const pick = async (id: string) => {
    setOpen(false);
    try {
      // No toast: the model name at the bottom right of the input box changes anyway
      await rpc(hostId, 'session.setModel', { sessionId: info.id, model: id });
    } catch (e) {
      errorToast(e);
    }
  };
  const pickEffort = async (effort: string) => {
    try {
      await rpc(hostId, 'session.setEffort', { sessionId: info.id, effort });
    } catch (e) {
      errorToast(e);
    }
  };
  const pickConfig = async (configId: string, value: string) => {
    try {
      await rpc(hostId, 'session.setModelConfig', { sessionId: info.id, configId, value });
    } catch (e) {
      errorToast(e);
    }
  };
  const cursorOptions = info.tool === 'cursor'
    ? (info.modelConfigOptions ?? []).filter((o) => o.id === 'fast' || o.id === 'reasoning')
    : [];
  const fast = cursorOptions.find((o) => o.id === 'fast')?.currentValue === 'true';
  const cursorEffort = cursorOptions.find((o) => o.id === 'reasoning')?.currentValue;
  const cur = info.model;
  const activeId = cur ? findModel(cur, models ?? [])?.id : undefined;
  const curModel = currentModel(cur, models ?? []);
  const efforts = curModel?.efforts ?? [];
  const effort = info.effort ?? '';
  const list = (models ?? []).filter(
    (m) => !q.trim() || `${m.name} ${m.id}`.toLowerCase().includes(q.trim().toLowerCase()),
  );
  const menuBody = (
        <div
          ref={sheetRef}
          className={sheet ? 'model-sheet' : 'menu menu-up menu-up-right model-menu'}
          role="dialog"
          onMouseLeave={sheet ? undefined : () => setOpen(false)}
        >
          {sheet && (
            <div className="model-sheet-head">
              <span>{t('chooseModel')}</span>
              <button className="icon-btn" aria-label={t('close')} onClick={() => setOpen(false)}>
                <Icon name="x" size={16} />
              </button>
            </div>
          )}
          {(models?.length ?? 0) > 8 && (
            <input autoFocus={!sheet} value={q} onChange={(e) => setQ(e.target.value)} placeholder={t('searchModels')} />
          )}
          {models === null && <div className="muted small pad-x">{t('loading')}</div>}
          {models?.length === 0 && (
            <div className="muted small pad-x">{t('noModelsReportedYetTry')}</div>
          )}
          <div className="model-list">
            {list.map((m) => {
              const active = m.id === activeId;
              return (
                <button key={m.id} className={active ? 'active' : ''} onClick={() => pick(m.id)} title={m.id}>
                  <span className="model-name">
                    {m.name || m.id}
                    {active && <Icon name="check" size={13} />}
                  </span>
                  {m.description && <span className="muted small">{m.description}</span>}
                </button>
              );
            })}
          </div>
          {cursorOptions.map((o) => (
            <div className="effort-row" key={o.id}>
              <div className="muted small">{o.id === 'fast' ? t('fastMode') : t('reasoningEffort')}</div>
              <div className="seg effort-seg">
                {o.options.map((v) => (
                  <button
                    key={v.value}
                    className={o.currentValue === v.value ? 'active' : ''}
                    onClick={() => pickConfig(o.id, v.value)}
                  >
                    {o.id === 'fast' ? (v.value === 'true' ? t('enabled') : t('disabled')) : effortLabel(v.value)}
                  </button>
                ))}
              </div>
              {o.id === 'fast' && <div className="muted small" title={o.description}>{t('fastHint')}</div>}
            </div>
          ))}
          {efforts.length > 0 && (
            <div className="effort-row">
              <div className="muted small">{t('reasoningEffort')}</div>
              <div className="seg effort-seg">
                <button
                  className={!effort ? 'active' : ''}
                  title={
                    curModel?.defaultEffort
                      ? t('toolDefault', { v: effortLabel(curModel.defaultEffort) })
                      : t('toolDefault2')
                  }
                  onClick={() => pickEffort('')}
                >
                  {t('default')}
                </button>
                {efforts.map((e) => (
                  <button key={e} className={effort === e ? 'active' : ''} onClick={() => pickEffort(e)}>
                    {effortLabel(e)}
                  </button>
                ))}
              </div>
            </div>
          )}
        </div>
  );
  return (
    <div className="menu-wrap" ref={wrapRef}>
      <button
        className="model-btn"
        title={t('switchModel')}
        onClick={() => {
          if (!open) load();
          setOpen(!open);
        }}
      >
        <span className="truncate">{modelLabel(cur, models ?? [])}</span>
        {(cursorEffort || effort) && <span className="model-effort">{effortLabel(cursorEffort || effort)}</span>}
        {fast && <span className="model-effort">{t('fastMode')}</span>}
        <Icon name="down" size={12} />
      </button>
      {open &&
        (sheet
          ? createPortal(
              <div className="model-sheet-bg" onClick={() => setOpen(false)}>
                <div onClick={(e) => e.stopPropagation()}>{menuBody}</div>
              </div>,
              document.body,
            )
          : menuBody)}
    </div>
  );
}
