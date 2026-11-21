import { useEffect, useMemo, useRef, useState } from 'react';
import { addContextRefToChat, type ContextRefInput } from '../contextRefs';
import { getT, useLang } from '../i18n';
import { permModes } from '../perm';
import { errorToast, getSession, hostStatus, loadOlder, loadSession, skey, toast, useSessionData, useStore } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type HubEvent, type QuestionAnswer, type SessionInfo, type ToolKind } from '../types';
import { AsyncAskCard, ChatView, openAsyncAsk, PermissionCard } from './ChatView';
import { Icon, ToolBadge } from './Icon';
import { ModelPicker } from './ModelPicker';
import { askConfirm, askText } from './Overlay';
import { PermPicker } from './PermPicker';
import { openFileInPanel } from './SessionActions';

const tx = getT('filePanel');

export const SIDE_PREFIX = 'side:';
export const isSideTab = (t: string) => t.startsWith(SIDE_PREFIX);
export const sideTabSession = (t: string) => t.slice(SIDE_PREFIX.length);
export const sideTabPath = (sid: string) => SIDE_PREFIX + sid;

/** Read-only default permission mode for a side chat (`plan` where supported, else `default`) */
export function defaultSidePerm(tool: ToolKind): string {
  return permModes(tool, 'structured').some((m) => m[0] === 'plan') ? 'plan' : 'default';
}

/** Short title derived from the first question sent into a side chat */
export function deriveSideTitle(prompt: string): string {
  const line = prompt.trim().replace(/\s+/g, ' ');
  if (!line) return '';
  return [...line].slice(0, 28).join('') + ([...line].length > 28 ? '…' : '');
}

/** Display label for a side chat tab or list item */
export function sideChatTitle(info?: Pick<SessionInfo, 'title'> | null): string {
  const title = info?.title?.trim();
  return title || tx('sideChatDefaultTitle');
}

/** Installed AI chat tools on a host that can back a side chat */
export function sideChatTools(hostId: string): ToolKind[] {
  return (hostStatus(hostId).hello?.tools ?? [])
    .filter((t) => t.available && t.kind !== 'shell' && t.kind !== 'files')
    .map((t) => t.kind);
}

/** Create a hidden side chat attached to `parent` and open it as a tab in the right workspace panel */
export async function startSideChat(opts: {
  hostId: string;
  parent: SessionInfo;
  prompt?: string;
  tool?: ToolKind;
}): Promise<SessionInfo | null> {
  const { hostId, parent } = opts;
  const prompt = opts.prompt?.trim() ?? '';
  const tool = opts.tool ?? (parent.tool !== 'shell' && parent.tool !== 'files' ? parent.tool : sideChatTools(hostId)[0]);
  if (!tool) {
    toast(tx('sideChatNoToolAvailable'), 'error');
    return null;
  }
  const sameTool = tool === parent.tool;
  const title = prompt ? deriveSideTitle(prompt) : '';
  const created = await rpc<SessionInfo>(hostId, 'session.create', {
    tool,
    mode: 'structured',
    cwd: parent.cwd,
    parentId: parent.id,
    sideChat: true,
    permissionMode: defaultSidePerm(tool),
    ...(sameTool && parent.model ? { model: parent.model } : {}),
    ...(sameTool && parent.effort ? { effort: parent.effort } : {}),
    ...(sameTool && parent.accountId ? { accountId: parent.accountId } : {}),
    ...(title ? { title } : {}),
    ...(prompt ? { prompt } : {}),
  });
  openFileInPanel(hostId, parent.id, sideTabPath(created.id));
  return created;
}

/** Whether a side chat has ever had a user message (used to auto-delete unused side chats on tab close) */
export function sideChatHasMessages(hostId: string, sideSid: string, info?: SessionInfo): boolean {
  const data = getSession(skey(hostId, sideSid));
  if (data.events.some((e) => e.kind === 'user_message' || e.kind === 'assistant_text')) return true;
  if (info && (info.lastSeq > 0 || info.turn != null || info.status === 'running')) return true;
  return false;
}

