// Code and diff snippets referenced from the workspace file panel as AI context.
// - In the composer, references sit in a compact pill bar above the textarea (not dumped into the text).
// - In structured chat mode, sending appends <yi-context ...> blocks to the prompt so every AI tool receives the exact
//   code / unified diff (including deleted lines no longer on disk), while ChatView and QueueList strip the blocks and
//   render only compact clickable pills above the user bubble.

export interface ContextRef {
  id: string;
  kind: 'diff' | 'code';
  /** Path relative to the workspace root (or repo-relative path for a commit file, or untitled tab label) */
  path: string;
  /** Commit hash when referenced from a commit diff tab (git:<hash>:<rel>) */
  commit?: string;
  /** Referenced from an unsaved untitled tab (not a file on disk) */
  untitled?: boolean;
  /** 1-based line range in the file */
  fromLine: number;
  toLine: number;
  /** Added / deleted line counts when kind === 'diff' */
  added?: number;
  deleted?: number;
  /** Unified diff hunk (for 'diff') or raw selected lines (for 'code') */
  content: string;
}

export type ContextRefInput = Omit<ContextRef, 'id'>;

export const ADD_CONTEXT_REF = 'yi-console:add-context-ref';

let refSeq = 0;
export const nextContextRefId = () => `cref-${Date.now()}-${++refSeq}`;

export function addContextRefToChat(sessionId: string, ref: ContextRefInput) {
  if (!ref.content) return;
  window.dispatchEvent(new CustomEvent(ADD_CONTEXT_REF, { detail: { sessionId, ref } }));
}

export function sameContextRef(a: ContextRefInput, b: ContextRefInput): boolean {
  return (
    a.kind === b.kind &&
    a.path === b.path &&
    (a.commit ?? '') === (b.commit ?? '') &&
    a.fromLine === b.fromLine &&
    a.toLine === b.toLine &&
    a.content === b.content
  );
}

export function formatLineRange(fromLine: number, toLine: number): string {
  return fromLine >= toLine ? `${fromLine}` : `${fromLine}-${toLine}`;
}

const escAttr = (s: string) =>
  s.replace(/&/g, '&amp;').replace(/"/g, '&quot;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

const unescAttr = (s: string) =>
  s.replace(/&quot;/g, '"').replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&amp;/g, '&');

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
  const lang = r.kind === 'diff' ? 'diff' : extLang(r.path);
  const inner = `${fence}${lang}\n${normContent}\n${fence}`;
  const tag = pickTag(inner);
  const attrs = [
    `kind="${r.kind}"`,
    `path="${escAttr(r.path)}"`,
    `lines="${formatLineRange(r.fromLine, r.toLine)}"`,
  ];
  if (r.commit) attrs.push(`commit="${escAttr(r.commit)}"`);
  if (r.untitled) attrs.push(`untitled="1"`);
  if (r.kind === 'diff') {
    if (r.added) attrs.push(`add="${r.added}"`);
    if (r.deleted) attrs.push(`del="${r.deleted}"`);
  }
  return `<${tag} ${attrs.join(' ')}>\n${inner}\n</${tag}>`;
}

/** Combine user text with any attached context references */
export function serializeContextMessage(text: string, refs: readonly ContextRefInput[]): string {
  const clean = text.trim();
  if (!refs.length) return clean;
  const blocks = refs.map(serializeContextRef).join('\n\n');
  return clean ? `${clean}\n\n${blocks}` : blocks;
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
    const kind: 'diff' | 'code' = attrs.kind === 'diff' ? 'diff' : 'code';
    const path = attrs.path || 'snippet';
    const lm = /^(\d+)(?:-(\d+))?$/.exec(attrs.lines ?? '');
    const fromLine = lm ? Math.max(1, Number(lm[1])) : 1;
    const toLine = lm && lm[2] ? Math.max(fromLine, Number(lm[2])) : fromLine;
    const trimmed = body.trim();
    const fm = /^(`{3,})[^\n]*\n([\s\S]*?)(?:\n\1)?$/.exec(trimmed);
    const content = fm ? fm[2] : trimmed;
    const added = attrs.add ? Number(attrs.add) : undefined;
    const deleted = attrs.del ? Number(attrs.del) : undefined;
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
      content,
    });
    return '';
  });
  if (!refs.length) return { text: raw, refs: [] };
  return { text: stripped.replace(/\n{3,}/g, '\n\n').trim(), refs };
}
