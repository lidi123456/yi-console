import { useEffect, useRef, useState } from 'react';
import {
  DEFAULT_NOTIFY,
  errorToast,
  notifyPermission,
  saveClientSettings,
  systemNotify,
  toast,
  useStore,
} from '../store';
import { isTauri } from '../transport';
import type { NotifyPrefs } from '../types';
import { setTermPrefs, useTermPrefs, type TermScheme } from './TerminalView';
import { setChatStyle, useChatStyle, type ChatStyle } from './ChatView';
import { historyTail, setHistoryTail, TAIL_OPTIONS } from '../store';
import { HandoffCard } from './HandoffCard';
import { UpdateSection } from './AppUpdate';
import { setSidePrefs, STALE_OPTIONS, useSidePrefs, type SideLayout } from './Sidebar';
import { Icon } from './Icon';
import { setTheme, useTheme, type Theme } from '../theme';
import { canZoom, CHAT_SIZES, EDITOR_SIZES, setFontPrefs, useFontPrefs, zoomFactor, ZOOM_MAX, ZOOM_MIN } from '../fontPrefs';
import { getT, setLangPref, type LangPref, useLang } from '../i18n';

const tx = getT('prefsView');

type Section = { id: string; title: () => string; body: () => React.ReactNode };

// Like VS Code's settings editor: every section on one scrolling page, with a table of contents on the left that
// jumps to a section and highlights the one being read. The contents are hidden on narrow screens.
const SECTIONS: Section[] = [
  {
    id: 'appearance',
    title: () => tx('catAppearance'),
    body: () => (
      <>
        <ThemeCard />
        <FontCard />
        <ChatStyleCard />
        <SidebarCard />
      </>
    ),
  },
  { id: 'terminal', title: () => tx('catTerminal'), body: () => <TermCard /> },
  { id: 'sessions', title: () => tx('catSessions'), body: () => <HandoffCard /> },
  { id: 'notifications', title: () => tx('notifications'), body: () => <NotifyCard /> },
  { id: 'network', title: () => tx('catNetwork'), body: () => <ProxyCard /> },
  { id: 'about', title: () => tx('about'), body: () => <AboutCard /> },
];

/** System settings: this device's own preferences (notifications, network proxy), not tied to a host */
export function PrefsView() {
  useLang();
  const page = useRef<HTMLDivElement>(null);
  const [active, setActive] = useState(SECTIONS[0].id);
  // After a click, keep the clicked entry highlighted while the page scrolls there: a short last section can't
  // reach the top, so the scroll position alone would pick the one above it
  const pinned = useRef(0);

  useEffect(() => {
    const el = page.current;
    if (!el) return;
    const onScroll = () => {
      if (Date.now() < pinned.current) return;
      const top = el.getBoundingClientRect().top + 48;
      let cur = SECTIONS[0].id;
      for (const s of SECTIONS) {
        const sec = el.querySelector<HTMLElement>(`[data-sec="${s.id}"]`);
        if (sec && sec.getBoundingClientRect().top <= top) cur = s.id;
      }
      if (el.scrollTop + el.clientHeight >= el.scrollHeight - 2) cur = SECTIONS[SECTIONS.length - 1].id;
      setActive(cur);
    };
    el.addEventListener('scroll', onScroll, { passive: true });
    return () => el.removeEventListener('scroll', onScroll);
  }, []);

  const jump = (id: string) => {
    setActive(id);
    pinned.current = Date.now() + 800;
    page.current?.querySelector(`[data-sec="${id}"]`)?.scrollIntoView({ block: 'start', behavior: 'smooth' });
  };

  return (
    <div className="page prefs" ref={page}>
      <div className="page-head">
        <h2>{tx('preferences')}</h2>
      </div>
      <p className="muted small">{tx('theseSettingsApplyOnlyTo')}</p>
      <div className="prefs-body">
        <nav className="prefs-toc">
          {SECTIONS.map((s) => (
            <button key={s.id} className={active === s.id ? 'active' : ''} onClick={() => jump(s.id)}>
              {s.title()}
            </button>
          ))}
        </nav>
        <div className="prefs-main">
          {SECTIONS.map((s) => (
            <section key={s.id} data-sec={s.id}>
              <h3 className="prefs-sec-title">{s.title()}</h3>
              {s.body()}
            </section>
          ))}
        </div>
      </div>
    </div>
  );
}

