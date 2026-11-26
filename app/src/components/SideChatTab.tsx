import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { useBack } from '../back';
import { getT, useLang } from '../i18n';
import { getActiveReviewId, setActiveReviewId } from '../reviewState';
import {
  bringSideChatToMain,
  deriveSideTitle,
  isSideChatBusy,
  sideChatMenuHint,
  sideChatNeedsDeleteConfirm,
  sideChatTitle,
  sideChatTools,
} from '../sideChat';
import { errorToast, loadOlder, loadSession, registerVisibleChildSession, skey, useSessionData, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type QuestionAnswer, type SessionInfo, type ToolKind } from '../types';
import { AsyncAskCard, ChatView, openAsyncAsk, PermissionCard } from './ChatView';
import { Composer, startReeditToComposer } from './Composer';
import { Icon, ToolBadge } from './Icon';
import { askConfirm, askText } from './Overlay';
import { deleteReviewMeta, getReviewMeta, ReviewSidePane } from './ReviewTab';
import { StatusLine } from './StatusLine';

export * from '../sideChat';

const tx = getT('filePanel');
const tsv = getT('sessionView');

/** Permanently delete a side chat (or review side chat) and clear any client-side review tracking attached to it */
export async function deleteSideChatSession(
  hostId: string,
  parentSessionId: string,
  info: SessionInfo,
): Promise<boolean> {
  try {
    await rpc(hostId, 'session.delete', { sessionId: info.id });
  } catch (err) {
    errorToast(err);
    return false;
  }
  if (info.review || getReviewMeta(info.id)) {
    deleteReviewMeta(info.id);
    if (getActiveReviewId(hostId, parentSessionId) === info.id) {
      setActiveReviewId(hostId, parentSessionId, null);
    }
  }
  return true;
}

export interface SideChatsMenuProps {
  hostId: string;
  parent: SessionInfo;
  sideChats: SessionInfo[];
  connected: boolean;
  anchor: { right: number; bottom: number };
  onClose: () => void;
  onOpenSideChat: (sc: SessionInfo) => void;
  onCreateSideChat: (tool?: ToolKind) => void;
}