/** Extract a concise 1-line summary from markdown text for the context pill */
function summarizeOneLine(text: string): string {
  for (const raw of text.split(/\r?\n/)) {
    const clean = raw
      .replace(/^#+\s*/, '')
      .replace(/^[-*+]\s+/, '')
      .replace(/[`*_~]/g, '')
      .trim();
    if (clean.length >= 4) {
      return [...clean].slice(0, 44).join('') + ([...clean].length > 44 ? '…' : '');
    }
  }
  return '';
}

/** Extract the latest assistant answer (or a compact Q&A transcript) from a side chat's events */
export function extractSideChatNotes(events: readonly HubEvent[], live?: Map<string, string>, specificText?: string): string {
  if (specificText && specificText.trim()) return specificText.trim();
  const streaming = live ? [...live.values()].join('\n').trim() : '';
  if (streaming) return streaming;
  for (let i = events.length - 1; i >= 0; i--) {
    const e = events[i];
    if (e.kind === 'assistant_text' && e.text.trim()) return e.text.trim();
  }
  const parts: string[] = [];
  for (const e of events) {
    if (e.kind === 'user_message' && e.text.trim()) parts.push(`Q: ${e.text.trim()}`);
  }
  return parts.join('\n\n');
}

/** Build a ContextRefInput carrying a side chat's answer back to the parent composer */
export function buildSideChatContextRef(
  sideInfo: Pick<SessionInfo, 'id' | 'title'>,
  content: string,
): ContextRefInput | null {
  const clean = content.trim();
  if (!clean) return null;
  const lines = clean.split('\n').length;
  return {
    kind: 'side',
    path: sideChatTitle(sideInfo),
    fromLine: 1,
    toLine: Math.max(1, lines),
    comment: summarizeOneLine(clean) || undefined,
    sideSessionId: sideInfo.id,
    content: clean,
  };
}

/** Load (if needed) and bring a side chat's conclusion into the parent session's composer as a ContextRef pill */
export async function bringSideChatToMain(
  hostId: string,
  parentSessionId: string,
  sideInfo: Pick<SessionInfo, 'id' | 'title'>,
  specificText?: string,
): Promise<boolean> {
  let content = specificText?.trim() ?? '';
  if (!content) {
    const k = skey(hostId, sideInfo.id);
    if (!getSession(k).loaded) {
      await loadSession(hostId, sideInfo.id).catch(() => {});
    }
    const d = getSession(k);
    content = extractSideChatNotes(d.events, d.live);
  }
  const ref = buildSideChatContextRef(sideInfo, content);
  if (!ref) {
    toast(tx('sideChatNothingToAdd'), 'info');
    return false;
  }
  addContextRefToChat(parentSessionId, ref, { focus: true });
  toast(tx('sideChatAddedToMain'));
  return true;
}

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

  const [text, setText] = useState('');
  const [sending, setSending] = useState(false);
  const [permOpen, setPermOpen] = useState(false);
  const taRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (connected) loadSession(hostId, sideSessionId).catch(errorToast);
  }, [hostId, sideSessionId, connected, data.loaded]);

  // Auto-size textarea up to 120px
  useEffect(() => {
    const el = taRef.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = `${Math.min(120, Math.max(36, el.scrollHeight))}px`;
  }, [text]);

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
  const hasNotes = useMemo(
    () => extractSideChatNotes(data.events, data.live).length > 0,
    [data.events, data.live, data.version],
  );
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

  const sendText = async (raw: string) => {
    const body = raw.trim();
    if (!body || !connected || !info || sending) return;
    setSending(true);
    try {
      // Name an untitled side chat after its first question so its tab stays readable
      if (!info.title?.trim()) {
        const autoTitle = deriveSideTitle(body);
        if (autoTitle) {
          rpc(hostId, 'session.rename', { sessionId: sideSessionId, title: autoTitle }).catch(() => {});
        }
      }
      await rpc(hostId, 'session.send', { sessionId: sideSessionId, text: body });
      setText('');
    } catch (err) {
      errorToast(err);
    } finally {
      setSending(false);
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

  const setPermMode = (mode: string) => {
    if (!info) return;
    rpc(hostId, 'session.setPermissionMode', { sessionId: sideSessionId, mode }).catch(errorToast);
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
      <div className="wp-head wp-side-head">
        <div className="wp-side-head-left">
          <ToolBadge tool={info.tool} size={15} />
          <button type="button" className="wp-side-title truncate" title={tx('sideChatRenameHint')} onDoubleClick={rename} onClick={rename}>
            {sideChatTitle(info)}
          </button>
          <span className="wp-side-sub muted hide-mobile">· {tx('sideChatHiddenBadge')}</span>
        </div>
        <div className="wp-side-head-acts">
          <button
            type="button"
            className="btn small wp-side-add-btn"
            disabled={!hasNotes}
            title={tx('sideChatAddToMainTip')}
            onClick={() => bringSideChatToMain(hostId, parentSessionId, info)}
          >
            <Icon name="back" size={12} />
            <span>{tx('sideChatAddToMain')}</span>
          </button>
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
            <AsyncAskCard key={openAsk.id} ask={openAsk} tool={info.tool} compact onSend={sendText} onSkip={() => {}} />
          )}
        </div>
      )}

      <div className="wp-side-composer">
        <div className="wp-side-input-box">
          <textarea
            ref={taRef}
            className="wp-side-ta"
            rows={1}
            value={text}
            disabled={!connected}
            placeholder={tx('sideChatPlaceholder')}
            onChange={(e) => setText(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Escape' && busy) {
                e.preventDefault();
                stop();
                return;
              }
              if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
                e.preventDefault();
                sendText(text);
              }
            }}
          />
          <div className="wp-side-bar">
            <div className="wp-side-controls">
              <ModelPicker hostId={hostId} info={info} listenOpenEvent={false} />
              <PermPicker
                tool={info.tool}
                mode="structured"
                value={info.permissionMode}
                onChange={setPermMode}
                open={permOpen}
                setOpen={setPermOpen}
              />
            </div>
            <div className="wp-side-send-wrap">
              {busy && (
                <button type="button" className="icon-btn wp-side-stop" title={tx('sideChatStop')} onClick={stop}>
                  <Icon name="stop" size={12} />
                </button>
              )}
              <button
                type="button"
                className="wp-side-send"
                disabled={!connected || sending || !text.trim()}
                title={tx('sideChatSend')}
                onClick={() => sendText(text)}
              >
                <Icon name="arrowUp" size={14} />
              </button>
            </div>
          </div>
        </div>
      </div>
    </div>
  );
}