function ThemeCard() {
  const [t] = useTheme();
  const [lang] = useLang();
  return (
    <div className="card">
      <div className="card-title">
        <Icon name="eye" size={16} /> {tx('appearanceLanguage')}
      </div>
      <PrefRow
        title={tx('language')}
        desc={tx('chooseTheDisplayLanguageFor')}
      >
        <div className="seg">
          {(
            [
              ['auto', tx('system')],
              ['zh', '简体中文'],
              ['en', 'English'],
            ] as [LangPref, string][]
          ).map(([k, l]) => (
            <button key={k} className={lang === k ? 'active' : ''} onClick={() => setLangPref(k)}>
              {l}
            </button>
          ))}
        </div>
      </PrefRow>
      <PrefRow
        title={tx('theme')}
        desc={tx('whenSetToSystemAutomatically')}
      >
        <div className="seg">
          {(
            [
              ['auto', tx('system')],
              ['light', tx('light')],
              ['dark', tx('dark')],
            ] as [Theme, string][]
          ).map(([k, l]) => (
            <button key={k} className={t === k ? 'active' : ''} onClick={() => setTheme(k)}>
              {l}
            </button>
          ))}
        </div>
      </PrefRow>
    </div>
  );
}

function FontCard() {
  const p = useFontPrefs();
  const mod = /Mac|iPhone|iPad/.test(navigator.platform) ? '⌘' : 'Ctrl';
  const sizes = (list: number[], def: string) => [
    <option key={0} value={0}>
      {tx('defaultSize', { size: def })}
    </option>,
    ...list.map((n) => (
      <option key={n} value={n}>
        {`${n}px`}
      </option>
    )),
  ];
  return (
    <div className="card">
      <div className="card-title">
        <Icon name="text" size={16} /> {tx('fontAndZoom')}
      </div>
      <PrefRow title={tx('chatFontSize')} desc={tx('chatFontSizeDesc')}>
        <select value={p.chat} onChange={(e) => setFontPrefs({ chat: Number(e.target.value) })}>
          {sizes(CHAT_SIZES, '14px')}
        </select>
      </PrefRow>
      <PrefRow title={tx('chatFont')} desc={tx('chatFontDesc')}>
        <FontInput value={p.chatFont} example="LXGW WenKai" onSave={(chatFont) => setFontPrefs({ chatFont })} />
      </PrefRow>
      <PrefRow title={tx('editorFontSize')} desc={tx('editorFontSizeDesc')}>
        <select value={p.editor} onChange={(e) => setFontPrefs({ editor: Number(e.target.value) })}>
          {sizes(EDITOR_SIZES, '12px')}
        </select>
      </PrefRow>
      <PrefRow title={tx('editorFont')} desc={tx('editorFontDesc')}>
        <FontInput value={p.editorFont} example="JetBrains Mono" onSave={(editorFont) => setFontPrefs({ editorFont })} />
      </PrefRow>
      {canZoom && (
        <PrefRow title={tx('uiZoom')} desc={tx('uiZoomDesc', { mod })}>
          <div className="seg">
            <button disabled={p.zoom <= ZOOM_MIN} onClick={() => setFontPrefs({ zoom: p.zoom - 1 })} title={`${mod} -`}>
              −
            </button>
            <button onClick={() => setFontPrefs({ zoom: 0 })} title={`${mod} 0`}>
              {Math.round(zoomFactor(p.zoom) * 100)}%
            </button>
            <button disabled={p.zoom >= ZOOM_MAX} onClick={() => setFontPrefs({ zoom: p.zoom + 1 })} title={`${mod} =`}>
              +
            </button>
          </div>
        </PrefRow>
      )}
    </div>
  );
}

/** Comma-separated font names, saved on blur or Enter so the terminal doesn't re-measure on every keystroke */
function FontInput({ value, example, onSave }: { value: string; example: string; onSave: (v: string) => void }) {
  const [v, setV] = useState(value);
  useEffect(() => setV(value), [value]);
  const save = () => {
    const next = v.trim();
    if (next !== value) onSave(next);
  };
  return (
    <input
      className="font-input"
      value={v}
      spellCheck={false}
      placeholder={tx('fontPlaceholder', { example })}
      onChange={(e) => setV(e.target.value)}
      onBlur={save}
      onKeyDown={(e) => e.key === 'Enter' && (e.currentTarget as HTMLInputElement).blur()}
    />
  );
}