/** Reactive dropdown menu for topbar "Side · N": supports opening, creating, and inline quick-deleting side chats */
export function SideChatsMenu({
  hostId,
  parent,
  sideChats,
  connected,
  anchor,
  onClose,
  onOpenSideChat,
  onCreateSideChat,
}: SideChatsMenuProps) {
  useLang();
  const menuRef = useRef<HTMLDivElement>(null);
  const [confirmId, setConfirmId] = useState<string | null>(null);
  const [deletingIds, setDeletingIds] = useState<string[]>([]);
  const [pos, setPos] = useState(() => ({
    left: Math.max(4, anchor.right - 340),
    top: anchor.bottom,
  }));

  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;
  const confirmIdRef = useRef(confirmId);
  confirmIdRef.current = confirmId;

  // Freeze row order while open: the store re-sorts sessions by updatedAt on every status
  // change, which would otherwise slide a different delete button under the pointer
  const orderRef = useRef<string[]>([]);
  const ordered = useMemo(() => {
    const byId = new Map(sideChats.map((s) => [s.id, s]));
    const ids = orderRef.current.filter((id) => byId.has(id));
    for (const s of sideChats) if (!ids.includes(s.id)) ids.push(s.id);
    orderRef.current = ids;
    return ids.map((id) => byId.get(id)!);
  }, [sideChats]);

  // Keep the right edge aligned with the trigger button while clamping inside the viewport
  useLayoutEffect(() => {
    const el = menuRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    setPos({
      left: Math.max(4, Math.min(anchor.right - r.width, window.innerWidth - r.width - 4)),
      top: Math.max(4, Math.min(anchor.bottom, window.innerHeight - r.height - 4)),
    });
  }, [anchor.right, anchor.bottom, sideChats.length, confirmId]);

  useEffect(() => {
    if (sideChats.length === 0) onCloseRef.current();
  }, [sideChats.length]);

  useEffect(() => {
    if (confirmId && !sideChats.some((s) => s.id === confirmId)) setConfirmId(null);
  }, [sideChats, confirmId]);

  // Auto-dismiss pending inline confirmation after inactivity
  useEffect(() => {
    if (!confirmId) return;
    const id = confirmId;
    const timer = window.setTimeout(() => {
      setConfirmId((cur) => (cur === id ? null : cur));
    }, 3500);
    return () => window.clearTimeout(timer);
  }, [confirmId]);

  // LIFO back-button order: cancel inline confirm first, then close the menu
  useBack(true, onClose);
  useBack(confirmId !== null, () => setConfirmId(null));

  // Keyboard access: focus the menu on open, restore the trigger on close
  useEffect(() => {
    const prev = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    menuRef.current?.querySelector<HTMLElement>('button:not(:disabled)')?.focus();
    return () => {
      if (prev?.isConnected) prev.focus();
    };
  }, []);

  // Entering inline confirm moves focus to its cancel button. Leaving only restores focus when
  // the confirm controls were focused and got removed (focus fell to body); if the user already
  // navigated elsewhere, leave their focus alone.
  const prevConfirmRef = useRef<string | null>(null);
  useEffect(() => {
    const prev = prevConfirmRef.current;
    prevConfirmRef.current = confirmId;
    if (confirmId) {
      menuRef.current?.querySelector<HTMLElement>('.scm-row.confirming .scm-cancel-btn')?.focus();
    } else if (prev && (!document.activeElement || document.activeElement === document.body)) {
      const rowDel = menuRef.current?.querySelector<HTMLElement>(
        `.scm-row[data-sid="${CSS.escape(prev)}"] .scm-del:not(:disabled)`,
      );
      (rowDel ?? menuRef.current?.querySelector<HTMLElement>('button:not(:disabled)'))?.focus();
    }
  }, [confirmId]);

  const onMenuKeyDown = (e: React.KeyboardEvent) => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    const btns = [...(menuRef.current?.querySelectorAll<HTMLElement>('button:not(:disabled)') ?? [])];
    if (!btns.length) return;
    e.preventDefault();
    const i = btns.indexOf(document.activeElement as HTMLElement);
    btns[e.key === 'ArrowDown' ? (i + 1) % btns.length : (i - 1 + btns.length) % btns.length]?.focus();
  };

  // A quick delete disables/removes the focused trash button without touching confirmId:
  // recover focus so arrow-key navigation keeps working across consecutive deletes
  useEffect(() => {
    if (document.activeElement && document.activeElement !== document.body) return;
    menuRef.current?.querySelector<HTMLElement>('button:not(:disabled)')?.focus();
  }, [deletingIds, sideChats]);

  useEffect(() => {
    const close = () => onCloseRef.current();
    const onWheel = (e: WheelEvent) => {
      if (menuRef.current && e.target instanceof Node && menuRef.current.contains(e.target)) return;
      close();
    };
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.isComposing || e.keyCode === 229) return;
      if (e.key !== 'Escape') return;
      e.preventDefault();
      e.stopImmediatePropagation();
      if (confirmIdRef.current) setConfirmId(null);
      else close();
    };
    window.addEventListener('blur', close);
    window.addEventListener('resize', close);
    window.addEventListener('keydown', onKeyDown, true);
    window.addEventListener('wheel', onWheel, { passive: true });
    return () => {
      window.removeEventListener('blur', close);
      window.removeEventListener('resize', close);
      window.removeEventListener('keydown', onKeyDown, true);
      window.removeEventListener('wheel', onWheel);
    };
  }, []);

  const performDelete = async (sc: SessionInfo, onlyIfUnused = false) => {
    if (!connected || deletingIds.includes(sc.id)) return;
    setDeletingIds((prev) => (prev.includes(sc.id) ? prev : [...prev, sc.id]));
    setConfirmId((cur) => (cur === sc.id ? null : cur));
    try {
      if (onlyIfUnused) {
        // The client snapshot can lag a just-sent message: the server's full index has the final say
        const used = await rpc<{ items?: unknown[] }>(hostId, 'session.userMessages', { sessionId: sc.id })
          .then((r) => (r.items ?? []).length > 0)
          .catch(() => false);
        if (used) return;
      }
      await deleteSideChatSession(hostId, parent.id, sc);
    } finally {
      setDeletingIds((prev) => prev.filter((id) => id !== sc.id));
    }
  };

  const onTrashClick = (sc: SessionInfo) => {
    if (!connected || deletingIds.includes(sc.id)) return;
    const isReview = !!(sc.review || getReviewMeta(sc.id));
    if (!sideChatNeedsDeleteConfirm(hostId, sc, isReview)) {
      void performDelete(sc, true);
    } else {
      setConfirmId(sc.id);
    }
  };

  const otherTools = sideChatTools(hostId).filter((tk) => tk !== parent.tool);

  return createPortal(
    <div
      className="ctx-scrim"
      onMouseDown={onClose}
      onContextMenu={(e) => {
        e.preventDefault();
        onClose();
      }}
    >
      <div
        ref={menuRef}
        className="ctx-menu side-chats-menu"
        role="menu"
        style={pos}
        onMouseDown={(e) => e.stopPropagation()}
        onKeyDown={onMenuKeyDown}
      >
        {ordered.map((sc) => {
          const label = sideChatTitle(sc);
          const hint = sideChatMenuHint(sc, label);
          const isReview = !!(sc.review || getReviewMeta(sc.id));
          const busy = isSideChatBusy(sc);
          const attn = sc.pendingPermissions > 0 || sc.status === 'waiting-permission';
          const running = sc.status === 'running' || sc.status === 'starting';
          const confirming = confirmId === sc.id;
          const deleting = deletingIds.includes(sc.id);
          return (
            <div
              key={sc.id}
              data-sid={sc.id}
              className={`scm-row ${confirming ? 'confirming' : ''} ${deleting ? 'deleting' : ''}`}
            >
              <button
                type="button"
                className="scm-main"
                role="menuitem"
                disabled={deleting}
                title={hint ? `${label}  ${hint}` : label}
                onMouseDown={(e) => e.preventDefault()}
                onClick={() => {
                  if (confirming) {
                    setConfirmId(null);
                    return;
                  }
                  onClose();
                  onOpenSideChat(sc);
                }}
              >
                <span className="ctx-icon">
                  <Icon name={isReview ? 'sparkles' : 'sideChat'} size={14} />
                </span>
                <span className="grow">{label}</span>
                {!confirming && hint && (
                  <span
                    className={`ctx-hint ${attn ? 'scm-hint-attn' : running ? 'scm-hint-running' : ''}`}
                  >
                    {(attn || running) && <span className={`scm-dot ${attn ? 'attn' : 'running'}`} />}
                    {hint}
                  </span>
                )}
              </button>
              {confirming ? (
                <div className="scm-confirm-group">
                  <button
                    type="button"
                    className="scm-confirm-btn"
                    disabled={!connected || deleting}
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => void performDelete(sc)}
                  >
                    {busy ? tsv('stopAndDeleteSideChat') : tsv('confirmDeleteSideChat')}
                  </button>
                  <button
                    type="button"
                    className="scm-cancel-btn"
                    title={tsv('cancelDeleteSideChat')}
                    aria-label={tsv('cancelDeleteSideChat')}
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => setConfirmId(null)}
                  >
                    <Icon name="x" size={11} />
                  </button>
                </div>
              ) : (
                <>
                  <span className="scm-del-sep" aria-hidden="true" />
                  <button
                    type="button"
                    className="scm-del"
                    disabled={!connected || deleting}
                    title={deleting ? tsv('deletingSideChat') : tsv('deleteSideChat')}
                    aria-label={tsv('deleteSideChat')}
                    onMouseDown={(e) => e.preventDefault()}
                    onClick={() => onTrashClick(sc)}
                  >
                    <Icon name="trash" size={13} />
                  </button>
                </>
              )}
            </div>
          );
        })}
        <div className="ctx-sep" />
        <button
          type="button"
          role="menuitem"
          disabled={!connected}
          onMouseDown={(e) => e.preventDefault()}
          onClick={() => {
            onClose();
            onCreateSideChat();
          }}
        >
          <span className="ctx-icon">
            <Icon name="plus" size={14} />
          </span>
          <span className="grow">{tsv('newSideChat')}</span>
          <span className="ctx-hint">{TOOL_LABEL[parent.tool]}</span>
        </button>
        {otherTools.map((tk) => (
          <button
            key={tk}
            type="button"
            role="menuitem"
            disabled={!connected}
            onMouseDown={(e) => e.preventDefault()}
            onClick={() => {
              onClose();
              onCreateSideChat(tk);
            }}
          >
            <span className="ctx-icon">
              <Icon name="sideChat" size={14} />
            </span>
            <span className="grow">{tsv('newSideChatWithTool', { tool: TOOL_LABEL[tk] })}</span>
          </button>
        ))}
      </div>
    </div>,
    document.body,
  );
}

