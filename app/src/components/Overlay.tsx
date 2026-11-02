import { useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore } from 'react';
import { useBack } from '../back';
import { readClipboard, writeClipboard } from '../clipboard';
import { tr } from '../i18n';
import { Icon } from './Icon';

// Our own input dialog, confirm dialog and context menu, replacing the browser's prompt / confirm / context menu

interface TextReq {
  kind: 'text';
  title: string;
  label?: string;
  value: string;
  placeholder?: string;
  okText: string;
  resolve: (v: string | null) => void;
}

interface ConfirmReq {
  kind: 'confirm';
  title: string;
  message?: string;
  okText: string;
  danger?: boolean;
  resolve: (v: boolean) => void;
}

export interface MenuItem {
  label: string;
  icon?: string;
  hint?: string;
  danger?: boolean;
  disabled?: boolean;
  /** Current item (highlighted) */
  active?: boolean;
  run: () => void;
}

interface MenuReq {
  x: number;
  y: number;
  items: (MenuItem | '-')[];
}

let dialog: TextReq | ConfirmReq | null = null;
let menu: MenuReq | null = null;
const subs = new Set<() => void>();
let version = 0;
const bump = () => {
  version++;
  subs.forEach((f) => f());
};
const useOverlay = () =>
  useSyncExternalStore(
    (cb) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
    () => version,
  );

export function askText(o: { title: string; label?: string; value?: string; placeholder?: string; okText?: string }) {
  return new Promise<string | null>((resolve) => {
    dialog = { kind: 'text', value: '', okText: tr('确定', 'OK'), ...o, resolve } as TextReq;
    bump();
  });
}

export function askConfirm(o: { title: string; message?: string; okText?: string; danger?: boolean }) {
  return new Promise<boolean>((resolve) => {
    dialog = { kind: 'confirm', okText: tr('确定', 'OK'), ...o, resolve } as ConfirmReq;
    bump();
  });
}

export function openMenu(e: { clientX: number; clientY: number; preventDefault?: () => void }, items: (MenuItem | '-')[]) {
  e.preventDefault?.();
  const list = items.filter((x, i, a) => x !== '-' || (i > 0 && a[i - 1] !== '-' && i < a.length - 1));
  if (!list.length) return;
  menu = { x: e.clientX, y: e.clientY, items: list };
  bump();
}

const closeMenu = () => {
  if (menu) {
    menu = null;
    bump();
  }
};

function Dialog({ d }: { d: TextReq | ConfirmReq }) {
  const [v, setV] = useState(d.kind === 'text' ? d.value : '');
  const input = useRef<HTMLInputElement>(null);
  const ok = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    if (d.kind === 'text') {
      input.current?.focus();
      input.current?.select();
    } else ok.current?.focus();
  }, [d]);
  useBack(true, () => done(false));
  const done = (yes: boolean) => {
    dialog = null;
    bump();
    if (d.kind === 'text') d.resolve(yes ? v : null);
    else d.resolve(yes);
  };
  return (
    <div className="modal-bg dlg-bg" onMouseDown={() => done(false)}>
      <div
        className="modal dlg"
        role="dialog"
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={(e) => {
          if (e.key === 'Escape') done(false);
          else if (e.key === 'Enter' && !e.nativeEvent.isComposing && (d.kind === 'confirm' || v.trim())) {
            e.preventDefault();
            done(true);
          }
        }}
      >
        <div className="dlg-title">{d.title}</div>
        {d.kind === 'confirm' && d.message && <div className="dlg-msg">{d.message}</div>}
        {d.kind === 'text' && (
          <label className="dlg-field">
            {d.label && <span className="muted small">{d.label}</span>}
            <input ref={input} value={v} placeholder={d.placeholder} onChange={(e) => setV(e.target.value)} />
          </label>
        )}
        <div className="dlg-actions">
          <button className="btn" onClick={() => done(false)}>
            {tr('取消', 'Cancel')}
          </button>
          <button
            ref={ok}
            className={`btn ${d.kind === 'confirm' && d.danger ? 'danger' : 'primary'}`}
            disabled={d.kind === 'text' && !v.trim()}
            onClick={() => done(true)}
          >
            {d.okText}
          </button>
        </div>
      </div>
    </div>
  );
}

