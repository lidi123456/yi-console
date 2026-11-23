import { useEffect, useRef, useState } from 'react';
import { removeReviewContextRef } from '../contextRefs';
import { getT, useLang } from '../i18n';
import {
  formatFindingFollowUp,
  setFindingDisposition,
  type FindingDisposition,
  type FindingThreadMessage,
  type ReviewIssue,
} from '../reviewState';
import { errorToast, toast } from '../store';
import { rpc } from '../transport';
import { TOOL_LABEL, type HubEvent, type QuestionAnswer, type ToolKind } from '../types';
import { ChatMarkdown } from './ChatMarkdown';
import { PermissionCard } from './ChatView';
import { Icon, ToolBadge } from './Icon';
import { adoptReviewFindings, unadoptReviewFinding } from './ReviewTab';

const tx = getT('filePanel');

export function ReviewInlineWidget({
  hostId,
  parentSessionId,
  reviewSessionId,
  tool,
  issue,
  disposition,
  thread,
  streamingText,
  busy,
  connected,
  pendingPerms,
  onAnswerPerm,
  highlighted = false,
  commitHash,
  onOpenSideChat,
}: {
  hostId: string;
  parentSessionId: string;
  reviewSessionId: string;
  tool: ToolKind;
  issue: ReviewIssue;
  disposition: FindingDisposition;
  thread: FindingThreadMessage[];
  streamingText?: string | null;
  busy: boolean;
  connected: boolean;
  pendingPerms: Extract<HubEvent, { kind: 'permission_request' }>[];
  onAnswerPerm: (requestId: string, optionId: string, answers?: QuestionAnswer[]) => void;
  highlighted?: boolean;
  commitHash?: string;
  onOpenSideChat: () => void;
}) {
  useLang();
  const [collapsed, setCollapsed] = useState(disposition === 'dismissed');
  const [threadOpen, setThreadOpen] = useState(
    () => thread.length > 0 || streamingText != null || pendingPerms.length > 0,
  );
  const [draft, setDraft] = useState('');
  const [sending, setSending] = useState(false);
  const taRef = useRef<HTMLTextAreaElement>(null);

  useEffect(() => {
    if (disposition === 'dismissed') setCollapsed(true);
    else if (highlighted) setCollapsed(false);
  }, [disposition, highlighted]);

  useEffect(() => {
    if (streamingText != null || pendingPerms.length > 0) {
      setCollapsed(false);
      setThreadOpen(true);
    }
  }, [streamingText, pendingPerms.length]);

  const sevLabel =
    issue.severity === 'high' ? tx('sevHigh') : issue.severity === 'medium' ? tx('sevMedium') : tx('sevLow');
  const toolName = TOOL_LABEL[tool] ?? tool;
  const threadActive = streamingText != null;

  const openThread = () => {
    setCollapsed(false);
    setThreadOpen((v) => {
      const next = !v;
      if (next) requestAnimationFrame(() => taRef.current?.focus());
      return next;
    });
  };

  const sendFollowUp = async () => {
    const q = draft.trim();
    if (!q || sending || !connected) return;
    setSending(true);
    try {
      const payload = formatFindingFollowUp(
        {
          findingId: issue.id,
          file: issue.file,
          line: issue.line,
          title: issue.title,
        },
        q,
      );
      await rpc(hostId, 'session.send', {
        sessionId: reviewSessionId,
        text: payload,
        queue: true,
      });
      setDraft('');
      setThreadOpen(true);
    } catch (e) {
      errorToast(e);
    } finally {
      setSending(false);
      requestAnimationFrame(() => taRef.current?.focus());
    }
  };

  const stop = () => rpc(hostId, 'session.interrupt', { sessionId: reviewSessionId }).catch(errorToast);

  return (
    <div
      className={`wp-inline-rev sev-${issue.severity} disp-${disposition} ${highlighted ? 'highlighted' : ''}`}
      data-finding-id={issue.id}
    >
      <div className="wp-inline-rev-head">
        <span className={`wp-review-sev sev-${issue.severity}`}>{sevLabel}</span>
        <span className="wp-inline-rev-tool" title={toolName}>
          <ToolBadge tool={tool} size={13} />
        </span>
        <span className="wp-inline-rev-title">{issue.title}</span>
        {issue.line && <span className="wp-inline-rev-line mono">L{issue.line}</span>}
        {disposition !== 'open' && (
          <span className={`wp-review-disp disp-${disposition}`}>
            {disposition === 'adopted' ? tx('reviewStatusAdopted') : tx('reviewStatusDismissed')}
          </span>
        )}
        <button
          type="button"
          className="icon-btn wp-inline-rev-collapse"
          onClick={() => setCollapsed((v) => !v)}
          title={collapsed ? tx('reviewExpandFinding') : tx('reviewCollapseFinding')}
        >
          <Icon name={collapsed ? 'down' : 'up'} size={12} />
        </button>
      </div>

      {!collapsed && (
        <>
          {issue.summary && <div className="wp-inline-rev-desc">{issue.summary}</div>}
          {issue.suggestion && (
            <div className="wp-inline-rev-sug">
              <b>{tx('reviewSuggestion')}: </b>
              {issue.suggestion}
            </div>
          )}

          <div className="wp-inline-rev-acts">
            <button
              type="button"
              className={`wp-review-act-btn ${threadOpen ? 'active' : ''}`}
              onClick={openThread}
            >
              <Icon name="chat" size={12} />
              <span>{tx('reviewAskToolBtn', { tool: toolName })}</span>
              {thread.length > 0 && <span className="wp-review-act-cnt">{thread.length}</span>}
            </button>

            {disposition === 'adopted' ? (
              <button
                type="button"
                className="wp-review-act-btn active"
                onClick={() =>
                  unadoptReviewFinding({
                    hostId,
                    reviewSessionId,
                    parentSessionId,
                    findingId: issue.id,
                  })
                }
              >
                <Icon name="check" size={12} />
                <span>{tx('reviewUnadoptBtn')}</span>
              </button>
            ) : (
              <button
                type="button"
                className="wp-review-act-btn primary"
                onClick={() => {
                  const n = adoptReviewFindings({
                    hostId,
                    reviewSessionId,
                    parentSessionId,
                    tool,
                    issues: [issue],
                    commitHash,
                  });
                  if (n > 0) toast(tx('reviewAdoptedToast', { n }));
                }}
              >
                <Icon name="plus" size={12} />
                <span>{tx('reviewAdoptBtn')}</span>
              </button>
            )}

            {disposition === 'dismissed' ? (
              <button
                type="button"
                className="wp-review-act-btn"
                onClick={() => setFindingDisposition(hostId, reviewSessionId, issue.id, 'open')}
              >
                <span>{tx('reviewRestoreBtn')}</span>
              </button>
            ) : (
              <button
                type="button"
                className="wp-review-act-btn muted"
                onClick={() => {
                  if (disposition === 'adopted') {
                    removeReviewContextRef(parentSessionId, reviewSessionId, issue.id);
                  }
                  setFindingDisposition(hostId, reviewSessionId, issue.id, 'dismissed');
                }}
              >
                <span>{tx('reviewDismissBtn')}</span>
              </button>
            )}

            <button type="button" className="wp-review-act-btn ml-auto" onClick={onOpenSideChat}>
              <Icon name="external" size={11} />
              <span>{tx('reviewOpenSideBtn')}</span>
            </button>
          </div>

          {threadOpen && (
            <div className="wp-inline-rev-drawer">
              {(thread.length > 0 || threadActive) && (
                <div className="wp-inline-rev-thread">
                  {thread.map((m) => (
                    <div key={`${m.seq}-${m.role}`} className={`wp-inline-rev-msg ${m.role}`}>
                      {m.role === 'user' ? (
                        <div className="wp-inline-rev-user">{m.text}</div>
                      ) : (
                        <div className="wp-inline-rev-ai">
                          <ChatMarkdown text={m.text} />
                        </div>
                      )}
                    </div>
                  ))}
                  {threadActive && (
                    <div className="wp-inline-rev-msg assistant live">
                      <div className="wp-inline-rev-ai">
                        <div className="wp-inline-rev-streaming">
                          <span className="spinner mini" />
                          <span>{tx('reviewReplyStreaming', { tool: toolName })}</span>
                        </div>
                        {streamingText?.trim() && <ChatMarkdown text={streamingText} streaming />}
                      </div>
                    </div>
                  )}
                </div>
              )}

              {pendingPerms.length > 0 && (
                <div className="wp-inline-rev-perms">
                  {pendingPerms.map((p) => (
                    <PermissionCard key={p.requestId} it={{ ...p, input: p.input }} onAnswer={onAnswerPerm} compact />
                  ))}
                </div>
              )}

              <div className="wp-inline-rev-input-row">
                <textarea
                  ref={taRef}
                  rows={1}
                  value={draft}
                  disabled={!connected}
                  placeholder={connected ? tx('reviewInlineInputPlaceholder', { tool: toolName }) : tx('reviewOffline')}
                  onChange={(e) => setDraft(e.target.value)}
                  onKeyDown={(e) => {
                    if (e.nativeEvent.isComposing || e.keyCode === 229) return;
                    if (e.key === 'Enter' && !e.shiftKey) {
                      e.preventDefault();
                      void sendFollowUp();
                    }
                  }}
                />
                {busy && !draft.trim() ? (
                  <button type="button" className="btn small" onClick={stop} title={tx('stopReview')}>
                    <Icon name="stop" size={12} />
                  </button>
                ) : (
                  <button
                    type="button"
                    className="btn small primary"
                    disabled={!connected || sending || !draft.trim()}
                    onClick={() => void sendFollowUp()}
                  >
                    {sending ? <span className="spinner mini" /> : <Icon name="send" size={12} />}
                    <span>{tx('reviewSendInlineBtn')}</span>
                  </button>
                )}
              </div>
            </div>
          )}
        </>
      )}
    </div>
  );
}
