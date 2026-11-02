import { useEffect, useState } from 'react';
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
import { Icon } from './Icon';
import { setTheme, useTheme, type Theme } from '../theme';
import { setLangPref, tr, useLang, type LangPref } from '../i18n';

/** 系统设置：这台设备自己的偏好（通知、网络代理），不跟主机走 */
export function PrefsView() {
  useLang();
  return (
    <div className="page">
      <div className="page-head">
        <h2>{tr('系统设置', 'Preferences')}</h2>
      </div>
      <p className="muted small">
        {tr(
          '这里的设置只对这台设备上的 Yi Console 生效。主机相关的设置（工具路径、手机推送、会话状态栏）在侧栏每台主机的「设置」里。',
          'These settings apply only to Yi Console on this device. Host-specific settings (tool paths, relay pairing, status line) are under each host\'s Settings in the sidebar.',
        )}
      </p>
      <ThemeCard />
      <NotifyCard />
      <ChatStyleCard />
      <HandoffCard />
      <TermCard />
      <ProxyCard />
      <AboutCard />
    </div>
  );
}

function ThemeCard() {
  const [t] = useTheme();
  const [lang] = useLang();
  return (
    <div className="card">
      <div className="card-title">
        <Icon name="eye" size={16} /> {tr('外观与语言', 'Appearance & Language')}
      </div>
      <PrefRow
        title={tr('界面语言', 'Language')}
        desc={tr('切换应用的显示语言', 'Choose the display language for the interface')}
      >
        <div className="seg">
          {(
            [
              ['auto', tr('跟随系统', 'System')],
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
        title={tr('界面主题', 'Theme')}
        desc={tr('跟随系统时，系统切换深色 / 浅色模式后这里自动跟着变', 'When set to System, automatically follows OS dark/light mode')}
      >
        <div className="seg">
          {(
            [
              ['auto', tr('跟随系统', 'System')],
              ['light', tr('亮色', 'Light')],
              ['dark', tr('暗色', 'Dark')],
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

function ChatStyleCard() {
  const v = useChatStyle();
  const [tail, setTail] = useState(historyTail);
  return (
    <div className="card">
      <div className="card-title">
        <Icon name="chat" size={16} /> {tr('对话外观', 'Chat Appearance')}
      </div>
      <PrefRow
        title={tr('AI 回复的显示风格', 'Assistant Reply Style')}
        desc={
          v === 'modern'
            ? tr(
                '简约：思考和工具调用收成一组，一行概括做了什么，点开看每一步',
                'Minimal: Groups consecutive thinking and tool calls into a compact summary',
              )
            : tr(
                'Claude Code：左侧一列圆点时间线，每次工具调用单独一行并带结果摘要',
                'Claude Code: Dot timeline on the left with each tool call and summary on its own line',
              )
        }
      >
        <div className="seg">
          {(
            [
              ['modern', tr('简约', 'Minimal')],
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
        title={tr('打开会话时加载的消息条数', 'Messages Loaded When Opening a Session')}
        desc={tr(
          '先取最近这么多条记录（含工具调用），往上翻再加载更早的。连远程主机较慢时调小。',
          'Load this many recent records (including tool calls) first; scroll up to load earlier ones. Lower it if remote hosts feel slow.',
        )}
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
              {tr(`${n} 条`, `${n}`)}
            </option>
          ))}
        </select>
      </PrefRow>
    </div>
  );
}

function TermCard() {
  const p = useTermPrefs();
  return (
    <div className="card">
      <div className="card-title">{tr('终端外观', 'Terminal Appearance')}</div>
      <p className="muted small">
        {tr(
          '终端模式会话的配色和字号（配色取自 Windows Terminal）。终端右上角的按钮也可以随时切换。',
          'Color scheme and font size for terminal sessions (themes from Windows Terminal). You can also toggle them from the terminal status bar.',
        )}
      </p>
      <div className="form">
        <div className="field">
          <span>{tr('配色', 'Color Scheme')}</span>
          <div className="seg">
            {(
              [
                ['auto', tr('跟随界面', 'Follow UI')],
                ['dark', tr('暗色', 'Dark')],
                ['light', tr('亮色', 'Light')],
              ] as [TermScheme, string][]
            ).map(([v, l]) => (
              <button key={v} className={p.scheme === v ? 'active' : ''} onClick={() => setTermPrefs({ scheme: v })}>
                {l}
              </button>
            ))}
          </div>
        </div>
        <label>
          {tr('字号', 'Font Size')}
          <select value={p.fontSize} onChange={(e) => setTermPrefs({ fontSize: Number(e.target.value) })}>
            {[11, 12, 13, 14, 15, 16, 18, 20].map((n) => (
              <option key={n} value={n}>
                {n}px
              </option>
            ))}
          </select>
        </label>
      </div>
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
      // 打开通知时顺便申请系统权限
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
        tr('系统没有允许 Yi Console 发通知，请在系统设置里打开', 'Notifications are not allowed for Yi Console. Please enable them in system settings.'),
      );
    await systemNotify(
      tr('Claude Code 回复完成：测试通知', 'Claude Code reply finished: Test notification'),
      tr('这是一条测试通知，能看到说明系统通知可以正常使用。', 'This is a test notification. System notifications are working properly.'),
    );
    toast(tr('已发送测试通知', 'Test notification sent'));
  };

  const off = !n.enabled;
  return (
    <div className="card">
      <div className="card-title">
        <Icon name="bell" size={16} /> {tr('通知', 'Notifications')}
      </div>
      <PrefRow
        title={tr('系统通知', 'System Notifications')}
        desc={tr('AI 回复完或者需要你审批时，在系统通知中心弹出提醒', 'Show system notifications when AI finishes replying or awaits approval')}
      >
        <Switch checked={n.enabled} onChange={(v) => set({ enabled: v })} />
      </PrefRow>
      <div className={`pref-group ${off ? 'disabled' : ''}`}>
        <PrefRow
          title={tr('AI 回复完成时', 'On Turn Completed')}
          desc={tr('通知里带上回复的开头一段', 'Include the beginning of the reply in the notification')}
        >
          <Switch checked={n.onTurnEnd} disabled={off} onChange={(v) => set({ onTurnEnd: v })} />
        </PrefRow>
        <PrefRow
          title={tr('等待审批时', 'On Approval Needed')}
          desc={tr('工具要执行命令、改文件，需要你确认', 'When a tool needs your confirmation to run commands or edit files')}
        >
          <Switch checked={n.onPermission} disabled={off} onChange={(v) => set({ onPermission: v })} />
        </PrefRow>
        <PrefRow title={tr('什么时候通知', 'When to Notify')}>
          <div className="seg">
            <button className={n.when === 'away' ? 'active' : ''} disabled={off} onClick={() => set({ when: 'away' })}>
              {tr('不在看这个会话时', 'When away from session')}
            </button>
            <button className={n.when === 'always' ? 'active' : ''} disabled={off} onClick={() => set({ when: 'always' })}>
              {tr('总是', 'Always')}
            </button>
          </div>
        </PrefRow>
      </div>
      <div className="row wrap">
        <button className="btn small" onClick={test}>
          {tr('发送测试通知', 'Send Test Notification')}
        </button>
        <span className="muted small">
          {perm === 'granted'
            ? tr('系统已允许通知', 'System notifications granted')
            : perm === 'denied'
              ? tr(
                  '系统没有允许通知：Windows 在「设置 → 系统 → 通知」，macOS 在「系统设置 → 通知」里打开 Yi Console',
                  'Notifications denied: Enable Yi Console in Windows (Settings → System → Notifications) or macOS (System Settings → Notifications)',
                )
              : perm === 'unsupported'
                ? tr('这个环境不支持系统通知', 'System notifications are not supported in this environment')
                : tr('首次发送时系统会询问是否允许', 'System will prompt for permission on first notification')}
        </span>
      </div>
      <p className="muted small">
        {tr(
          '提示音跟随系统的通知设置。「不在看这个会话时」包括：窗口最小化或在别的窗口后面、正在看别的会话。手机 App 退到后台后收不到实时消息，也就收不到通知。',
          'Notification sound follows OS settings. "When away from session" includes when the window is minimized, unfocused, or viewing another session. Mobile app cannot receive notifications while in background.',
        )}
        {!isTauri && tr(' 浏览器版需要页面开着才能收到。', ' Web version requires the tab to stay open.')}
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
      toast(tr('代理设置已保存，新建的连接生效', 'Proxy settings saved (applies to new connections)'));
    } catch (e) {
      errorToast(e);
    }
  };
  return (
    <div className="card">
      <div className="card-title">
        <Icon name="cloud" size={16} /> {tr('网络代理', 'Network Proxy')}
      </div>
      <p className="muted small">
        {tr(
          '连 SSH 主机和中继时使用。自动模式读取 HTTPS_PROXY / HTTP_PROXY / ALL_PROXY 环境变量，没有时用系统代理设置；内网地址和 NO_PROXY 里的主机直连。只支持 HTTP 代理。',
          'Used when connecting to SSH hosts and relays. Auto mode reads HTTPS_PROXY / HTTP_PROXY / ALL_PROXY env vars or system proxy; private IPs and NO_PROXY hosts connect directly. HTTP proxy only.',
        )}
      </p>
      <div className="seg">
        {[
          ['auto', tr('自动', 'Auto')],
          ['manual', tr('手动', 'Manual')],
          ['off', tr('不使用', 'Off')],
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
            {tr('保存', 'Save')}
          </button>
        </div>
      )}
      <p className="muted small">
        {tr('当前：', 'Current: ')}
        {detected ? tr(`经代理 ${detected}`, `Via proxy ${detected}`) : tr('直连', 'Direct connection')}
      </p>
    </div>
  );
}

function AboutCard() {
  const hosts = useStore((s) => s.hosts);
  const local = hosts.find((h) => h.config.kind === 'local');
  return (
    <div className="card">
      <div className="card-title">{tr('关于', 'About')}</div>
      <div className="small">
        Yi Console {local?.status.clientVersion ?? ''}
        {local?.status.hello && ` · ${tr('本机', 'Local')} yi-agentd ${local.status.hello.daemonVersion}`}
      </div>
    </div>
  );
}
