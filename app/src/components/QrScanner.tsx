import jsQR from 'jsqr';
import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { tr } from '../i18n';
import { Icon } from './Icon';

// QR scanning in the page itself: getUserMedia opens the rear camera, and frames are periodically scaled down onto a canvas for jsQR to decode.
// No native scanner plugin: Tauri's official plugin relies on ML Kit from Google Play services, which crashes on opening the camera on phones in mainland China.
// On Android, when the WebView needs the camera, wry's RustWebChromeClient requests the CAMERA permission; the manifest entry is added in src-tauri/build.rs.

/** Whether the camera can be opened in the page in this environment (pages in insecure contexts have no mediaDevices) */
export const canScanQr = () => !!navigator.mediaDevices?.getUserMedia;

// Scale frames so the longest side is this size for decoding: the QR code fills a good part of the viewfinder, so it's enough, and a frame takes well under a few dozen ms on a phone
const MAX_SIDE = 720;
const INTERVAL = 150;

function cameraError(e: unknown): string {
  const name = e instanceof DOMException ? e.name : '';
  if (name === 'NotAllowedError' || name === 'SecurityError')
    return tr('没有相机权限。请在系统设置里允许 Yi Console 使用相机，或者直接粘贴配对链接。', 'No camera permission. Allow Yi Console to use the camera in system settings, or paste the pairing link.');
  if (name === 'NotFoundError' || name === 'OverconstrainedError') return tr('没有找到可用的相机', 'No camera found');
  if (name === 'NotReadableError' || name === 'AbortError') return tr('相机被其他应用占用，或者打不开', 'The camera is in use by another app or could not be opened');
  return tr(`打不开相机：${String(e)}`, `Could not open the camera: ${String(e)}`);
}

export function QrScanner({ onResult, onClose }: { onResult: (text: string) => void; onClose: () => void }) {
  const video = useRef<HTMLVideoElement>(null);
  const [error, setError] = useState<string | null>(null);
  const [ready, setReady] = useState(false);
  const done = useRef({ onResult, onClose });
  done.current = { onResult, onClose };
  useBack(true, onClose);

  useEffect(() => {
    let stream: MediaStream | null = null;
    let timer = 0;
    let stopped = false;
    const canvas = document.createElement('canvas');
    const ctx = canvas.getContext('2d', { willReadFrequently: true });

    const tick = () => {
      if (stopped) return;
      const v = video.current;
      if (v && ctx && v.readyState >= v.HAVE_CURRENT_DATA && v.videoWidth) {
        const k = Math.min(1, MAX_SIDE / Math.max(v.videoWidth, v.videoHeight));
        const w = Math.round(v.videoWidth * k);
        const h = Math.round(v.videoHeight * k);
        if (canvas.width !== w || canvas.height !== h) {
          canvas.width = w;
          canvas.height = h;
        }
        ctx.drawImage(v, 0, 0, w, h);
        // Pairing QR codes are dark-on-light even in the dark theme, but screenshots or inverted displays should still work, so try both
        const r = jsQR(ctx.getImageData(0, 0, w, h).data, w, h, { inversionAttempts: 'attemptBoth' });
        if (r?.data) {
          stopped = true;
          navigator.vibrate?.(50);
          done.current.onResult(r.data);
          return;
        }
      }
      timer = window.setTimeout(tick, INTERVAL);
    };

    (async () => {
      try {
        stream = await navigator.mediaDevices.getUserMedia({
          audio: false,
          video: { facingMode: { ideal: 'environment' }, width: { ideal: 1280 }, height: { ideal: 720 } },
        });
        if (stopped) return;
        const v = video.current!;
        v.srcObject = stream;
        await v.play();
        setReady(true);
        tick();
      } catch (e) {
        if (!stopped) setError(cameraError(e));
      }
    })();

    return () => {
      stopped = true;
      clearTimeout(timer);
      // Turn the camera off: unless the tracks are stopped, the camera indicator stays on and other apps can't use it
      stream?.getTracks().forEach((t) => t.stop());
      if (video.current) video.current.srcObject = null;
    };
  }, []);

  return createPortal(
    <div className="qr-scan" role="dialog" onKeyDown={(e) => e.key === 'Escape' && onClose()}>
      <video ref={video} className="qr-scan-video" playsInline muted autoPlay />
      {!error && <div className={'qr-scan-frame' + (ready ? '' : ' waiting')} />}
      <div className="qr-scan-bar">
        <button className="qr-scan-close" onClick={onClose} aria-label={tr('关闭', 'Close')}>
          <Icon name="x" size={22} />
        </button>
      </div>
      <div className="qr-scan-tip">
        {error ?? (ready ? tr('把电脑上的配对二维码放进框里', 'Fit the pairing QR code from the computer inside the frame') : tr('正在打开相机…', 'Opening the camera…'))}
      </div>
    </div>,
    document.body,
  );
}