function ChatStyleCard() {
  const v = useChatStyle();
  const [tail, setTail] = useState(historyTail);
  return (
    <div className="card">
      <div className="card-title">
        <Icon name="chat" size={16} /> {tx('chatAppearance')}
      </div>
      <PrefRow
        title={tx('assistantReplyStyle')}
        desc={
          v === 'modern'
            ? tx('minimalGroupsConsecutiveThinkingAnd')
            : tx('claudeCodeDotTimelineOn')
        }
      >
        <div className="seg">
          {(
            [
              ['modern', tx('minimal')],
              ['claude', 'Claude Code'],
            ] as [ChatStyle, string][]
          ).map(([k, l]) => (
            <button key={k} className={v === k ? 'active' : ''} onClick={() => setChatStyle(k)}>
              {l}
            </button>
          ))}
        </div>
      </PrefRow>
      <PrefRow
        title={tx('messagesLoadedWhenOpeningA')}
        desc={tx('loadThisManyRecentRecords')}
      >
        <select
          value={tail}
          onChange={(e) => {
            const n = Number(e.target.value);
            setHistoryTail(n);
            setTail(n);
          }}
        >
          {TAIL_OPTIONS.map((n) => (
            <option key={n} value={n}>
              {tx('text', { n })}
            </option>
          ))}
        </select>
      </PrefRow>
    </div>
  );
}

function SidebarCard() {
  const p = useSidePrefs();
  return (
    <div className="card">
      <div className="card-title">
        <Icon name="folder" size={16} /> {tx('sidebarSessions')}
      </div>
      <PrefRow
        title={tx('layout')}
        desc={
          p.layout === 'project'
            ? tx('groupedByWorkingDirectoryMost')
            : tx('oneListSortedByRecent')
        }
      >
        <div className="seg">
          {(
            [
              ['project', tx('byDirectory')],
              ['time', tx('byTime')],
            ] as [SideLayout, string][]
          ).map(([k, l]) => (
            <button key={k} className={p.layout === k ? 'active' : ''} onClick={() => setSidePrefs({ layout: k })}>
              {l}
            </button>
          ))}
        </div>
      </PrefRow>
      <PrefRow
        title={tx('foldIdleSessions')}
        desc={tx('endedSessionsIdleThisLong')}
      >
        <select value={p.staleDays} onChange={(e) => setSidePrefs({ staleDays: Number(e.target.value) })}>
          {STALE_OPTIONS.map((n) => (
            <option key={n} value={n}>
              {n === 0 ? tx('never') : tx('dayCount', { count: n })}
            </option>
          ))}
        </select>
      </PrefRow>
    </div>
  );
}

function TermCard() {
  const p = useTermPrefs();
  const f = useFontPrefs();
  return (
    <div className="card">
      <div className="card-title">{tx('terminalAppearance')}</div>
      <p className="muted small">
        {tx('colorSchemeAndFontSize')}
      </p>
      <PrefRow title={tx('colorScheme')}>
        <div className="seg">
          {(
            [
              ['auto', tx('followUi')],
              ['dark', tx('dark')],
              ['light', tx('light')],
            ] as [TermScheme, string][]
          ).map(([v, l]) => (
            <button key={v} className={p.scheme === v ? 'active' : ''} onClick={() => setTermPrefs({ scheme: v })}>
              {l}
            </button>
          ))}
        </div>
      </PrefRow>
      <PrefRow title={tx('fontSize')}>
        <select value={p.fontSize} onChange={(e) => setTermPrefs({ fontSize: Number(e.target.value) })}>
          {[11, 12, 13, 14, 15, 16, 18, 20].map((n) => (
            <option key={n} value={n}>
              {`${n}px`}
            </option>
          ))}
        </select>
      </PrefRow>
      <PrefRow title={tx('terminalFont')} desc={tx('terminalFontDesc')}>
        <FontInput value={f.termFont} example="Sarasa Term SC" onSave={(termFont) => setFontPrefs({ termFont })} />
      </PrefRow>
    </div>
  );
}

function Switch({ checked, onChange, disabled }: { checked: boolean; onChange: (v: boolean) => void; disabled?: boolean }) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      disabled={disabled}
      className={`switch ${checked ? 'on' : ''}`}
      onClick={() => onChange(!checked)}
    >
      <i />
    </button>
  );
}

function PrefRow({ title, desc, children }: { title: string; desc?: string; children: React.ReactNode }) {
  return (
    <div className="pref-row">
      <div className="grow min0">
        <div>{title}</div>
        {desc && <div className="muted small">{desc}</div>}
      </div>
      {children}
    </div>
  );
}

