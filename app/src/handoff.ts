import { getT } from './i18n';
import { TOOL_LABEL, type SessionInfo, type ToolKind } from './types';
import { lsGet, lsSet } from './util';

const t = getT('handoff');

// Continue with another tool: tell the new tool where the original session's log is; in the first turn it only reads it and reports back, without continuing the work

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
];

/** The default template follows the UI language; a user-edited one is stored in local localStorage, and leaving it empty restores the default */
const TEMPLATE_KEY = 'handoff-template';

export function defaultHandoffTemplate(): string {
  return t('thisTaskWasPreviouslyWorked');
}

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

/** The first message. exported means the tool has no log file of its own, and path is the Markdown exported by Yi Console */
export function handoffPrompt(info: SessionInfo, path: string, exported: boolean): string {
  const fmt = exported
    ? t('markdownExportedByYiConsole')
    : formatHint(info.tool);
  const vars: Record<string, string> = {
    tool: TOOL_LABEL[info.tool],
    title: info.title || t('untitledSession'),
    cwd: info.cwd,
    path,
    format: fmt,
  };
  // Unrecognized {xxx} are left as is, so braces in the user's template aren't swallowed
  return (customHandoffTemplate() ?? defaultHandoffTemplate()).replace(/\{(tool|title|cwd|path|format)\}/g, (_, k: string) => vars[k]);
}
