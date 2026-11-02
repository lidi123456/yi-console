import { tr } from '../i18n';
import { toast } from '../store';
import { isTauri } from '../transport';
import { openImage } from './Lightbox';
import { openMenu } from './Overlay';

// 图片的右键菜单：复制图片、另存为、放大查看。对话里的图片和放大查看页共用

function loadImg(src: string) {
  return new Promise<HTMLImageElement>((resolve, reject) => {
    const img = new Image();
    img.crossOrigin = 'anonymous';
    img.onload = () => resolve(img);
    img.onerror = () => reject(new Error(tr('图片加载失败', 'Failed to load image')));
    img.src = src;
  });
}

/** 剪贴板只认 PNG，其他格式先画到 canvas 上转一下 */
async function toPng(src: string): Promise<Blob> {
  const img = await loadImg(src);
  const c = document.createElement('canvas');
  c.width = img.naturalWidth;
  c.height = img.naturalHeight;
  c.getContext('2d')!.drawImage(img, 0, 0);
  return new Promise((resolve, reject) => c.toBlob((b) => (b ? resolve(b) : reject(new Error(tr('转换失败', 'Conversion failed')))), 'image/png'));
}

export async function copyImage(src: string) {
  try {
    const blob = await toPng(src);
    await navigator.clipboard.write([new ClipboardItem({ 'image/png': blob })]);
    toast(tr('已复制图片', 'Image copied'));
  } catch (e) {
    toast(tr(`复制图片失败：${e instanceof Error ? e.message : e}`, `Failed to copy image: ${e instanceof Error ? e.message : e}`), 'error');
  }
}

const EXT: Record<string, string> = { 'image/png': 'png', 'image/jpeg': 'jpg', 'image/gif': 'gif', 'image/webp': 'webp' };

/** data: 地址直接取原始数据，其他地址下载下来 */
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
        if (await invoke<boolean>('save_file', { name, data: base64 })) toast(tr('已保存', 'Saved'));
        return;
      } catch {
        // 手机上没有保存对话框，退回到浏览器下载
      }
    }
    const a = document.createElement('a');
    a.href = `data:${mime};base64,${base64}`;
    a.download = name;
    a.click();
  } catch (e) {
    toast(tr(`保存失败：${e instanceof Error ? e.message : e}`, `Failed to save: ${e instanceof Error ? e.message : e}`), 'error');
  }
}

export function imageMenu(e: { clientX: number; clientY: number; preventDefault?: () => void }, src: string, opts: { zoom?: boolean } = {}) {
  openMenu(e, [
    { label: tr('复制图片', 'Copy Image'), icon: 'copy', run: () => copyImage(src) },
    { label: tr('另存为…', 'Save Image As…'), icon: 'download', run: () => saveImage(src) },
    ...(opts.zoom === false ? [] : ['-' as const, { label: tr('放大查看', 'Zoom In'), icon: 'zoom', run: () => openImage(src) }]),
  ]);
}