export function SideChatPane({
  hostId,
  parentSessionId,
  sideSessionId,
  onOpenFile,
  onDeleted,
}: {
  hostId: string;
  parentSessionId: string;
  sideSessionId: string;
  onOpenFile?: (relPath: string, line?: number) => void;
  onDeleted: () => void;
}) {
  const info = useStore((s) => s.sessions[hostId]?.find((x) => x.id === sideSessionId));
  const isReviewSide = !!(info?.review || getReviewMeta(sideSessionId));

  if (isReviewSide) {
    return (
      <ReviewSidePane
        hostId={hostId}
        reviewSessionId={sideSessionId}
        parentSessionId={parentSessionId}
        onOpenFile={onOpenFile}
        onDeleted={onDeleted}
      />
    );
  }

  return (
    <RegularSideChatPane
      hostId={hostId}
      parentSessionId={parentSessionId}
      sideSessionId={sideSessionId}
      onDeleted={onDeleted}
    />
  );
}

function RegularSideChatPane({
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
  const hasInfo = !!info;
  const connected = useStore((s) => s.hosts.find((h) => h.config.id === hostId)?.status.status === 'connected');

  useEffect(() => {
    if (connected && hasInfo) loadSession(hostId, sideSessionId).catch(errorToast);
  }, [hostId, sideSessionId, connected, hasInfo, data.loaded]);

  useEffect(() => (hasInfo ? registerVisibleChildSession(hostId, sideSessionId) : undefined), [hostId, sideSessionId, hasInfo]);

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
    if (!(await deleteSideChatSession(hostId, parentSessionId, info))) return;
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
            onReedit={(msg) => startReeditToComposer(hostId, sideSessionId, msg)}
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
        <StatusLine hostId={hostId} info={info} events={data.events} compact />
      </div>
    </div>
  );
}
