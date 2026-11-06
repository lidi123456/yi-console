import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { getT, isEn } from '../i18n';
import { errorToast } from '../store';
import { rpc } from '../transport';
import type { ModelConfigOption, ModelOption, SessionInfo } from '../types';
import { Icon } from './Icon';
import { isMobile } from '../util';

const t = getT('modelPicker');

/** Open the model menu (used by /model in the input box) */
export const OPEN_MODEL = 'ah:open-model';

/** Look up by id, then by the actual model (Claude aliases); "Default" and "Opus" may point at the same model, prefer the more specific one */
function findModel(current: string, models: ModelOption[]) {
  const same = models.filter((o) => o.resolved === current);
  // Old Cursor sessions saved the parameters in the model string: `grok-4.7[context=256k]`
  const base = current.split('[')[0];
  return models.find((o) => o.id === current) ?? same.find((o) => o.id !== 'default') ?? same[0] ?? models.find((o) => o.id === base);
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

/** Cursor parameter ids vary by model; older daemons/saved sessions may omit the ACP category. */
function isReasoningOption(option: ModelConfigOption) {
  return option.category === 'thought_level'
    || ['reasoning', 'reasoning_effort', 'effort', 'thinking', 'thought_level'].includes(option.id);
}

/** Thinking levels supported by the current model */
function currentModel(current: string | null | undefined, models: ModelOption[]) {
  if (!current) return models[0];
  return findModel(current, models);
}

type PickerSection = 'models' | 'effort' | 'context';

/** A desktop submenu floats outside the composer's clipping/stacking context and flips at the window edge. */
function ModelSubmenu({ anchor, menuRef, label, children }: {
  anchor: HTMLButtonElement;
  menuRef: React.RefObject<HTMLDivElement | null>;
  label: string;
  children: React.ReactNode;
}) {
  const [position, setPosition] = useState<{ left: number; top: number } | null>(null);
  useLayoutEffect(() => {
    const element = menuRef.current;
    if (!element) return;
    const place = () => {
      const rect = anchor.getBoundingClientRect();
      const size = element.getBoundingClientRect();
      const right = rect.right + 8;
      const left = right + size.width <= window.innerWidth - 12 ? right : rect.left - size.width - 8;
      const next = {
        left: Math.max(12, Math.min(left, window.innerWidth - size.width - 12)),
        top: Math.max(12, Math.min(rect.top, window.innerHeight - size.height - 12)),
      };
      setPosition((old) => old?.left === next.left && old.top === next.top ? old : next);
    };
    place();
    const observer = new ResizeObserver(place);
    observer.observe(element);
    window.addEventListener('resize', place);
    window.addEventListener('scroll', place, true);
    return () => {
      observer.disconnect();
      window.removeEventListener('resize', place);
      window.removeEventListener('scroll', place, true);
    };
  }, [anchor, menuRef]);
  return createPortal(
    <div ref={menuRef} className="menu model-submenu" role="menu" aria-label={label} data-model-pane
      style={{ left: position?.left ?? 0, top: position?.top ?? 0, visibility: position ? 'visible' : 'hidden' }}>
      {children}
    </div>,
    document.body,
  );
}

export function ModelPicker({ hostId, info }: { hostId: string; info: SessionInfo }) {
  const [open, setOpen] = useState(false);
  const [mobile, setMobile] = useState(isMobile);
  const [section, setSection] = useState<PickerSection | null>(null);
  const [models, setModels] = useState<ModelOption[] | null>(null);
  const [q, setQ] = useState('');
  const [changing, setChanging] = useState<string | null>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const sheetRef = useRef<HTMLDivElement>(null);
  const submenuRef = useRef<HTMLDivElement>(null);
  const anchorRef = useRef<HTMLButtonElement | null>(null);
  const sheet = open && mobile;
  // Only a model whose parameters aren't known yet; choosing never waits for Cursor
  const configPending = info.tool === 'cursor' && !!info.modelConfigPending;
  const close = useCallback((focus = true) => {
    setOpen(false);
    setSection(null);
    setQ('');
    if (focus) buttonRef.current?.focus();
  }, []);
  const back = useCallback(() => {
    const previous = section;
    setSection(null);
    requestAnimationFrame(() => sheetRef.current?.querySelector<HTMLButtonElement>(`[data-section="${previous}"]`)?.focus());
  }, [section]);
  useEffect(() => {
    const media = window.matchMedia('(max-width: 760px)');
    const resize = () => { setMobile(media.matches); setSection(null); };
    media.addEventListener('change', resize);
    return () => media.removeEventListener('change', resize);
  }, []);
  useBack(open, () => close());
  useBack(open && section !== null, back);
  useEffect(() => {
    if (!open) return;
    const onDown = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!wrapRef.current?.contains(target) && !sheetRef.current?.contains(target) && !submenuRef.current?.contains(target)) close(false);
    };
    document.addEventListener('pointerdown', onDown);
    return () => document.removeEventListener('pointerdown', onDown);
  }, [open, close]);
  useEffect(() => {
    if (!open) return;
    // Hovering away from the search submenu can remove the focused input, leaving focus on the body.
    const onEscape = (event: KeyboardEvent) => {
      if (event.key !== 'Escape' || event.defaultPrevented) return;
      event.preventDefault();
      if (section) back(); else close();
    };
    document.addEventListener('keydown', onEscape);
    return () => document.removeEventListener('keydown', onEscape);
  }, [open, section, back, close]);
  const load = () =>
    rpc<{ current: string | null; models: ModelOption[] }>(hostId, 'session.models', { sessionId: info.id })
      .then((r) => setModels(r.models))
      .catch(() => setModels([]));
  useEffect(() => {
    setModels(null);
    setSection(null);
    load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, info.id, info.model]);
  useEffect(() => {
    const show = () => {
      load();
      setSection(null);
      setOpen(true);
    };
    window.addEventListener(OPEN_MODEL, show);
    return () => window.removeEventListener(OPEN_MODEL, show);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [hostId, info.id]);
  const pick = async (id: string) => {
    close();
    if (id === info.model) return;
    setChanging('model');
    try {
      await rpc(hostId, 'session.setModel', { sessionId: info.id, model: id });
    } catch (error) {
      errorToast(error);
    } finally {
      setChanging(null);
    }
  };
  const pickEffort = async (effort: string) => {
    setChanging('effort');
    try {
      await rpc(hostId, 'session.setEffort', { sessionId: info.id, effort });
      back();
    } catch (error) {
      errorToast(error);
    } finally {
      setChanging(null);
    }
  };
  const pickConfig = async (configId: string, value: string) => {
    setChanging(configId);
    try {
      await rpc(hostId, 'session.setModelConfig', { sessionId: info.id, configId, value });
      if (configId !== 'fast') back();
    } catch (error) {
      errorToast(error);
    } finally {
      setChanging(null);
    }
  };
  const cursorOptions = info.tool === 'cursor' ? (info.modelConfigOptions ?? []).filter((o) => o.options.length > 1) : [];
  const fastOption = cursorOptions.find((o) => o.id === 'fast' && ['true', 'false'].every((v) => o.options.some((o) => o.value === v)));
  const reasoningOption = cursorOptions.find(isReasoningOption);
  const contextOption = cursorOptions.find((o) => o.id === 'context');
  const fast = fastOption?.currentValue === 'true';
  const cur = info.model;
  const activeId = cur ? findModel(cur, models ?? [])?.id : undefined;
  const curModel = currentModel(cur, models ?? []);
  const efforts = curModel?.efforts ?? [];
  const effort = reasoningOption?.currentValue ?? info.effort ?? '';
  const hasEffort = !!reasoningOption || efforts.length > 0;
  const hasControls = configPending || !!fastOption || !!contextOption || hasEffort;
  const list = (models ?? []).filter((m) => !q.trim() || `${m.name} ${m.id}`.toLowerCase().includes(q.trim().toLowerCase()));
  const sectionLabel = section === 'effort' ? t('reasoningEffort') : section === 'context' ? t('contextWindow') : t('chooseModel');
  const contextLabel = contextOption?.options.find((o) => o.value === contextOption.currentValue)?.name || contextOption?.currentValue || t('default');
  // A model/capability update can remove the parameter whose submenu is currently open.
  useEffect(() => {
    if (section === 'effort' && !hasEffort || section === 'context' && !contextOption) setSection(null);
  }, [section, hasEffort, contextOption]);
  const showSection = (next: PickerSection, anchor: HTMLButtonElement) => {
    if (changing !== null) return;
    anchorRef.current = anchor;
    setSection(next);
  };
  const settingsRow = (key: PickerSection, label: string, value: string) => (
    <button className={`model-setting-row${section === key ? ' active' : ''}`} data-section={key}
      aria-label={label} aria-haspopup="menu" aria-expanded={section === key}
      onClick={(event) => showSection(key, event.currentTarget)}
      onPointerEnter={(event) => { if (!sheet && event.pointerType === 'mouse') showSection(key, event.currentTarget); }}
      onPointerMove={(event) => { if (!sheet && event.pointerType === 'mouse' && section !== key) showSection(key, event.currentTarget); }}
      onKeyDown={(event) => {
        if (event.key === 'ArrowRight') {
          event.preventDefault();
          showSection(key, event.currentTarget);
          requestAnimationFrame(() => submenuRef.current?.querySelector<HTMLElement>('input, button')?.focus());
        }
      }}>
      <span>{label}</span><span className="model-setting-value">{value}</span><Icon name="chevron" size={13} />
    </button>
  );
  const modelList = (
    <>
      <input className="model-search" aria-label={t('searchModels')} autoFocus={!sheet} value={q}
        onChange={(event) => setQ(event.target.value)} placeholder={t('searchModels')} />
      {models === null && <div className="muted small pad-x">{t('loading')}</div>}
      {models?.length === 0 && <div className="muted small pad-x">{t('noModelsReportedYetTry')}</div>}
      {models && models.length > 0 && list.length === 0 && <div className="muted small pad-x">{t('noMatchingModels')}</div>}
      <div className="model-list">
        {list.map((m) => (
          <button key={m.id} className={m.id === activeId ? 'active' : ''} onClick={() => pick(m.id)} title={m.id}>
            <span className="model-name">{m.name || m.id}{m.id === activeId && <Icon name="check" size={13} />}</span>
            {m.description && <span className="muted small">{m.description}</span>}
          </button>
        ))}
      </div>
    </>
  );
  const choices = section === 'context' ? contextOption?.options ?? [] : reasoningOption?.options.map((o) => ({ value: o.value, name: effortLabel(o.value) }))
    ?? [{ value: '', name: t('default') }, ...efforts.map((value) => ({ value, name: effortLabel(value) }))];
  const choiceValue = section === 'context' ? contextOption?.currentValue : effort;
  const optionList = (
    <div className="model-option-list">
      {choices.map((option) => (
        <button key={option.value} role="menuitemradio" aria-checked={option.value === choiceValue}
          className={option.value === choiceValue ? 'active' : ''} disabled={changing !== null}
          onClick={() => {
            if (section === 'context' && contextOption) void pickConfig(contextOption.id, option.value);
            else if (reasoningOption) void pickConfig(reasoningOption.id, option.value);
            else void pickEffort(option.value);
          }}>
          <span>{option.name}</span>{option.value === choiceValue && <Icon name="check" size={13} />}
        </button>
      ))}
    </div>
  );
  const settings = (
    <div className="model-settings" role="menu" aria-label={t('modelSettings')}>
      {configPending && <div className="model-config-loading muted small" role="status"><span className="spinner" aria-hidden="true" />{t('loadingModelParameters')}</div>}
      {fastOption && (
        <button className="model-setting-row" role="switch" aria-label={t('fastMode')} aria-checked={fast}
          title={t('fastHint')} disabled={changing !== null}
          onPointerEnter={() => { if (!sheet) setSection(null); }}
          onClick={() => void pickConfig(fastOption.id, fast ? 'false' : 'true')}>
          <span>{t('fastMode')}</span><span className={`switch${fast ? ' on' : ''}`} aria-hidden="true"><i /></span>
        </button>
      )}
      {contextOption && settingsRow('context', t('contextWindow'), contextLabel)}
      {hasEffort && settingsRow('effort', t('reasoningEffort'), effort ? effortLabel(effort) : t('default'))}
      <div className="model-settings-divider" />
      {settingsRow('models', t('model'), modelLabel(cur, models ?? []))}
    </div>
  );
  const menuBody = (
    <div ref={sheetRef} className={sheet ? 'model-sheet' : `menu menu-up menu-up-right model-menu${hasControls ? ' model-menu-compact' : ''}`}
      role="dialog" aria-busy={configPending} aria-label={t('modelSettings')} data-model-pane>
      {sheet && (
        <div className="model-sheet-head">
          {section && <button className="icon-btn" aria-label={t('backToModelSettings')} onClick={back}><Icon name="back" size={17} /></button>}
          <span>{section ? sectionLabel : t('modelSettings')}</span>
          <button className="icon-btn" aria-label={t('close')} onClick={() => close()}><Icon name="x" size={16} /></button>
        </div>
      )}
      {sheet && section ? (section === 'models' ? modelList : optionList) : hasControls ? settings : modelList}
    </div>
  );
  const onKeyDown = (event: React.KeyboardEvent) => {
    if (!open || event.defaultPrevented) return;
    if (event.key === 'Escape' || event.key === 'ArrowLeft' && section && !(event.target instanceof HTMLInputElement)) {
      event.preventDefault();
      event.stopPropagation();
      if (section) back(); else close();
    } else if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      const target = event.target as HTMLElement;
      const pane = target.closest('[data-model-pane]') ?? wrapRef.current;
      const items = Array.from(pane?.querySelectorAll<HTMLElement>('input, button:not(:disabled)') ?? []);
      const index = items.indexOf(target);
      if (items.length) {
        event.preventDefault();
        items[(index + (event.key === 'ArrowDown' ? 1 : items.length - 1) + items.length) % items.length].focus();
      }
    }
  };
  return (
    <div className="menu-wrap" ref={wrapRef} onKeyDown={onKeyDown}>
      <button ref={buttonRef} className="model-btn" title={t('switchModel')} aria-haspopup="dialog" aria-expanded={open}
        onClick={() => { if (open) close(); else { load(); setOpen(true); } }}>
        <span className="truncate">{modelLabel(cur, models ?? [])}</span>
        {effort && <span className="model-effort">{effortLabel(effort)}</span>}
        {fast && <span className="model-effort">{t('fastMode')}</span>}
        <Icon name="down" size={12} />
      </button>
      {open && (sheet ? createPortal(
        <div className="model-sheet-bg" onClick={() => close()}><div onClick={(event) => event.stopPropagation()}>{menuBody}</div></div>,
        document.body,
      ) : menuBody)}
      {open && section && !sheet && anchorRef.current && (
        <ModelSubmenu anchor={anchorRef.current} menuRef={submenuRef} label={sectionLabel}>
          {section === 'models' ? modelList : optionList}
        </ModelSubmenu>
      )}
    </div>
  );
}
