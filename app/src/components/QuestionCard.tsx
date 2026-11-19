import { useState } from 'react';
import { getT, isEn, useLang } from '../i18n';
import type { Question, QuestionAnswer } from '../types';
import { Icon } from './Icon';

const t = getT('questionCard');

// Card for questions from the AI: one or more questions, each answered by clicking options (single / multiple choice), with free text if allowed; submit or skip.
// Used for question requests from Claude, Codex, Cursor and Antigravity, and Codex's async questions

function displayTitle(title: string) {
  const m = /^(.+) 有问题想问你$/.exec(title);
  return m ? t('hasAQuestion', { name: m[1] }) : title;
}

function stripLeadingIndex(text: string, idx: number, total: number) {
  if (total <= 1) return text;
  const re = new RegExp(`^${idx + 1}[.、)]\\s*`);
  return text.replace(re, '') || text;
}

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
  useLang();
  const [draft, setDraft] = useState<Draft>({});
  const [busy, setBusy] = useState(false);
  const [folded, setFolded] = useState(false);
  const shownTitle = displayTitle(title);

  if (resolved) return <Resolved title={shownTitle} questions={questions} resolved={resolved} />;

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
    <div className={`q-card ${compact ? 'compact' : ''} ${folded ? 'folded' : ''}`} role="group" aria-label={shownTitle}>
      <div className="q-head" onClick={compact ? () => setFolded(!folded) : undefined}>
        <Icon name="chat" size={15} />
        <span className="q-title">{shownTitle}</span>
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
                <span>{stripLeadingIndex(q.question, qi, questions.length)}</span>
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
                    if (e.key === 'Enter' && !e.nativeEvent.isComposing && e.nativeEvent.keyCode !== 229 && ready) submit();
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

/** Answers to Codex's async questions as a plain message: just the answer for one question, else one "title: answer" line each */
export function asyncAnswer(questions: Question[], answers: QuestionAnswer[]) {
  const one = (q: Question) => answerLabel(q, answers.find((a) => a.questionId === q.id));
  if (questions.length === 1) return one(questions[0]);
  return questions
    .map((q) => [q, one(q)] as const)
    .filter(([, v]) => v)
    .map(([q, v]) => `${q.question}: ${v}`)
    .join('\n');
}

/** Read an async question's answer back from the message that answered it (the reverse of asyncAnswer) */
export function asyncAnswers(questions: Question[], text: string): QuestionAnswer[] {
  const pick = (q: Question, v: string): QuestionAnswer => {
    const opt = q.options.find((o) => o.label === v.trim());
    return opt ? { questionId: q.id, selected: [opt.id] } : { questionId: q.id, selected: [], text: v.trim() };
  };
  if (questions.length === 1) return [pick(questions[0], text)];
  const out: QuestionAnswer[] = [];
  for (const line of text.split('\n')) {
    const q = questions.find((q) => line.startsWith(`${q.question}: `));
    if (q) out.push(pick(q, line.slice(q.question.length + 2)));
  }
  // Typed by hand rather than through the card: show it under the first question
  return out.length || !questions.length ? out : [pick(questions[0], text)];
}
