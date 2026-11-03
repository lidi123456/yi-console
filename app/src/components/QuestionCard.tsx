import { useState } from 'react';
import { getT, isEn } from '../i18n';
import type { Question, QuestionAnswer } from '../types';
import { Icon } from './Icon';

const t = getT('questionCard');

// Card for questions from the AI: one or more questions, each answered by clicking options (single / multiple choice), with free text if allowed; submit or skip.
// Used for question requests from Claude, Codex and Cursor, and for numbered options in Kiro's reply text

type Draft = Record<string, { selected: string[]; text: string }>;

export function QuestionCard({
  title,
  questions,
  resolved,
  onSubmit,
  onSkip,
  compact,
  submitLabel,
}: {
  title: string;
  questions: Question[];
  /** Answered: optionId is answer / skip / cancelled */
  resolved?: { optionId: string; answers: QuestionAnswer[] };
  onSubmit?: (answers: QuestionAnswer[]) => Promise<void> | void;
  onSkip?: () => Promise<void> | void;
  compact?: boolean;
  submitLabel?: string;
}) {
  const [draft, setDraft] = useState<Draft>({});
  const [busy, setBusy] = useState(false);
  const [folded, setFolded] = useState(false);

  if (resolved) return <Resolved title={title} questions={questions} resolved={resolved} />;

  const get = (q: Question) => draft[q.id] ?? { selected: [], text: '' };
  const set = (q: Question, v: { selected: string[]; text: string }) => setDraft((d) => ({ ...d, [q.id]: v }));
  const pick = (q: Question, id: string) => {
    const cur = get(q);
    if (q.multiSelect) set(q, { ...cur, selected: cur.selected.includes(id) ? cur.selected.filter((x) => x !== id) : [...cur.selected, id] });
    // Single choice: either an option or free text
    else set(q, { selected: cur.selected[0] === id ? [] : [id], text: '' });
  };
  const answered = (q: Question) => get(q).selected.length > 0 || !!get(q).text.trim();
  const ready = questions.every(answered);

  const run = async (f?: () => Promise<void> | void) => {
    if (!f || busy) return;
    setBusy(true);
    try {
      await f();
    } finally {
      setBusy(false);
    }
  };
  const submit = () =>
    run(() => onSubmit?.(questions.map((q) => ({ questionId: q.id, selected: get(q).selected, text: get(q).text.trim() || undefined }))));

  return (
    <div className={`q-card ${compact ? 'compact' : ''} ${folded ? 'folded' : ''}`} role="group" aria-label={title}>
      <div className="q-head" onClick={compact ? () => setFolded(!folded) : undefined}>
        <Icon name="chat" size={15} />
        <span className="q-title">{title}</span>
        {questions.length > 1 && <span className="muted small">{t('questions', { length: questions.length })}</span>}
        {compact && (
          <button
            type="button"
            className="icon-btn card-fold-btn"
            title={folded ? t('expand') : t('collapse')}
            onClick={(e) => {
              e.stopPropagation();
              setFolded(!folded);
            }}
          >
            <Icon name={folded ? 'up' : 'down'} size={14} />
          </button>
        )}
      </div>
      {!folded && (
        <>
      <div className="q-list">
        {questions.map((q, qi) => {
          const cur = get(q);
          return (
            <div key={q.id} className="q-item">
              <div className="q-question">
                {questions.length > 1 && <span className="q-num">{qi + 1}</span>}
                {q.header && <span className="q-header">{q.header}</span>}
                <span>{q.question}</span>
                {q.multiSelect && !/多选|multi/i.test(q.question) && <span className="muted small">{t('multiSelect')}</span>}
              </div>
              {q.options.length > 0 && (
                <div className="q-options" role={q.multiSelect ? 'group' : 'radiogroup'}>
                  {q.options.map((o) => {
                    const on = cur.selected.includes(o.id);
                    return (
                      <button
                        key={o.id}
                        type="button"
                        role={q.multiSelect ? 'checkbox' : 'radio'}
                        aria-checked={on}
                        className={`q-opt ${on ? 'on' : ''}`}
                        onClick={() => pick(q, o.id)}
                      >
                        <span className={`q-mark ${q.multiSelect ? 'box' : 'dot'}`}>{on && <Icon name="check" size={11} />}</span>
                        <span className="q-opt-body">
                          <span className="q-opt-label">{o.label}</span>
                          {o.description && <span className="q-opt-desc">{o.description}</span>}
                        </span>
                      </button>
                    );
                  })}
                </div>
              )}
              {q.allowOther && (
                <input
                  className="q-other"
                  type={q.secret ? 'password' : 'text'}
                  value={cur.text}
                  placeholder={q.options.length ? t('otherTypeYourAnswer') : t('typeYourAnswer')}
                  onChange={(e) => set(q, { selected: q.multiSelect ? cur.selected : [], text: e.target.value })}
                  onKeyDown={(e) => {
                    if (e.key === 'Enter' && !e.nativeEvent.isComposing && ready) submit();
                  }}
                />
              )}
            </div>
          );
        })}
      </div>
      <div className="q-actions">
        {onSkip && (
          <button className="btn small" disabled={busy} onClick={() => run(onSkip)}>
            {t('skip')}
          </button>
        )}
        <button
          className="btn primary small"
          disabled={busy || !ready}
          onClick={submit}
          title={ready ? '' : t('pleaseAnswerAllQuestions')}
        >
          {submitLabel ?? t('submit')}
        </button>
      </div>
        </>
      )}
    </div>
  );
}

