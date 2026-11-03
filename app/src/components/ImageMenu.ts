import { getT } from '../i18n';
import { toast } from '../store';
import { isTauri } from '../transport';
import { openImage } from './Lightbox';
import { openMenu } from './Overlay';

const t = getT('imageMenu');

// Image context menu: copy image, save as, view enlarged. Shared by chat images and the enlarged view

function loadImg(src: string) {
  return new Promise<HTMLImageElement>((resolve, reject) => {
    const img = new Image();
    img.crossOrigin = 'anonymous';
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error(t('failedToLoadImage')));
    img.src = src;
  });
}

/** The clipboard only accepts PNG; draw other formats to a canvas and convert first */
async function toPng(src: string): Promise<Blob> {
  const img = await loadImg(src);
  const c = document.createElement('canvas');
  c.width = img.naturalWidth;
  c.height = img.naturalHeight;
  c.getContext('2d')!.drawImage(img, 0, 0);
  return new Promise((resolve, reject) => c.toBlob((b) => (b ? resolve(b) : reject(new Error(t('conversionFailed')))), 'image/png'));
}

export async function copyImage(src: string) {
  try {
    const blob = await toPng(src);
    await navigator.clipboard.write([new ClipboardItem({ 'image/png': blob })]);
    toast(t('imageCopied'));
  } catch (e) {
    toast(t('failedToCopyImage', { v: e instanceof Error ? e.message : String(e) }), 'error');
  }
}

const EXT: Record<string, string> = { 'image/png': 'png', 'image/jpeg': 'jpg', 'image/gif': 'gif', 'image/webp': 'webp' };

/** Take the raw data straight from data: URLs; download other URLs */
async function imageData(src: string): Promise<{ mime: string; base64: string }> {
  const m = /^data:([^;,]+);base64,(.*)$/.exec(src);
  if (m) return { mime: m[1], base64: m[2] };
  const blob = await (await fetch(src)).blob();
  const buf = new Uint8Array(await blob.arrayBuffer());
  let bin = '';
  for (let i = 0; i < buf.length; i += 0x8000) bin += String.fromCharCode(...buf.subarray(i, i + 0x8000));
  return { mime: blob.type, base64: btoa(bin) };
}

export async function saveImage(src: string) {
  try {
    const { mime, base64 } = await imageData(src);
    const name = `image-${new Date().toISOString().slice(0, 19).replace(/[-:T]/g, '')}.${EXT[mime] ?? 'png'}`;
    if (isTauri) {
      const { invoke } = await import('@tauri-apps/api/core');
      try {
        if (await invoke<boolean>('save_file', { name, data: base64 })) toast(t('saved'));
        return;
      } catch {
        // No save dialog on mobile, fall back to a browser download
      }
    }
    const a = document.createElement('a');
    a.href = `data:${mime};base64,${base64}`;
    a.download = name;
    a.click();
  } catch (e) {
    toast(t('failedToSave', { v: e instanceof Error ? e.message : String(e) }), 'error');
  }
}

export function imageMenu(e: { clientX: number; clientY: number; preventDefault?: () => void }, src: string, opts: { zoom?: boolean } = {}) {
  openMenu(e, [
    { label: t('copyImage'), icon: 'copy', run: () => copyImage(src) },
    { label: t('saveImageAs'), icon: 'download', run: () => saveImage(src) },
    ...(opts.zoom === false ? [] : ['-' as const, { label: t('zoomIn'), icon: 'zoom', run: () => openImage(src) }]),
  ]);
}