function ContextMenu({ m }: { m: MenuReq }) {
  const ref = useRef<HTMLDivElement>(null);
  const [pos, setPos] = useState({ left: m.x, top: m.y });
  useBack(true, closeMenu);
  // Pull inward when touching the window edge
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    setPos({
      left: Math.max(4, Math.min(m.x, window.innerWidth - r.width - 4)),
      top: Math.max(4, Math.min(m.y, window.innerHeight - r.height - 4)),
    });
  }, [m]);
  useEffect(() => {
    const off = () => closeMenu();
    const key = (e: KeyboardEvent) => e.key === 'Escape' && closeMenu();
    window.addEventListener('blur', off);
    window.addEventListener('resize', off);
    window.addEventListener('keydown', key);
    window.addEventListener('wheel', off, { passive: true });
    return () => {
      window.removeEventListener('blur', off);
      window.removeEventListener('resize', off);
      window.removeEventListener('keydown', key);
      window.removeEventListener('wheel', off);
    };
  }, []);
  return (
    <div
      className="ctx-scrim"
      onMouseDown={closeMenu}
      onContextMenu={(e) => {
        e.preventDefault();
        closeMenu();
      }}
    >
      <div ref={ref} className="ctx-menu" style={pos} onMouseDown={(e) => e.stopPropagation()}>
        {m.items.map((it, i) =>
          it === '-' ? (
            <div key={i} className="ctx-sep" />
          ) : (
            <button
              key={i}
              className={`${it.danger ? 'danger' : ''} ${it.active ? 'active' : ''}`}
              title={it.hint ? `${it.label}  ${it.hint}` : undefined}
              disabled={it.disabled}
              // Don't steal focus: paste and cut must act on the original input
              onMouseDown={(e) => e.preventDefault()}
              onClick={() => {
                closeMenu();
                it.run();
              }}
            >
              <span className="ctx-icon">{it.icon && <Icon name={it.icon} size={14} />}</span>
              <span className="grow">{it.label}</span>
              {it.hint && <span className="ctx-hint">{it.hint}</span>}
            </button>
          ),
        )}
      </div>
    </div>
  );
}

export function Overlays() {
  useOverlay();
  return (
    <>
      {dialog && <Dialog key={version} d={dialog} />}
      {menu && <ContextMenu m={menu} />}
    </>
  );
}

const isMac = /Mac|iPhone|iPad/.test(navigator.platform);
const MOD = isMac ? '⌘' : 'Ctrl+';

type Editable = HTMLInputElement | HTMLTextAreaElement;
const editable = (el: Element | null): Editable | null =>
  el instanceof HTMLTextAreaElement ||
  (el instanceof HTMLInputElement && /^(text|search|url|email|password|number|)$/.test(el.type))
    ? el
    : null;

/** Edit menu: cut / copy / paste / select all in inputs; elsewhere, copy if text is selected */
export function editMenu(target: Element | null): (MenuItem | '-')[] {
  const ed = editable(target);
  if (ed) {
    const sel = ed.value.slice(ed.selectionStart ?? 0, ed.selectionEnd ?? 0);
    const ro = ed.readOnly || ed.disabled;
    const secret = ed.type === 'password';
    return [
      {
        label: tr('剪切', 'Cut'),
        icon: 'cut',
        hint: `${MOD}X`,
        disabled: !sel || ro || secret,
        run: () => {
          ed.focus();
          writeClipboard(sel).then(() => document.execCommand('delete'));
        },
      },
      { label: tr('复制', 'Copy'), icon: 'copy', hint: `${MOD}C`, disabled: !sel || secret, run: () => writeClipboard(sel) },
      {
        label: tr('粘贴', 'Paste'),
        icon: 'paste',
        hint: `${MOD}V`,
        disabled: ro,
        run: async () => {
          const t = await readClipboard(false);
          ed.focus();
          if (t) document.execCommand('insertText', false, t);
        },
      },
      '-',
      {
        label: tr('全选', 'Select All'),
        hint: `${MOD}A`,
        disabled: !ed.value,
        run: () => {
          ed.focus();
          ed.select();
        },
      },
    ];
  }
  const text = window.getSelection()?.toString() ?? '';
  if (!text.trim()) return [];
  return [{ label: tr('复制', 'Copy'), icon: 'copy', hint: `${MOD}C`, run: () => writeClipboard(text) }];
}

/** Suppress the WebView's built-in context menu globally; leave alone events components already handled (preventDefault) */
export function useContextMenu() {
  useEffect(() => {
    const f = (e: MouseEvent) => {
      if (e.defaultPrevented) return;
      e.preventDefault();
      openMenu(e, editMenu(e.target as Element));
    };
    window.addEventListener('contextmenu', f);
    return () => window.removeEventListener('contextmenu', f);
  }, []);
}