/** Write one question's answer as text */
export function answerLabel(q: Question, a?: QuestionAnswer) {
  if (!a) return '';
  return [...a.selected.map((id) => q.options.find((o) => o.id === id)?.label ?? id), a.text?.trim() ?? '']
    .filter(Boolean)
    .join(isEn() ? ', ' : '、');
}

function Resolved({ title, questions, resolved }: { title: string; questions: Question[]; resolved: { optionId: string; answers: QuestionAnswer[] } }) {
  const how = resolved.optionId === 'answer' ? '' : resolved.optionId === 'cancelled' ? t('cancelled') : t('skipped');
  return (
    <div className="q-card resolved">
      <div className="q-head">
        <Icon name="chat" size={15} />
        <span className="q-title">{title}</span>
        {how && <span className="pill">{how}</span>}
      </div>
      {!how && (
        <div className="q-summary">
          {questions.map((q) => (
            <div key={q.id} className="q-sum-row">
              <span className="muted">{q.header || q.question}</span>
              <span>{q.secret ? '••••' : answerLabel(q, resolved.answers.find((a) => a.questionId === q.id)) || t('unanswered')}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/**
 * Kiro has no question tool; when it needs the user to choose, it lists numbered questions with a) b) c) options in its reply and asks for a "1=a, 2=b" reply.
 * Recognize that structure in the reply text, and send answers as one message in the same format
 */
export function parseTextQuestions(text: string): Question[] {
  const clean = (s: string) => s.replace(/\*\*|__|`/g, '').trim();
  const qs: Question[] = [];
  let lastLine = '';
  for (const raw of text.split('\n')) {
    const line = raw.replace(/\s+$/, '');
    const opt = /^\s*(?:[-*]\s+)?\(?([a-hA-H])[).:]\s+(.+)$/.exec(line);
    const num = /^\s*(?:[-*]\s+)?(?:\*\*)?(\d{1,2})[.)、]\s*(.+)$/.exec(line);
    if (opt) {
      let q = qs[qs.length - 1];
      // A single question without a number: the line before the options is the question
      if (!q) {
        q = { id: '1', question: clean(lastLine) || t('pleaseSelect'), options: [], allowOther: true };
        qs.push(q);
      }
      const label = clean(opt[2]);
      const [head, ...rest] = label.split(/\s+[—–-]\s+|：|:\s/);
      q.options.push({ id: opt[1].toLowerCase(), label: rest.length ? head : label, description: rest.length ? rest.join(' - ') : undefined });
    } else if (num) {
      qs.push({ id: num[1], question: clean(num[2]), options: [], allowOther: true });
    } else if (line.trim()) lastLine = line;
  }
  const good = qs.filter((q) => q.options.length >= 2);
  // Require a proper option structure, and only accept questions near the end (so ordinary numbered lists aren't mistaken for questions)
  if (!good.length) return [];
  const hint = /reply with your choices|1\s*=\s*a|请回复|回复.*选择/i.test(text);
  return hint || good.length === qs.length ? good : [];
}

/** Join answers as "1=a, 2=b, 3=free text" */
export function textAnswer(questions: Question[], answers: QuestionAnswer[]) {
  return questions
    .map((q) => {
      const a = answers.find((x) => x.questionId === q.id);
      const v = [...(a?.selected ?? []), a?.text?.trim() ?? ''].filter(Boolean).join('+');
      return v ? `${q.id}=${v}` : '';
    })
    .filter(Boolean)
    .join(', ');
}
