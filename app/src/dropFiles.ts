/** Dragging a file or tab from the file panel to the composer: the payload is `@relative-path ` */
export const DRAG_REF = 'application/x-yi-console-ref';

export interface DroppedLocalEntry {
  path: string;
  isDir: boolean;
}

export interface ExtractedDropEntry {
  file: File;
  isDirHint: boolean;
}

export interface ComposerDropPlan {
  localPaths: (string | undefined)[];
  dirHints: boolean[];
  shouldOpenInPanel: boolean;
}

export type ChatDropItem =
  | { kind: 'open-local'; path: string }
  | { kind: 'error-dir'; name: string }
  | { kind: 'upload-and-open'; file: File };

/** True when a drag carries OS/external files and is NOT an internal file-panel reference drag */
export function hasExternalFiles(dt: Pick<DataTransfer, 'types'> | null | undefined): boolean {
  if (!dt?.types) return false;
  const types = Array.from(dt.types);
  return types.includes('Files') && !types.includes(DRAG_REF);
}

/** Synchronously snapshot dropped File objects and directory hints before the browser clears DataTransfer */
export function extractDroppedEntries(dt: DataTransfer | null | undefined): ExtractedDropEntry[] {
  if (!dt) return [];
  const out: ExtractedDropEntry[] = [];
  if (dt.items && dt.items.length > 0) {
    for (const item of Array.from(dt.items)) {
      if (item.kind !== 'file') continue;
      const entry = (item as DataTransferItem & { webkitGetAsEntry?: () => { isDirectory?: boolean } | null }).webkitGetAsEntry?.();
      const file = item.getAsFile();
      if (file) out.push({ file, isDirHint: Boolean(entry?.isDirectory) });
    }
  }
  if (out.length > 0) return out;
  return Array.from(dt.files ?? []).map((file) => ({ file, isDirHint: false }));
}

/** Whether a saved tab path points to another session's temporary upload directory */
export function isOtherSessionUpload(path: string, sessionId: string): boolean {
  const m = /[\\/]uploads[\\/]([^\\/]+)[\\/]files[\\/]/i.exec(path);
  return !!m && m[1] !== sessionId;
}

/** Classify Composer drops: images stay inline attachments only; non-image files use local paths when available and optionally open in FilePanel */
export function planComposerDrop<T extends { name: string; type: string }>(
  files: readonly T[],
  opts: {
    isLocalHost: boolean;
    resolved: (DroppedLocalEntry | null)[] | null;
    dirHints?: readonly boolean[];
    imageTypes: readonly string[];
    openInPanel: boolean;
  },
): ComposerDropPlan {
  const localPaths: (string | undefined)[] = [];
  const dirHints: boolean[] = [];
  for (let i = 0; i < files.length; i++) {
    const f = files[i];
    const isImage = opts.imageTypes.includes(f.type);
    const entry = opts.isLocalHost ? opts.resolved?.[i] ?? null : null;
    const isDir = entry ? entry.isDir : Boolean(opts.dirHints?.[i]);
    dirHints.push(isDir);
    if (!isImage && entry && !entry.isDir && entry.path) {
      localPaths.push(entry.path);
    } else {
      localPaths.push(undefined);
    }
  }
  return {
    localPaths,
    dirHints,
    shouldOpenInPanel: opts.openInPanel,
  };
}

/** Classify Chat-area drops: all files (including images) and local directories open in FilePanel without modifying Composer */
export function planChatDrop<T extends { name: string; type: string }>(
  files: readonly T[],
  opts: {
    isLocalHost: boolean;
    resolved: (DroppedLocalEntry | null)[] | null;
    dirHints?: readonly boolean[];
  },
): ChatDropItem[] {
  const out: ChatDropItem[] = [];
  for (let i = 0; i < files.length; i++) {
    const f = files[i];
    const entry = opts.isLocalHost ? opts.resolved?.[i] ?? null : null;
    if (entry?.path) {
      out.push({ kind: 'open-local', path: entry.path });
    } else if (opts.dirHints?.[i]) {
      out.push({ kind: 'error-dir', name: f.name });
    } else {
      out.push({ kind: 'upload-and-open', file: f as unknown as File });
    }
  }
  return out;
}

interface DropPathsPayload {
  id: string;
  entries: (DroppedLocalEntry | null)[];
}

const pendingDrops = new Map<string, (entries: (DroppedLocalEntry | null)[] | null) => void>();
let dropListenerPromise: Promise<void> | null = null;
let dropSeq = 0;

async function ensureDropListener(): Promise<void> {
  if (!dropListenerPromise) {
    dropListenerPromise = import('@tauri-apps/api/event')
      .then(({ listen }) =>
        listen<DropPathsPayload>('drop-paths', (ev) => {
          const resolve = pendingDrops.get(ev.payload?.id);
          if (!resolve) return;
          pendingDrops.delete(ev.payload.id);
          resolve(Array.isArray(ev.payload.entries) ? ev.payload.entries : null);
        }),
      )
      .then(() => undefined)
      .catch((err) => {
        dropListenerPromise = null;
        throw err;
      });
  }
  return dropListenerPromise;
}

/**
 * On Windows desktop (Tauri WebView2), resolve real disk paths for dropped File objects using
 * `chrome.webview.postMessageWithAdditionalObjects` without enabling Tauri's native dragDropEnabled handler.
 * Returns null on non-Windows / browser / unsupported environments or timeout.
 */
export async function resolveDroppedLocalPaths(
  files: File[],
  opts?: { timeoutMs?: number },
): Promise<(DroppedLocalEntry | null)[] | null> {
  if (typeof window === 'undefined' || !files.length) return null;
  if (!('__TAURI_INTERNALS__' in window)) return null;
  if (typeof window.matchMedia === 'function' && window.matchMedia('(max-width: 760px)').matches) return null;
  const webview = (window as unknown as {
    chrome?: { webview?: { postMessageWithAdditionalObjects?: (msg: unknown, objs: File[]) => void } };
  }).chrome?.webview;
  if (typeof webview?.postMessageWithAdditionalObjects !== 'function') return null;

  try {
    await ensureDropListener();
  } catch {
    return null;
  }

  const id = `drop-${++dropSeq}`;
  const timeoutMs = opts?.timeoutMs ?? 1500;
  return new Promise<(DroppedLocalEntry | null)[] | null>((resolve) => {
    const timer = setTimeout(() => {
      if (!pendingDrops.has(id)) return;
      pendingDrops.delete(id);
      resolve(null);
    }, timeoutMs);

    pendingDrops.set(id, (entries) => {
      clearTimeout(timer);
      resolve(entries && entries.length === files.length ? entries : null);
    });

    try {
      webview.postMessageWithAdditionalObjects!({ __yiDropPaths: id }, files);
    } catch {
      clearTimeout(timer);
      pendingDrops.delete(id);
      resolve(null);
    }
  });
}
