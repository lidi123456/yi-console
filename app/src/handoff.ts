import { tr } from './i18n';
import { TOOL_LABEL, type SessionInfo, type ToolKind } from './types';
import { lsGet, lsSet } from './util';

// Continue with another tool: tell the new tool where the original session's log is; in the first turn it only reads it and reports back, without continuing the work

/** Format notes for each tool's session files, so the new tool doesn't waste time guessing the format */
function formatHint(tool: ToolKind): string {
  switch (tool) {
    case 'claude':
    case 'qoder':
      return tr(
        'jsonl，每行一个 JSON。type 为 "user" / "assistant" 的行是对话，正文在 message.content（字符串，或数组：text 块是文字，tool_use / tool_result 是工具调用和结果）；其他 type（api-request、attachment、file-history-* 等）可以忽略。用户消息里 tool_result 那种是工具结果，不是用户说的话。',
        'JSONL, one JSON object per line. Lines with type "user" / "assistant" are the conversation; the content is in message.content (a string, or an array where text blocks are text and tool_use / tool_result are tool calls and results). Other types (api-request, attachment, file-history-*, etc.) can be ignored. User entries containing tool_result are tool results, not something the user said.',
      );
    case 'codex':
      return tr(
        'jsonl，每行一个 JSON。type=="response_item" 且 payload.type=="message" 的是对话，payload.role 为 user / assistant，正文在 payload.content[].text（role=user 的前几条可能是 AGENTS.md、环境信息等系统注入内容）；payload.type 为 function_call / custom_tool_call 的是工具调用，*_output 是结果；reasoning 可忽略。',
        'JSONL, one JSON object per line. Entries with type=="response_item" and payload.type=="message" are the conversation; payload.role is user / assistant and the text is in payload.content[].text (the first few role=user entries may be injected content such as AGENTS.md or environment info). payload.type function_call / custom_tool_call are tool calls, *_output are their results; reasoning can be ignored.',
      );
    case 'kiro':
      return tr(
        'jsonl，每行一个 JSON。kind=="Prompt" 是用户消息，kind=="AssistantMessage" 是回复，正文在 data.content[] 中 kind=="text" 的 data；kind=="ToolResults" 是工具结果，"Compaction" 是上下文压缩摘要。',
        'JSONL, one JSON object per line. kind=="Prompt" is a user message, kind=="AssistantMessage" is a reply, with the text in the data of data.content[] items whose kind=="text"; kind=="ToolResults" are tool results and "Compaction" is a context compaction summary.',
      );
    case 'codebuddy':
      return tr(
        'jsonl，每行一个 JSON。type=="message" 的是对话，role 为 user / assistant，正文在 content[].text（content 的 type 为 input_text / output_text）。',
        'JSONL, one JSON object per line. Entries with type=="message" are the conversation; role is user / assistant and the text is in content[].text (content type input_text / output_text).',
      );
    default:
      return '';
  }
}

/** Placeholders usable in the template; system settings shows this list to the user as help */
export const HANDOFF_VARS: [string, () => string][] = [
  ['{tool}', () => tr('原会话的工具名，如 Claude Code', 'Tool of the original session, e.g. Claude Code')],
  ['{title}', () => tr('原会话的标题', 'Title of the original session')],
  ['{cwd}', () => tr('工作目录', 'Working directory')],
  ['{path}', () => tr('原会话记录文件的绝对路径', 'Absolute path of the original session record')],
  ['{format}', () => tr('这个记录文件的格式说明（按工具自动给出）', 'Description of the record file format (filled in per tool)')],
];

/** The default template follows the UI language; a user-edited one is stored in local localStorage, and leaving it empty restores the default */
const TEMPLATE_KEY = 'handoff-template';

export function defaultHandoffTemplate(): string {
  return tr(
    `这个任务之前在 {tool} 里做，现在换你接手。工作目录：{cwd}

原会话记录：\`{path}\`
格式：{format}

请先读原会话记录了解之前的上下文，并结合 git status / git diff / git log 看当前改动。

这一轮只做汇报，不要修改任何文件，也不要接着做任务。请汇报：
- 之前的目标是什么
- 已完成哪些
- 还有哪些没完成，最后停在哪里
- 你建议的下一步`,
    `This task was previously worked on in {tool}; you are taking it over now. Working directory: {cwd}

Original session record: \`{path}\`
Format: {format}

First read the original session record to understand the earlier context, and check the current changes with git status / git diff / git log.

In this turn, only report back. Do not modify any files and do not continue the task. Please report:
- What the goal was
- What has been completed
- What is still unfinished, and where it stopped
- What you suggest as the next step`,
  );
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
    ? tr(
        'Markdown，是 Yi Console 从会话记录导出的：用户消息、回复正文和工具调用摘要（不含工具输出）。',
        'Markdown exported by Yi Console from the session record: user messages, reply text and tool call summaries (tool output not included).',
      )
    : formatHint(info.tool);
  const vars: Record<string, string> = {
    tool: TOOL_LABEL[info.tool],
    title: info.title || tr('未命名会话', 'Untitled session'),
    cwd: info.cwd,
    path,
    format: fmt,
  };
  // Unrecognized {xxx} are left as is, so braces in the user's template aren't swallowed
  return (customHandoffTemplate() ?? defaultHandoffTemplate()).replace(/\{(tool|title|cwd|path|format)\}/g, (_, k: string) => vars[k]);
}
