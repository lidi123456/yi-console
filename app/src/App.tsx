import { useEffect } from 'react';
import { FilesView } from './components/FilesView';
import { HistoryView } from './components/HistoryView';
import { Home } from './components/Home';
import { HostsView } from './components/HostsView';
import { Icon } from './components/Icon';
import { NewSessionPage } from './components/NewSession';
import { SessionView } from './components/SessionView';
import { SettingsView } from './components/SettingsView';
import { Sidebar } from './components/Sidebar';
import { StatsView } from './components/StatsView';
import { PortsView } from './components/PortsView';
import { PrefsView } from './components/PrefsView';
import { BotView } from './components/BotView';
import { PluginsView } from './components/PluginsView';
import { BotEventsView } from './components/BotActivity';
import { BotSource } from './bots';
import { Palette, useShortcuts } from './components/Palette';
import { SearchView } from './components/SearchView';
import { setState, useStore } from './store';
import { Overlays, useContextMenu } from './components/Overlay';
import { customFrame, TitleBar } from './components/TitleBar';
import { Lightbox } from './components/Lightbox';
import { UpgradeOverlay } from './components/Upgrade';
import { AppUpdateNotice } from './components/AppUpdate';
import { getT, useLang } from './i18n';

const t = getT('app');

function Toasts() {
  const toasts = useStore((s) => s.toasts);
  return (
    <div className="toasts">
      {toasts.map((t) => (
        <div key={t.id} className={`toast toast-${t.kind}`}>
          {t.kind === 'loading' && <span className="spinner" />}
          <span>{t.text}</span>
          {t.action && (
            <button
              className="link-btn"
              onClick={() => {
                t.action!.run();
                setState((s) => ({ toasts: s.toasts.filter((x) => x.id !== t.id) }));
              }}
            >
              {t.action.label}
            </button>
          )}
        </div>
      ))}
    </div>
  );
}

function Main() {
  const view = useStore((s) => s.view);
  switch (view.kind) {
    case 'session':
      return <SessionView key={view.hostId + view.sessionId} hostId={view.hostId} sessionId={view.sessionId} seq={view.seq} />;
    case 'hosts':
      return <HostsView editId={view.editId} />;
    case 'history':
      return <HistoryView key={view.hostId} hostId={view.hostId} cwd={view.cwd} />;
    case 'files':
      return <FilesView key={view.hostId} hostId={view.hostId} path={view.path} />;
    case 'ports':
      return <PortsView key={view.hostId} hostId={view.hostId} />;
    case 'stats':
      return <StatsView key={view.hostId} hostId={view.hostId} />;
    case 'settings':
      return <SettingsView key={view.hostId} hostId={view.hostId} />;
    case 'prefs':
      return <PrefsView />;
    case 'bot':
      return (
        <BotSource.Provider value={view.src ?? ''}>
          {/* An older saved view may be the removed new-Bot form */}
          {view.botId ? <BotView key={`${view.src ?? ''}/${view.botId}`} botId={view.botId} /> : <div className="page muted" />}
        </BotSource.Provider>
      );
    case 'botEvents':
      return (
        <BotSource.Provider value={view.src ?? ''}>
          <BotEventsView key={`${view.src ?? ''}/${view.botId}`} botId={view.botId} />
        </BotSource.Provider>
      );
    case 'plugins':
      return (
        <BotSource.Provider value={view.src ?? ''}>
          <PluginsView key={`${view.src ?? ''}/${view.review ?? ''}`} review={view.review} />
        </BotSource.Provider>
      );
    case 'new': {
      // Opened again with other parameters (another directory, a hand-off): start over
      const { kind: _k, ...p } = view;
      return <NewSessionPage key={JSON.stringify(p)} params={p} />;
    }
    case 'search':
      return <SearchView key={view.q ?? ''} initial={view.q} />;
    default:
      return <Home />;
  }
}

export function App() {
  useShortcuts();
  useContextMenu();
  useLang();
  const view = useStore((s) => s.view);
  const sideHidden = useStore((s) => s.sidebarHidden);
  const attn = useStore((s) =>
    Object.values(s.sessions).reduce((n, l) => n + l.filter((x) => x.pendingPermissions > 0).length, 0),
  );
  useEffect(() => {
    document.title = attn > 0 ? `(${attn}) Yi Console` : 'Yi Console';
  }, [attn]);
  return (
    <div className={`app ${sideHidden ? 'side-hidden' : ''} ${customFrame ? 'framed' : ''}`}>
      <TitleBar />
      <div className="app-body">
      <Sidebar />
      <main className="main">
        <div className="mobile-bar">
          <button className="icon-btn" onClick={() => setState({ sidebarOpen: true })} aria-label={t('menu')}>
            <Icon name="menu" />
            {attn > 0 && <span className="badge">{attn}</span>}
          </button>
          <span className="mobile-title">Yi Console</span>
        </div>
        <div className={`main-body view-${view.kind}`}>
          <Main />
        </div>
      </main>
      </div>
      <Palette />
      <Toasts />
      <Lightbox />
      <Overlays />
      <UpgradeOverlay />
      <AppUpdateNotice />
    </div>
  );
}
