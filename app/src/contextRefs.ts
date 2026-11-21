// Code, diff, review comment and terminal snippets referenced into AI context.
// - In the composer, references sit in a compact pill bar above the textarea (not dumped into the text).
// - In structured chat mode, sending appends <yi-context ...> blocks to the prompt so every AI tool receives the exact
//   code / unified diff / inline review comment / terminal output, while ChatView and QueueList strip the blocks and
//   render only compact clickable pills above the user bubble.

export type ContextRefKind = 'diff' | 'code' | 'review' | 'terminal' | 'side';

export interface ContextRef {
  id: string;
  kind: ContextRefKind;
  /** Path relative to the workspace root (or repo-relative path for a commit file, untitled tab label, terminal title, or side chat title) */
  path: string;
  /** Commit hash when referenced from a commit diff tab (git:<hash>:<rel>) */
  commit?: string;
  /** Referenced from an unsaved untitled tab (not a file on disk) */
  untitled?: boolean;
  /** 1-based line range in the file (or 1..lineCount for terminal / side chat notes) */
  fromLine: number;
  toLine: number;
  /** Added / deleted line counts when kind === 'diff' or 'review' */
  added?: number;
  deleted?: number;
  /** Inline review comment text when kind === 'review', or short summary when kind === 'side' */
  comment?: string;
  /** Non-zero exit code when kind === 'terminal' (from OSC 133 triage) */
  exitCode?: number;
  /** Side chat session id when kind === 'side' (allows clicking the pill to reopen that side chat tab) */
  sideSessionId?: string;
  /** Unified diff hunk (for 'diff' / 'review'), raw selected lines (for 'code' / 'terminal'), or side chat conclusion markdown */
  content: string;
}

export type ContextRefInput = Omit<ContextRef, 'id'>;

export interface AddContextRefOptions {
  /** Focus the composer textarea after adding (default true; false for batched inline review comments) */
  focus?: boolean;
  /** Optional prompt prefix to insert into the composer if it is currently empty */
  prompt?: string;
}

export const ADD_CONTEXT_REF = 'yi-console:add-context-ref';
export const UPDATE_CONTEXT_REF = 'yi-console:update-context-ref';
export const REMOVE_CONTEXT_REF = 'yi-console:remove-context-ref';

let refSeq = 0;
export const nextContextRefId = () => `cref-${Date.now()}-${++refSeq}`;

export function addContextRefToChat(sessionId: string, ref: ContextRefInput, opts?: AddContextRefOptions) {
  if (!ref.content && !ref.comment) return;
  window.dispatchEvent(new CustomEvent(ADD_CONTEXT_REF, { detail: { sessionId, ref, ...opts } }));
}

export function sameContextRef(a: ContextRefInput, b: ContextRefInput): boolean {
  return (
    a.kind === b.kind &&
    a.path === b.path &&
    (a.commit ?? '') === (b.commit ?? '') &&
    a.fromLine === b.fromLine &&
    a.toLine === b.toLine &&
    (a.comment ?? '') === (b.comment ?? '') &&
    (a.exitCode ?? 0) === (b.exitCode ?? 0) &&
    (a.sideSessionId ?? '') === (b.sideSessionId ?? '') &&
    a.content === b.content
  );
}

export function formatLineRange(fromLine: number, toLine: number): string {
  return fromLine >= toLine ? `${fromLine}` : `${fromLine}-${toLine}`;
}

