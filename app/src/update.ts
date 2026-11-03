// App self-update from GitHub Releases.
// Desktop: tauri-plugin-updater behind the update_check / update_install commands (app/src-tauri/src/updater.rs), reading latest.json.
// Android: the YiConsoleAndroid bridge in MainActivity.kt reads latest-android.json, downloads and verifies the APK and opens the
// system installer; it reports back through window.__yiUpdate. Browser (yi-web) and iOS: not supported, nothing is shown.
// Releases are made by CI from a v* tag (.github/workflows/build.yml, scripts/release-manifest.mjs).

import { useSyncExternalStore } from 'react';
import { errText } from './store';
import { isTauri } from './transport';
import { lsGet, lsSet } from './util';

export type UpdatePhase = 'idle' | 'checking' | 'latest' | 'available' | 'downloading' | 'installing' | 'permission' | 'error';

export interface UpdateState {
  phase: UpdatePhase;
  /** Version of the running app */
  current: string;
  /** The newer version found */
  version?: string;
  notes?: string;
  got?: number;
  total?: number | null;
  /** Error of the last manual check or of the install */
  error?: string;
  checkedAt?: number;
  /** false: this install can't update itself (Linux .deb / .rpm), offer the release page instead */
  installable?: boolean;
}

/** Where releases are published (same repository as the endpoints in tauri.conf.json and MainActivity.kt) */
export const RELEASES_URL = 'https://github.com/lidi123456/yi-console/releases/latest';

interface AndroidBridge {
  appVersion?: () => string;
  updateCheck?: () => void;
  updateDownload?: () => void;
}

const ua = typeof navigator !== 'undefined' ? navigator.userAgent : '';
const android = (): AndroidBridge | undefined => (window as unknown as { YiConsoleAndroid?: AndroidBridge }).YiConsoleAndroid;
const platform: 'desktop' | 'android' | null = !isTauri
  ? null
  : /Android/i.test(ua)
    ? 'android'
    : /iPhone|iPad/i.test(ua)
      ? null
      : 'desktop';

export const updatePlatform = platform;

/** Whether this build can update itself (the settings card and the corner notice only show then) */
export const updateSupported = platform === 'desktop' || (platform === 'android' && !!android()?.updateCheck);

const AUTO_KEY = 'update-auto';
const DISMISS_KEY = 'update-dismissed';
const FIRST_CHECK_MS = 15_000;
const CHECK_EVERY_MS = 6 * 3600_000;

let state: UpdateState = { phase: 'idle', current: '' };
const subs = new Set<() => void>();
function set(patch: Partial<UpdateState>) {
  state = { ...state, ...patch };
  for (const f of subs) f();
}

export function useUpdate(): UpdateState {
  return useSyncExternalStore(
    (cb) => {
      subs.add(cb);
      return () => subs.delete(cb);
    },
    () => state,
  );
}

// ---------- Android events ----------

let androidCheck: ((r: { ok: boolean; available?: boolean; version?: string; notes?: string; current?: string; message?: string }) => void) | null =
  null;

type AndroidEvent =
  | { type: 'check'; ok: boolean; current?: string; available?: boolean; version?: string; notes?: string; message?: string }
  | { type: 'progress'; got: number; total: number }
  | { type: 'downloaded' }
  | { type: 'permission'; granted?: boolean }
  | { type: 'installing' }
  | { type: 'error'; message: string };

if (platform === 'android') {
  (window as unknown as { __yiUpdate: (e: AndroidEvent) => void }).__yiUpdate = (e) => {
    switch (e.type) {
      case 'check':
        androidCheck?.(e);
        androidCheck = null;
        break;
      case 'progress':
        set({ phase: 'downloading', got: e.got, total: e.total > 0 ? e.total : null });
        break;
      case 'downloaded':
        set({ phase: 'installing' });
        break;
      case 'permission':
        set({ phase: 'permission' });
        break;
      case 'installing':
        // The system installer is in front now; if the user cancels it there, "Install again" starts over
        set({ phase: 'installing' });
        break;
      case 'error':
        set({ phase: 'error', error: errText(e.message) });
        break;
    }
  };
}

// ---------- Check / install ----------

async function currentVersion(): Promise<string> {
  if (platform === 'android') return android()?.appVersion?.() ?? '';
  const { getVersion } = await import('@tauri-apps/api/app');
  return getVersion();
}

