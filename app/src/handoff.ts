import { getT } from './i18n';
import { TOOL_LABEL, type SessionInfo, type ToolKind } from './types';
import { lsGet, lsSet } from './util';

const t = getT('handoff');

// Continue with another tool. The new session is granted yi_session read access to the original one (`Hub::create` with
// `handoffFrom`), so the default first message carries a short digest plus the session id for session_read; the new tool
// never has to find or parse another tool's log. In the first turn it only reports back, without continuing the work.
// Terminal sessions whose tool log the daemon can't parse (typed input isn't in our events) keep the log-file template

/** Format notes for each tool's session files, so the new tool doesn't waste time guessing the format */
function formatHint(tool: ToolKind): string {
  switch (tool) {
    case 'claude':
    case 'qoder':
      return t('jsonlOneJsonObjectPer');
    case 'codex':
      return t('jsonlOneJsonObjectPer2');
    case 'kiro':
      return t('jsonlOneJsonObjectPer3');
    case 'codebuddy':
      return t('jsonlOneJsonObjectPer4');
    default:
      return '';
  }
}

/** Placeholders usable in the template; system settings shows this list to the user as help */
export const HANDOFF_VARS: [string, () => string][] = [
  ['{tool}', () => t('toolOfTheOriginalSession')],
  ['{title}', () => t('titleOfTheOriginalSession')],
  ['{cwd}', () => t('workingDirectory')],
  ['{path}', () => t('absolutePathOfTheOriginal')],
  ['{format}', () => t('descriptionOfTheRecordFile')],
  ['{sessionId}', () => t('idOfTheOriginalSession')],
  ['{digest}', () => t('digestOfTheOriginalSession')],
];

/** Terminal-mode tools whose own log the daemon turns into a timeline for session_read (`history::read`) */
const NATIVE_LOG_TOOLS: readonly ToolKind[] = ['claude', 'codex', 'qoder'];

/** Whether session_read sees the whole original conversation, so the session-id template can be used */
export function handoffViaSessionRead(info: SessionInfo): boolean {
  return info.mode !== 'terminal' || NATIVE_LOG_TOOLS.includes(info.tool);
}

/** The default template follows the UI language; a user-edited one is stored in local localStorage, and leaving it empty restores the default */
const TEMPLATE_KEY = 'handoff-template';

/** viaSessionRead false: the original session is only readable through its log file (see `handoffViaSessionRead`) */
export function defaultHandoffTemplate(viaSessionRead = true): string {
  return viaSessionRead ? t('handoffViaSessionReadTemplate') : t('thisTaskWasPreviouslyWorked');
}

/** The template a hand-off from `info` will use. supported false: the host's daemon predates session read grants
 *  (no `session.digest`), so the new session couldn't call session_read and the log-file template is used */
export function handoffTemplate(info: SessionInfo, supported = true): string {
  return customHandoffTemplate() ?? defaultHandoffTemplate(supported && handoffViaSessionRead(info));
}

/** Whether the template needs the log file path ({path} / {format}) */
export const templateNeedsPath = (tpl: string) => /\{(path|format)\}/.test(tpl);

/** The user's own template; null if never edited */
export function customHandoffTemplate(): string | null {
  const v = lsGet<string>(TEMPLATE_KEY, '');
  return v.trim() ? v : null;
}

/** Save the template; if it equals the default or is empty, delete it and use the default template for the UI language from then on */
export function setHandoffTemplate(v: string | null) {
  if (!v || !v.trim() || v === defaultHandoffTemplate()) localStorage.removeItem(TEMPLATE_KEY);
  else lsSet(TEMPLATE_KEY, v);
}

/** The first message. path is the log file (empty when the template doesn't need it); exported means it is the Markdown
 *  exported by Yi Console because the tool has no log file of its own */
export function handoffPrompt(info: SessionInfo, opts: { path?: string; exported?: boolean; digest?: string; supported?: boolean }): string {
  const fmt = opts.exported ? t('markdownExportedByYiConsole') : formatHint(info.tool);
  const vars: Record<string, string> = {
    tool: TOOL_LABEL[info.tool],
    title: info.title || t('untitledSession'),
    cwd: info.cwd,
    path: opts.path ?? '',
    format: fmt,
    sessionId: info.id,
    digest: opts.digest?.trim() || t('noDigestAvailable'),
  };
  // Unrecognized {xxx} are left as is, so braces in the user's template aren't swallowed
  return handoffTemplate(info, opts.supported ?? true).replace(/\{(tool|title|cwd|path|format|sessionId|digest)\}/g, (_, k: string) => vars[k]);
}
