import { isEn } from './i18n';
import type { SessionMode, ToolKind } from './types';

const PERM_MODES_ZH: [string, string, string][] = [
  ['default', '默认', '按工具自身规则询问审批'],
  ['acceptEdits', '自动接受编辑', '改文件不再询问'],
  ['auto', '自动审批', '由工具自己判断放不放行，有风险的操作仍会拦下'],
  ['plan', '只读规划', '只分析不改动'],
  ['bypass', '全部放行', '不询问任何操作，谨慎使用'],
];

const PERM_MODES_EN: [string, string, string][] = [
  ['default', 'Default', 'Ask for approval according to tool rules'],
  ['acceptEdits', 'Accept Edits', 'Auto-approve file edits'],
  ['auto', 'Auto', 'Let the tool decide; risky actions still require approval'],
  ['plan', 'Plan', 'Read-only planning without making changes'],
  ['bypass', 'Bypass All', 'Skip all permission prompts (use with caution)'],
];

export function getPermModesList(): [string, string, string][] {
  return isEn() ? PERM_MODES_EN : PERM_MODES_ZH;
}

/** Permission modes each tool actually supports; keep in sync with yi-agentd's `permission_modes` */
export function permModes(tool: ToolKind, mode: SessionMode) {
  const ids: Record<ToolKind, string[]> = {
    claude: ['default', 'acceptEdits', 'auto', 'plan', 'bypass'],
    codex: mode === 'structured' ? ['default', 'acceptEdits', 'auto', 'plan', 'bypass'] : ['default', 'auto', 'plan', 'bypass'],
    cursor: mode === 'structured' ? ['default', 'acceptEdits', 'plan', 'bypass'] : ['default', 'plan', 'bypass'],
    kiro: mode === 'structured' ? ['default', 'acceptEdits', 'plan', 'bypass'] : ['default', 'bypass'],
    qoder: ['default', 'acceptEdits', 'bypass'],
    codebuddy: ['default', 'acceptEdits', 'plan', 'bypass'],
    shell: [],
    files: [],
  };
  return getPermModesList().filter((m) => ids[tool].includes(m[0]));
}

/** When the tool switches itself to a mode we don't know (e.g. Claude's dontAsk), show its raw name */
export function permLabel(mode?: string | null) {
  return getPermModesList().find((m) => m[0] === (mode || 'default'))?.[1] ?? mode!;
}
