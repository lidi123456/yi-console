import { useState } from 'react';
import { isEn, tr } from '../i18n';
import type { Question, QuestionAnswer } from '../types';
import { Icon } from './Icon';

// AI 提问的卡片：一道或几道题，每题点选项（单选 / 多选），允许的话还能自己填；提交或跳过。
// Claude、Codex、Cursor 的提问请求和 Kiro 正文里的编号选项都用它

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
  /** 已回答：optionId 是 answer / skip / cancelled */
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
    // 单选：选项和自己填的二选一
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
        {questions.length > 1 && <span className="muted small">{tr(`共 ${questions.length} 个问题`, `${questions.length} questions`)}</span>}
        {compact && (
          <button
            type="button"
            className="icon-btn card-fold-btn"
            title={folded ? tr('展开问题', 'Expand') : tr('暂时收起', 'Collapse')}
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
                {q.multiSelect && !/多选|multi/i.test(q.question) && <span className="muted small">{tr('（可多选）', ' (Multi-select)')}</span>}
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
                  placeholder={q.options.length ? tr('其他，自己填写…', 'Other (type your answer)…') : tr('输入你的回答…', 'Type your answer…')}
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
            {tr('跳过', 'Skip')}
          </button>
        )}
        <button
          className="btn primary small"
          disabled={busy || !ready}
          onClick={submit}
          title={ready ? '' : tr('每个问题都要回答', 'Please answer all questions')}
        >
          {submitLabel ?? tr('提交', 'Submit')}
        </button>
      </div>
        </>
      )}
    </div>
  );
}

/** 一道题的回答写成文字 */
export function answerLabel(q: Question, a?: QuestionAnswer) {
  if (!a) return '';
  return [...a.selected.map((id) => q.options.find((o) => o.id === id)?.label ?? id), a.text?.trim() ?? '']
    .filter(Boolean)
    .join(isEn() ? ', ' : '、');
}

function Resolved({ title, questions, resolved }: { title: string; questions: Question[]; resolved: { optionId: string; answers: QuestionAnswer[] } }) {
  const how = resolved.optionId === 'answer' ? '' : resolved.optionId === 'cancelled' ? tr('已取消', 'Cancelled') : tr('已跳过', 'Skipped');
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
              <span>{q.secret ? '••••' : answerLabel(q, resolved.answers.find((a) => a.questionId === q.id)) || tr('（未回答）', '(Unanswered)')}</span>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

/**
 * Kiro 没有提问工具，需要用户选择时在正文里列编号问题和 a) b) c) 选项，并让用户按 "1=a, 2=b" 回复。
 * 从正文里把这种结构认出来，回答时拼成同样格式的一条消息
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
      // 没有编号的单个问题：选项前面那一行就是问题
      if (!q) {
        q = { id: '1', question: clean(lastLine) || tr('请选择', 'Please select'), options: [], allowOther: true };
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
  // 要有像样的选项结构，并且只认结尾附近的提问（避免把普通的编号列表当成问题）
  if (!good.length) return [];
  const hint = /reply with your choices|1\s*=\s*a|请回复|回复.*选择/i.test(text);
  return hint || good.length === qs.length ? good : [];
}

/** 把回答拼成 "1=a, 2=b, 3=自己的话" */
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
