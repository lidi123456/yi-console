import { useEffect, useMemo } from 'react';
import { getT, useLang } from '../i18n';
import { bringSideChatToMain, deriveSideTitle, sideChatTitle } from '../sideChat';
import { errorToast, loadOlder, loadSession, registerVisibleChildSession, skey, useSessionData, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type QuestionAnswer } from '../types';
import { AsyncAskCard, ChatView, openAsyncAsk, PermissionCard } from './ChatView';
import { Composer } from './Composer';
import { Icon, ToolBadge } from './Icon';
import { askConfirm, askText } from './Overlay';

export * from '../sideChat';

const tx = getT('filePanel');

export function SideChatPane({
  hostId,
  parentSessionId,
  sideSessionId,
  onDeleted,
}: {
  hostId: string;
  parentSessionId: string;
  sideSessionId: string;
  onDeleted: () => void;
}) {
  useLang();
  const key = skey(hostId, sideSessionId);
  const data = useSessionData(key);
  const info = useStore((s) => s.sessions[hostId]?.find((x) => x.id === sideSessionId));
  const connected = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.status.status === 'connected');

  useEffect(() => {
    if (connected) loadSession(hostId, sideSessionId).catch(errorToast);
  }, [hostId, sideSessionId, connected, data.loaded]);

  useEffect(() => registerVisibleChildSession(hostId, sideSessionId), [hostId, sideSessionId]);

  const pending = useMemo(() => {
    const open = new Map<string, any>();
    for (const e of data.events) {
      if (e.kind === 'permission_request') open.set(e.requestId, e);
      if (e.kind === 'permission_resolved') open.delete(e.requestId);
    }
    return [...open.values()];
  }, [data.events, data.events.length]);

  const openAsk = useMemo(() => openAsyncAsk(data.events), [data.events, data.events.length]);
  const busy = info?.status === 'running' || info?.status === 'starting';
  const hasMessages = useMemo(
    () => data.events.some((e) => e.kind === 'user_message' || e.kind === 'assistant_text'),
    [data.events, data.events.length],
  );

  const answer = (requestId: string, optionId: string, answers?: QuestionAnswer[]) => {
    rpc(hostId, 'session.permission', {
      sessionId: sideSessionId,
      requestId,
      optionId,
      answers,
    }).catch(errorToast);
  };

  const sendAskReply = async (raw: string) => {
    const body = raw.trim();
    if (!body || !connected || !info) return;
    try {
      if (!info.title?.trim()) {
        const autoTitle = deriveSideTitle(body);
        if (autoTitle) rpc(hostId, 'session.rename', { sessionId: sideSessionId, title: autoTitle }).catch(() => {});
      }
      await rpc(hostId, 'session.send', { sessionId: sideSessionId, text: body });
    } catch (err) {
      errorToast(err);
    }
  };

  const stop = () => rpc(hostId, 'session.interrupt', { sessionId: sideSessionId }).catch(errorToast);

  const rename = async () => {
    if (!info) return;
    const next = await askText({ title: tx('sideChatRenameTitle'), value: info.title || '' });
    if (next == null) return;
    rpc(hostId, 'session.rename', { sessionId: sideSessionId, title: next.trim() }).catch(errorToast);
  };

  const remove = async () => {
    if (!info) {
      onDeleted();
      return;
    }
    const used = hasMessages || info.lastSeq > 0;
    if (used) {
      const ok = await askConfirm({
        title: tx('sideChatDelete'),
        message: tx('sideChatDeleteConfirm', { title: sideChatTitle(info) }),
        danger: true,
      });
      if (!ok) return;
    }
    try {
      await rpc(hostId, 'session.delete', { sessionId: sideSessionId });
    } catch (err) {
      errorToast(err);
      return;
    }
    onDeleted();
  };

  if (!info) {
    return (
      <div className="wp-side-pane">
        <div className="wp-side-empty">
          <Icon name="sideChat" size={22} />
          <div className="wp-side-empty-title">{tx('sideChatUnavailable')}</div>
          <button type="button" className="btn small" onClick={onDeleted}>
            {tx('close')}
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="wp-side-pane">
      <div className="wp-head wp-side-chat-head">
        <div className="wp-side-head-left">
          <ToolBadge tool={info.tool} size={15} />
          <button
            type="button"
            className="wp-side-title truncate"
            title={tx('sideChatRenameHint')}
            onDoubleClick={rename}
            onClick={rename}
          >
            {sideChatTitle(info)}
          </button>
          <span className="wp-side-sub muted hide-mobile">· {tx('sideChatHiddenBadge')}</span>
        </div>
        <div className="wp-side-head-acts">
          <button
            type="button"
            className="icon-btn"
            title={tx('sideChatDelete')}
            aria-label={tx('sideChatDelete')}
            onClick={remove}
          >
            <Icon name="trash" size={14} />
          </button>
        </div>
      </div>

      <div className="wp-side-body">
        {!hasMessages && !busy ? (
          <div className="wp-side-empty">
            <div className="wp-side-empty-ico">
              <Icon name="sideChat" size={20} />
            </div>
            <div className="wp-side-empty-title">{tx('sideChatEmptyTitle')}</div>
            <p className="wp-side-empty-desc">{tx('sideChatEmptyDesc', { tool: TOOL_LABEL[info.tool] })}</p>
          </div>
        ) : (
          <ChatView
            events={data.events}
            live={data.live}
            liveThinking={data.liveThinking}
            info={info}
            hostId={hostId}
            onAnswer={answer}
            hasMore={data.hasMore}
            loadingOlder={data.loadingOlder}
            loading={data.loading}
            onLoadOlder={(targetSeq) => loadOlder(hostId, sideSessionId, targetSeq).catch(errorToast)}
            pendingOutside
            onBringBack={(replyText) => bringSideChatToMain(hostId, parentSessionId, info, replyText)}
          />
        )}
      </div>

      {(pending.length > 0 || openAsk) && (
        <div className="wp-side-perms">
          {pending.map((p) => (
            <PermissionCard key={p.requestId} it={p} onAnswer={answer} compact />
          ))}
          {openAsk && (
            <AsyncAskCard key={openAsk.id} ask={openAsk} tool={info.tool} compact onSend={sendAskReply} onSkip={() => {}} />
          )}
        </div>
      )}

      <div className="composer-wrap wp-side-composer">
        <Composer
          hostId={hostId}
          info={info}
          connected={connected}
          onInterrupt={stop}
          onRename={rename}
          placeholder={tx('sideChatPlaceholder')}
          compact
        />
      </div>
    </div>
  );
}