const escAttr = (s: string) =>
  s
    .replace(/&/g, '&amp;')
    .replace(/"/g, '&quot;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/\r?\n/g, '&#10;');

const unescAttr = (s: string) =>
  s
    .replace(/&#10;/g, '\n')
    .replace(/&quot;/g, '"')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&amp;/g, '&');

function extLang(path: string): string {
  const i = path.lastIndexOf('.');
  return i >= 0 ? path.slice(i + 1).toLowerCase().replace(/[^a-z0-9_+-]/g, '') : '';
}

function pickFence(s: string): string {
  let len = 3;
  for (const m of s.matchAll(/`{3,}/g)) len = Math.max(len, m[0].length + 1);
  return '`'.repeat(len);
}

function pickTag(body: string): string {
  let tag = 'yi-context';
  let n = 1;
  while (body.includes(`</${tag}>`)) tag = `yi-context-${n++}`;
  return tag;
}

/** Serialize one reference as a tagged block for the AI prompt */
export function serializeContextRef(r: ContextRefInput): string {
  const normContent = r.content.replace(/\r\n/g, '\n').replace(/\n$/, '');
  const fence = pickFence(normContent);
  const lang =
    r.kind === 'diff' || r.kind === 'review'
      ? 'diff'
      : r.kind === 'terminal'
        ? 'text'
        : r.kind === 'side'
          ? 'markdown'
          : extLang(r.path);
  const codeBlock = `${fence}${lang}\n${normContent}\n${fence}`;
  const inner =
    r.kind === 'review' && r.comment
      ? `Review comment: ${r.comment.trim()}\n${codeBlock}`
      : r.kind === 'side'
        ? `Side chat notes (${r.path}):\n${normContent}`
        : codeBlock;
  const tag = pickTag(inner);
  const attrs = [
    `kind="${r.kind}"`,
    `path="${escAttr(r.path)}"`,
    `lines="${formatLineRange(r.fromLine, r.toLine)}"`,
  ];
  if (r.commit) attrs.push(`commit="${escAttr(r.commit)}"`);
  if (r.untitled) attrs.push(`untitled="1"`);
  if (r.kind === 'diff' || r.kind === 'review') {
    if (r.added) attrs.push(`add="${r.added}"`);
    if (r.deleted) attrs.push(`del="${r.deleted}"`);
  }
  if ((r.kind === 'review' || r.kind === 'side') && r.comment) {
    attrs.push(`comment="${escAttr(r.comment.trim())}"`);
  }
  if (r.kind === 'terminal' && r.exitCode !== undefined) {
    attrs.push(`exit="${r.exitCode}"`);
  }
  if (r.kind === 'side' && r.sideSessionId) {
    attrs.push(`sid="${escAttr(r.sideSessionId)}"`);
  }
  return `<${tag} ${attrs.join(' ')}>\n${inner}\n</${tag}>`;
}

/** Combine user text with any attached context references */
export function serializeContextMessage(text: string, refs: readonly ContextRefInput[]): string {
  const clean = text.trim();
  if (!refs.length) return clean;
  const blocks = refs.map(serializeContextRef).join('\n\n');
  if (clean) return `${clean}\n\n${blocks}`;
  if (refs.every((r) => r.kind === 'review')) {
    return `Please address the following inline review comments:\n\n${blocks}`;
  }
  return blocks;
}

const TAG_RE = /<(yi-context(?:-\d+)?)\s+([^>\n]+)>\n([\s\S]*?)\n<\/\1>/g;
const ATTR_RE = /([a-z]+)="([^"]*)"/g;

/** Extract <yi-context> blocks from a stored/queued user message, returning the clean user text and pill metadata */
export function parseContextMessage(raw: string): { text: string; refs: ContextRef[] } {
  if (!raw.includes('<yi-context')) return { text: raw, refs: [] };
  const refs: ContextRef[] = [];
  let idx = 0;
  const stripped = raw.replace(TAG_RE, (_all, _tag: string, attrStr: string, body: string) => {
    const attrs: Record<string, string> = {};
    for (const m of attrStr.matchAll(ATTR_RE)) attrs[m[1]] = unescAttr(m[2]);
    const kind: ContextRefKind =
      attrs.kind === 'diff' || attrs.kind === 'review' || attrs.kind === 'terminal' || attrs.kind === 'side'
        ? attrs.kind
        : 'code';
    const path = attrs.path || 'snippet';
    const lm = /^(\d+)(?:-(\d+))?$/.exec(attrs.lines ?? '');
    const fromLine = lm ? Math.max(1, Number(lm[1])) : 1;
    const toLine = lm && lm[2] ? Math.max(fromLine, Number(lm[2])) : fromLine;
    let trimmed = body.trim();
    let comment = attrs.comment || undefined;
    if (kind === 'review' && trimmed.startsWith('Review comment: ')) {
      const nl = trimmed.indexOf('\n');
      if (nl > 0) {
        if (!comment) comment = trimmed.slice('Review comment: '.length, nl).trim();
        trimmed = trimmed.slice(nl + 1).trim();
      }
    }
    if (kind === 'side' && trimmed.startsWith('Side chat notes (')) {
      const nl = trimmed.indexOf('\n');
      if (nl > 0) trimmed = trimmed.slice(nl + 1).trim();
    }
    const fm = /^(`{3,})[^\n]*\n([\s\S]*?)(?:\n\1)?$/.exec(trimmed);
    const content = fm ? fm[2] : trimmed;
    const added = attrs.add ? Number(attrs.add) : undefined;
    const deleted = attrs.del ? Number(attrs.del) : undefined;
    const exitCode = attrs.exit !== undefined && attrs.exit !== '' ? Number(attrs.exit) : undefined;
    refs.push({
      id: `msg-ref-${idx++}`,
      kind,
      path,
      commit: attrs.commit || undefined,
      untitled: attrs.untitled === '1' || undefined,
      fromLine,
      toLine,
      added: added || undefined,
      deleted: deleted || undefined,
      comment,
      exitCode: Number.isFinite(exitCode) ? exitCode : undefined,
      sideSessionId: attrs.sid || undefined,
      content,
    });
    return '';
  });
  if (!refs.length) return { text: raw, refs: [] };
  return { text: stripped.replace(/\n{3,}/g, '\n\n').trim(), refs };
}