function NotifyCard() {
  const client = useStore((s) => s.client);
  const n = client?.notify ?? DEFAULT_NOTIFY;
  const [perm, setPerm] = useState<string>('');

  useEffect(() => {
    notifyPermission(false).then(setPerm);
  }, []);

  const set = async (p: Partial<NotifyPrefs>) => {
    try {
      const next = { ...n, ...p };
      // Request the system permission when notifications are turned on
      if (next.enabled && (p.enabled || p.onTurnEnd || p.onPermission)) setPerm(await notifyPermission(true));
      await saveClientSettings({ notify: next });
    } catch (e) {
      errorToast(e);
    }
  };

  const test = async () => {
    const r = await notifyPermission(true);
    setPerm(r);
    if (r !== 'granted')
      return errorToast(
        tx('notificationsAreNotAllowedFor'),
      );
    await systemNotify(
      tx('claudeCodeReplyFinishedTest'),
      tx('thisIsATestNotification'),
    );
    toast(tx('testNotificationSent'));
  };

  const off = !n.enabled;
  return (
    <div className="card">
      <PrefRow
        title={tx('systemNotifications')}
        desc={tx('showSystemNotificationsWhenAi')}
      >
        <Switch checked={n.enabled} onChange={(v) => set({ enabled: v })} />
      </PrefRow>
      <div className={`pref-group ${off ? 'disabled' : ''}`}>
        <PrefRow
          title={tx('onTurnCompleted')}
          desc={tx('includeTheBeginningOfThe')}
        >
          <Switch checked={n.onTurnEnd} disabled={off} onChange={(v) => set({ onTurnEnd: v })} />
        </PrefRow>
        <PrefRow
          title={tx('onApprovalNeeded')}
          desc={tx('whenAToolNeedsYour')}
        >
          <Switch checked={n.onPermission} disabled={off} onChange={(v) => set({ onPermission: v })} />
        </PrefRow>
        <PrefRow title={tx('whenToNotify')}>
          <div className="seg">
            <button className={n.when === 'away' ? 'active' : ''} disabled={off} onClick={() => set({ when: 'away' })}>
              {tx('whenAwayFromSession')}
            </button>
            <button className={n.when === 'always' ? 'active' : ''} disabled={off} onClick={() => set({ when: 'always' })}>
              {tx('always')}
            </button>
          </div>
        </PrefRow>
      </div>
      <div className="row wrap">
        <button className="btn small" onClick={test}>
          {tx('sendTestNotification')}
        </button>
        <span className="muted small">
          {perm === 'granted'
            ? tx('systemNotificationsGranted')
            : perm === 'denied'
              ? tx('notificationsDeniedEnableYiConsole')
              : perm === 'unsupported'
                ? tx('systemNotificationsAreNotSupported')
                : tx('systemWillPromptForPermission')}
        </span>
      </div>
      <p className="muted small">
        {tx('notificationSoundFollowsOsSettings')}
        {!isTauri && tx('webVersionRequiresTheTab')}
      </p>
    </div>
  );
}

function ProxyCard() {
  const client = useStore((s) => s.client);
  const detected = useStore((s) => s.detectedProxy);
  const [mode, setMode] = useState('auto');
  const [url, setUrl] = useState('');
  useEffect(() => {
    if (!client) return;
    setMode(client.proxy.mode || 'auto');
    setUrl(client.proxy.url ?? '');
  }, [client]);
  const save = async (m: string) => {
    try {
      await saveClientSettings({ proxy: { mode: m, url } });
      toast(tx('proxySettingsSavedAppliesTo'));
    } catch (e) {
      errorToast(e);
    }
  };
  return (
    <div className="card">
      <div className="card-title">
        <Icon name="cloud" size={16} /> {tx('networkProxy')}
      </div>
      <p className="muted small">
        {tx('usedWhenConnectingToSsh')}
      </p>
      <div className="seg">
        {[
          ['auto', tx('auto')],
          ['manual', tx('manual')],
          ['off', tx('off')],
        ].map(([v, label]) => (
          <button key={v} className={mode === v ? 'active' : ''} onClick={() => (v === 'manual' ? setMode(v) : save(v))}>
            {label}
          </button>
        ))}
      </div>
      {mode === 'manual' && (
        <div className="row">
          <input className="grow" value={url} onChange={(e) => setUrl(e.target.value)} placeholder="http://127.0.0.1:7897" />
          <button className="btn primary small" disabled={!url} onClick={() => save('manual')}>
            {tx('save')}
          </button>
        </div>
      )}
      <p className="muted small">
        {tx('current')}
        {detected ? tx('viaProxy', { detected }) : tx('directConnection')}
      </p>
    </div>
  );
}

function AboutCard() {
  const hosts = useStore((s) => s.hosts);
  const local = hosts.find((h) => h.config.kind === 'local');
  return (
    <div className="card">
      <div className="small">
        Yi Console {local?.status.clientVersion ?? ''}
        {local?.status.hello && ` · ${tx('local')} yi-agentd ${local.status.hello.daemonVersion}`}
      </div>
      <UpdateSection Switch={Switch} />
    </div>
  );
}
