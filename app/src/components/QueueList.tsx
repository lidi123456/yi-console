import { useState } from 'react';
import { getT, useLang } from '../i18n';
import { errorToast } from '../store';
import { rpc } from '../transport';
import type { SessionInfo } from '../types';
import { Icon } from './Icon';
import { askConfirm } from './Overlay';

const t = getT('composer');
const DRAG = 'application/x-yc-queue';

/** Messages queued behind the current turn, above the input box: edit, drag to reorder, send now, recall, and pause /
 *  resume the whole queue. The queue lives on yi-agentd and survives its restarts, after which it waits for "Resume" */
export function QueueList({ hostId, info, onRecall }: { hostId: string; info: SessionInfo; onRecall: (text: string) => void }) {
  useLang();
  const queued = info.queued ?? [];
  const [editing, setEditing] = useState<{ id: string; text: string } | null>(null);
  const [over, setOver] = useState<number | null>(null);
  const cut = !!info.restartInterrupted;
  if (queued.length === 0 && !cut) return null;
  const paused = info.queuePaused;

  const act = async (params: Record<string, unknown>) => {
    try {
      return await rpc<{ text?: string } | null>(hostId, 'session.queue', { sessionId: info.id, ...params });
    } catch (e) {
      errorToast(e);
      return null;
    }
  };
  const recall = async (id: string) => {
    const r = await act({ id, action: 'remove' });
    if (r?.text != null) onRecall(r.text);
  };
  const saveEdit = async () => {
    if (!editing) return;
    const cur = queued.find((q) => q.id === editing.id);
    setEditing(null);
    if (cur && cur.text !== editing.text) await act({ id: editing.id, action: 'edit', text: editing.text });
  };
  const clear = async () => {
    const ok = await askConfirm({
      title: t('clearQueuedMessages', { n: queued.length }),
      message: t('queuedMessagesWillNotBeSent'),
      okText: t('clear'),
      danger: true,
    });
    if (ok) await act({ action: 'clear' });
  };
  const interrupted = async (action: 'continue' | 'dismiss') => {
    try {
      await rpc(hostId, 'session.interrupted', { sessionId: info.id, action });
    } catch (e) {
      errorToast(e);
    }
  };
  const n = queued.length;
  if (n === 0) {
    // Cut off by a restart with nothing queued: continue that turn, or leave it
    return (
      <div className="queue">
        <div className="queue-held" role="status">
          <Icon name="pause" size={14} />
          <span className="queue-text">{t('turnCutOffByRestart')}</span>
          <button className="btn small" onClick={() => interrupted('continue')} title={t('askTheToolToFinish')}>
            {t('continueTurn')}
          </button>
          <button className="btn small" onClick={() => interrupted('dismiss')}>
            {t('dismiss')}
          </button>
        </div>
      </div>
    );
  }
  const why =
    cut && paused === 'restart' ? t('turnCutOffQueueHeld', { n })
    : paused === 'restart' ? t('queueHeldAfterRestart', { n })
    : paused === 'interrupt' ? t('queueHeldAfterInterrupt', { n })
    : paused === 'handoff' ? t('queueHeldAfterHandoff', { n })
    : t('queuePaused', { n });

  return (
    <div className="queue">
      {paused && (
        <div className="queue-held" role="status">
          <Icon name="pause" size={14} />
          <span className="queue-text">{why}</span>
          <button className="btn small" onClick={() => act({ action: 'resume' })} title={cut ? t('askTheToolToFinishThenQueue') : t('sendTheNextQueuedMessage')}>
            {cut ? t('continueTurn') : t('resumeSending')}
          </button>
          <button className="btn small" onClick={clear}>
            {t('clear')}
          </button>
        </div>
      )}
      {queued.map((q, i) => (
        <div
          key={q.id}
          className={`queue-item ${over === i ? 'drop' : ''}`}
          draggable={editing?.id !== q.id}
          onDragStart={(e) => {
            e.dataTransfer.setData(DRAG, q.id);
            e.dataTransfer.effectAllowed = 'move';
          }}
          onDragOver={(e) => {
            if (!e.dataTransfer.types.includes(DRAG)) return;
            e.preventDefault();
            setOver(i);
          }}
          onDragLeave={() => setOver((o) => (o === i ? null : o))}
          onDrop={(e) => {
            const id = e.dataTransfer.getData(DRAG);
            setOver(null);
            if (!id) return;
            e.preventDefault();
            if (id !== q.id) act({ id, action: 'move', to: i });
          }}
        >
          <span className="queue-grip hide-mobile" title={t('dragToReorder')}>
            <Icon name="grip" size={14} />
          </span>
          {editing?.id === q.id ? (
            <input
              className="queue-edit"
              autoFocus
              value={editing.text}
              onChange={(e) => setEditing({ id: q.id, text: e.target.value })}
              onBlur={saveEdit}
              onKeyDown={(e) => {
                if (e.nativeEvent.isComposing) return;
                if (e.key === 'Enter') {
                  e.preventDefault();
                  saveEdit();
                } else if (e.key === 'Escape') {
                  e.preventDefault();
                  setEditing(null);
                }
              }}
            />
          ) : (
            <span className="queue-text truncate" title={q.text} onDoubleClick={() => setEditing({ id: q.id, text: q.text })}>
              {q.text || t('image')}
              {q.images > 0 && <span className="muted"> · {t('imageS', { images: q.images })}</span>}
            </span>
          )}
          {!paused && <span className="muted small hide-mobile">{i === 0 ? t('sendAfterCurrentTurn') : t('queued')}</span>}
          {i > 0 && (
            <button className="icon-btn show-mobile" title={t('moveUp')} onClick={() => act({ id: q.id, action: 'move', to: i - 1 })}>
              <Icon name="up" size={14} />
            </button>
          )}
          <button className="btn small" title={t('interruptCurrentTurnAndSend')} onClick={() => act({ id: q.id, action: 'sendNow' })}>
            {t('sendNow')}
          </button>
          {i === 0 && !paused && (
            <button className="icon-btn" title={t('pauseQueue')} onClick={() => act({ action: 'pause' })}>
              <Icon name="pause" size={14} />
            </button>
          )}
          <button className="icon-btn" title={t('editQueuedMessage')} onClick={() => setEditing({ id: q.id, text: q.text })}>
            <Icon name="edit" size={14} />
          </button>
          <button className="icon-btn" title={t('recallToInputOrPress')} onClick={() => recall(q.id)}>
            <Icon name="x" size={14} />
          </button>
        </div>
      ))}
    </div>
  );
}