let checking: Promise<void> | null = null;

/** Look for a newer release. A silent (automatic) check keeps the current phase and error on failure */
export function checkUpdate(silent = false): Promise<void> {
  if (!updateSupported) return Promise.resolve();
  if (checking) return checking;
  // Don't disturb a download or install in progress
  if (['downloading', 'installing', 'permission'].includes(state.phase)) return Promise.resolve();
  const before = state.phase;
  if (!silent) set({ phase: 'checking', error: undefined });
  checking = (async () => {
    try {
      let r: { current: string; version?: string; notes?: string; available: boolean; installable?: boolean };
      if (platform === 'desktop') {
        const { invoke } = await import('@tauri-apps/api/core');
        const v = await invoke<{ current: string; installable?: boolean; update: { version: string; notes?: string | null } | null }>(
          'update_check',
        );
        r = {
          current: v.current,
          available: !!v.update,
          version: v.update?.version,
          notes: v.update?.notes ?? undefined,
          installable: v.installable !== false,
        };
      } else {
        const v = await new Promise<{ ok: boolean; available?: boolean; version?: string; notes?: string; current?: string; message?: string }>(
          (resolve) => {
            androidCheck = resolve;
            android()!.updateCheck!();
          },
        );
        if (!v.ok) throw new Error(v.message || 'update check failed');
        r = { current: v.current ?? state.current, available: !!v.available, version: v.version, notes: v.notes };
      }
      set({
        current: r.current,
        phase: r.available ? 'available' : 'latest',
        version: r.available ? r.version : undefined,
        notes: r.available ? r.notes : undefined,
        error: undefined,
        checkedAt: Date.now(),
        installable: r.installable !== false,
      });
    } catch (e) {
      if (silent) set({ phase: before === 'checking' ? 'idle' : before });
      else set({ phase: 'error', error: errText(e), checkedAt: Date.now() });
    } finally {
      checking = null;
    }
  })();
  return checking;
}

/** Download and install the version found by the last check. Desktop restarts when done; Android opens the system installer */
export async function installUpdate(): Promise<void> {
  // On Android "installing" only means the system installer was opened, so allow starting again
  if (!updateSupported || state.phase === 'downloading' || (platform === 'desktop' && state.phase === 'installing')) return;
  if (state.installable === false) {
    const { openUrl } = await import('./components/PortsView');
    openUrl(RELEASES_URL);
    return;
  }
  set({ phase: 'downloading', got: 0, total: null, error: undefined });
  if (platform === 'android') {
    android()!.updateDownload!();
    return;
  }
  const { invoke } = await import('@tauri-apps/api/core');
  const { listen } = await import('@tauri-apps/api/event');
  const off = await listen<{ got: number; total: number | null }>('update-progress', (e) => {
    const { got, total } = e.payload;
    set({ phase: total != null && got >= total ? 'installing' : 'downloading', got, total });
  });
  try {
    // Only returns on failure: on success the app restarts (on Windows the installer closes it first)
    await invoke('update_install');
  } catch (e) {
    set({ phase: 'error', error: errText(e) });
  } finally {
    off();
  }
}

// ---------- Automatic checks and the corner notice ----------

export const autoUpdateEnabled = () => lsGet(AUTO_KEY, true);

let timer: ReturnType<typeof setInterval> | null = null;

/** Called once at startup: read the version, then check after a short delay and every few hours (if enabled) */
export function startUpdateChecks() {
  if (!updateSupported) return;
  currentVersion()
    .then((v) => set({ current: v }))
    .catch(() => {});
  setTimeout(() => autoUpdateEnabled() && checkUpdate(true), FIRST_CHECK_MS);
  if (!timer) timer = setInterval(() => autoUpdateEnabled() && checkUpdate(true), CHECK_EVERY_MS);
}

export function setAutoUpdate(on: boolean) {
  lsSet(AUTO_KEY, on);
  if (on) checkUpdate(true);
}

/** "Later" on the corner notice: don't show it again for this version (the settings card still offers it) */
export function dismissUpdate() {
  if (state.version) lsSet(DISMISS_KEY, state.version);
  set({});
}

export const isDismissed = (version?: string) => !!version && lsGet<string>(DISMISS_KEY, '') === version;
