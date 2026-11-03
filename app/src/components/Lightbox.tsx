import { useEffect, useRef, useState, useSyncExternalStore } from 'react';
import { useBack } from '../back';
import { getT } from '../i18n';
import { Icon } from './Icon';
import { imageMenu, saveImage } from './ImageMenu';

const t = getT('lightbox');

// Enlarged image view: wheel to zoom, drag to pan, rotate; Esc or clicking empty space closes it

let current: string | null = null;
const subs = new Set<() => void>();
const set = (v: string | null) => {
  current = v;
  subs.forEach((f) => f());
};

export const openImage = (src: string) => set(src);

export function Lightbox() {
  const src = useSyncExternalStore(
    (cb) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
    () => current,
  );
  return src ? <Viewer key={src} src={src} /> : null;
}

function Viewer({ src }: { src: string }) {
  const box = useRef<HTMLDivElement>(null);
  const [scale, setScale] = useState(1);
  const [pos, setPos] = useState({ x: 0, y: 0 });
  const [rot, setRot] = useState(0);
  const pointers = useRef(new Map<number, { x: number; y: number }>());
  const gesture = useRef<{ x: number; y: number; px: number; py: number; dist: number; s: number; moved: boolean } | null>(null);
  const close = () => set(null);
  useBack(true, close);

  useEffect(() => {
    const k = (e: KeyboardEvent) => {
      if (e.key === 'Escape') close();
      else if (e.key === '+' || e.key === '=') zoom(1.25);
      else if (e.key === '-') zoom(0.8);
      else if (e.key === '0') reset();
      else if (e.key === 'r') setRot((r) => r + 90);
      else if (e.key === 'R') setRot((r) => r - 90);
    };
    window.addEventListener('keydown', k);
    return () => window.removeEventListener('keydown', k);
  });

  const zoom = (f: number) => setScale((s) => Math.min(8, Math.max(0.2, s * f)));
  const reset = () => {
    setScale(1);
    setPos({ x: 0, y: 0 });
    setRot(0);
  };

  const onDown = (e: React.PointerEvent) => {
    if ((e.target as HTMLElement).closest('.lightbox-bar')) return;
    box.current?.setPointerCapture(e.pointerId);
    pointers.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
    const ps = [...pointers.current.values()];
    const mid = ps.length >= 2 ? { x: (ps[0].x + ps[1].x) / 2, y: (ps[0].y + ps[1].y) / 2 } : ps[0];
    const dist = ps.length >= 2 ? Math.hypot(ps[0].x - ps[1].x, ps[0].y - ps[1].y) : 0;
    gesture.current = { x: mid.x, y: mid.y, px: pos.x, py: pos.y, dist, s: scale, moved: ps.length >= 2 };
  };

  const onMove = (e: React.PointerEvent) => {
    if (!pointers.current.has(e.pointerId) || !gesture.current) return;
    pointers.current.set(e.pointerId, { x: e.clientX, y: e.clientY });
    const g = gesture.current;
    const ps = [...pointers.current.values()];
    if (ps.length >= 2 && g.dist) {
      g.moved = true;
      const dist = Math.hypot(ps[0].x - ps[1].x, ps[0].y - ps[1].y);
      const next = Math.min(8, Math.max(0.2, (g.s * dist) / g.dist));
      const r = box.current!.getBoundingClientRect();
      const cx = g.x - r.left - r.width / 2;
      const cy = g.y - r.top - r.height / 2;
      const k = next / g.s;
      setScale(next);
      setPos({ x: cx - (cx - g.px) * k, y: cy - (cy - g.py) * k });
    } else if (ps.length === 1) {
      if (Math.abs(e.clientX - g.x) + Math.abs(e.clientY - g.y) > 4) g.moved = true;
      if (g.moved) setPos({ x: g.px + e.clientX - g.x, y: g.py + e.clientY - g.y });
    }
  };

  const onUp = (e: React.PointerEvent) => {
    const g = gesture.current;
    pointers.current.delete(e.pointerId);
    const ps = [...pointers.current.values()];
    if (ps.length) {
      gesture.current = { x: ps[0].x, y: ps[0].y, px: pos.x, py: pos.y, dist: 0, s: scale, moved: true };
      return;
    }
    gesture.current = null;
    // Close on a click outside the image that wasn't a drag
    if (g && !g.moved && (e.target as HTMLElement).tagName !== 'IMG') close();
  };

  return (
    <div
      ref={box}
      className="lightbox"
      onWheel={(e) => zoom(e.deltaY < 0 ? 1.12 : 0.9)}
      onPointerDown={onDown}
      onPointerMove={onMove}
      onPointerUp={onUp}
      onPointerCancel={onUp}
    >
      <img
        src={src}
        alt=""
        draggable={false}
        style={{ transform: `translate(${pos.x}px, ${pos.y}px) rotate(${rot}deg) scale(${scale})` }}
        onDoubleClick={() => (scale === 1 ? setScale(2) : reset())}
        onContextMenu={(e) => imageMenu(e, src, { zoom: false })}
      />
      <div className="lightbox-bar" onMouseDown={(e) => e.stopPropagation()} onMouseUp={(e) => e.stopPropagation()}>
        <button title={t('zoomOut')} onClick={() => zoom(0.8)}>
          <Icon name="win-min" size={16} />
        </button>
        <button className="lightbox-pct" title={t('originalSize0')} onClick={reset}>
          {Math.round(scale * 100)}%
        </button>
        <button title={t('zoomIn')} onClick={() => zoom(1.25)}>
          <Icon name="plus" size={16} />
        </button>
        <button title={t('rotateLeftShiftR')} onClick={() => setRot((r) => r - 90)}>
          <Icon name="rotate-l" size={16} />
        </button>
        <button title={t('rotateRightR')} onClick={() => setRot((r) => r + 90)}>
          <Icon name="rotate-r" size={16} />
        </button>
        <button title={t('saveAs')} onClick={() => saveImage(src)}>
          <Icon name="download" size={16} />
        </button>
        <button title={t('closeEsc')} onClick={close}>
          <Icon name="x" size={16} />
        </button>
      </div>
    </div>
  );
}
