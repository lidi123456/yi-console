import { useEffect, useRef } from 'react';
import { useBack } from '../back';
import { getT } from '../i18n';
import { permLabel, permModes } from '../perm';
import type { SessionMode, ToolKind } from '../types';
import { Icon } from './Icon';

const t = getT('composer');

/** Permission mode button and menu above it; used by the session input box and the new session page */
export function PermPicker({
  tool,
  mode,
  value,
  onChange,
  open,
  setOpen,
}: {
  tool: ToolKind;
  mode: SessionMode;
  value: string | null | undefined;
  onChange: (mode: string) => void;
  open: boolean;
  setOpen: (v: boolean) => void;
}) {
  const cur = value || 'default';
  const wrapRef = useRef<HTMLDivElement>(null);
  useBack(open, () => setOpen(false));
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener('pointerdown', onDown);
    return () => document.removeEventListener('pointerdown', onDown);
  }, [open, setOpen]);
  return (
    <div className="menu-wrap" ref={wrapRef}>
      <button type="button" className={`perm-btn perm-${cur}`} title={t('permissionModeShiftTabTo')} onClick={() => setOpen(!open)}>
        <Icon name="shield" size={14} />
        <span className="perm-label">{permLabel(cur)}</span>
        <Icon name="down" size={11} />
      </button>
      {open && (
        <div className="menu menu-up perm-menu" onMouseLeave={() => setOpen(false)}>
          {permModes(tool, mode).map(([id, label, desc]) => (
            <button
              key={id}
              className={id === cur ? 'active' : ''}
              onClick={() => {
                setOpen(false);
                onChange(id);
              }}
            >
              <span className="model-name">
                <span className={`perm-dot perm-${id}`} />
                {label}
                {id === cur && <Icon name="check" size={13} />}
              </span>
              <span className="muted small">{desc}</span>
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/** The next permission mode for Shift+Tab */
export function nextPerm(tool: ToolKind, mode: SessionMode, cur: string | null | undefined) {
  const ids = permModes(tool, mode).map((m) => m[0]);
  if (!ids.length) return cur || 'default';
  return ids[(ids.indexOf(cur || 'default') + 1) % ids.length];
}
