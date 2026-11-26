import { getComposerSnapshot } from './composerState';
import { addContextRefToChat, type ContextRefInput } from './contextRefs';
import { getT } from './i18n';
import { permModes } from './perm';
import { getSession, hostStatus, loadSession, skey, toast } from './store';
import { rpc } from './transport';
import { TOOL_LABEL, type HubEvent, type SessionInfo, type ToolKind } from './types';
import { STATUS_LABEL } from './util';
import { openFileInPanel } from './components/SessionActions';

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

/** Whether a side chat has ever had a user message or an unsent draft (used to auto-delete unused side chats on tab close) */
export function sideChatHasMessages(hostId: string, sideSid: string, info?: SessionInfo): boolean {
  const key = skey(hostId, sideSid);
  const data = getSession(key);
  if (data.events.some((e) => e.kind === 'user_message' || e.kind === 'assistant_text')) return true;
  // The user-message index covers the whole history even when only a recent event window is loaded
  if (data.userMessages.length > 0) return true;
  // /clear wipes events and the index but keeps the session: it was used
  if (data.clearSeq > 0) return true;
  const draft = getComposerSnapshot(key);
  if (
    draft.text.trim() ||
    draft.imgs.length > 0 ||
    draft.refs.length > 0 ||
    draft.pendingUploads > 0 ||
    draft.uploads.length > 0 ||
    draft.sending ||
    draft.waitSend
  )
    return true;
  if (data.loaded && !data.hasMore) return false;
  if (data.loaded && data.userMessagesLoaded && !data.userMessagesUnsupported) return false;
  // Partial or unloaded history: startup status events bump lastSeq too, so it only proves
  // "something happened", not "someone talked". Stay conservative and keep the confirmation.
  return !!info && (info.lastSeq > 0 || info.turn != null || info.status === 'running');
}

/** Whether a side chat currently has an active turn or pending approval/question */
export function isSideChatBusy(info: Pick<SessionInfo, 'status' | 'pendingPermissions'>): boolean {
  return (
    info.status === 'running' ||
    info.status === 'starting' ||
    info.status === 'waiting-permission' ||
    info.pendingPermissions > 0
  );
}

/** Whether deleting a side chat from the header menu should require inline confirmation */
export function sideChatNeedsDeleteConfirm(hostId: string, info: SessionInfo, isReview: boolean): boolean {
  return isReview || isSideChatBusy(info) || sideChatHasMessages(hostId, info.id, info);
}

/** Build the compact right-side hint for a side chat item without repeating tool/scope already shown in the title */
export function sideChatMenuHint(info: SessionInfo, label: string): string {
  const toolLabel = TOOL_LABEL[info.tool];
  const scope = info.review?.scopeLabel;
  const status = STATUS_LABEL[info.status];
  return [
    toolLabel && !label.includes(toolLabel) ? toolLabel : null,
    scope && !label.includes(scope) ? scope : null,
    status,
  ]
    .filter(Boolean)
    .join(' · ');
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
