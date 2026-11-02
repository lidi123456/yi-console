// Code outline: find functions, classes, headings, etc. in source with per-extension regexes, without a syntax tree
// (language packs are lazy-loaded and large files are only parsed up to the visible part, so regexes are more predictable)

export interface Symbol {
  name: string;
  /** function / class / type / heading / ... */
  kind: string;
  /** 1-based */
  line: number;
  /** Indent level (headings use the number of #) */
  depth: number;
}

type Rule = [RegExp, string];

const JS: Rule[] = [
  [/^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s*\*?\s*([A-Za-z_$][\w$]*)/, 'function'],
  [/^\s*(?:export\s+)?(?:default\s+)?(?:abstract\s+)?class\s+([A-Za-z_$][\w$]*)/, 'class'],
  [/^\s*(?:export\s+)?interface\s+([A-Za-z_$][\w$]*)/, 'interface'],
  [/^\s*(?:export\s+)?type\s+([A-Za-z_$][\w$]*)\s*(?:<[^=]*>)?\s*=/, 'type'],
  [/^\s*(?:export\s+)?(?:const\s+)?enum\s+([A-Za-z_$][\w$]*)/, 'enum'],
  [/^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_$][\w$]*)\s*(?::[^=]+)?=\s*(?:async\s+)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*(?::[^=]+)?=>/, 'function'],
  [/^\s+(?:public\s+|private\s+|protected\s+|static\s+|async\s+|readonly\s+)*([A-Za-z_$][\w$]*)\s*\([^)]*\)\s*(?::[^{]+)?\{\s*$/, 'method'],
];

const RULES: Record<string, Rule[]> = {
  js: JS,
  jsx: JS,
  ts: JS,
  tsx: JS,
  mjs: JS,
  cjs: JS,
  vue: JS,
  rs: [
    [/^\s*(?:pub(?:\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?fn\s+([A-Za-z_]\w*)/, 'function'],
    [/^\s*(?:pub(?:\([^)]*\))?\s+)?struct\s+([A-Za-z_]\w*)/, 'struct'],
    [/^\s*(?:pub(?:\([^)]*\))?\s+)?enum\s+([A-Za-z_]\w*)/, 'enum'],
    [/^\s*(?:pub(?:\([^)]*\))?\s+)?trait\s+([A-Za-z_]\w*)/, 'interface'],
    [/^\s*(?:pub(?:\([^)]*\))?\s+)?type\s+([A-Za-z_]\w*)/, 'type'],
    [/^\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+([A-Za-z_]\w*)/, 'module'],
    [/^\s*impl(?:<[^>]*>)?\s+(.+?)\s*\{?\s*$/, 'impl'],
    [/^\s*macro_rules!\s*([A-Za-z_]\w*)/, 'function'],
  ],
  py: [
    [/^\s*(?:async\s+)?def\s+([A-Za-z_]\w*)/, 'function'],
    [/^\s*class\s+([A-Za-z_]\w*)/, 'class'],
  ],
  go: [
    [/^func\s+(?:\([^)]*\)\s*)?([A-Za-z_]\w*)/, 'function'],
    [/^type\s+([A-Za-z_]\w*)\s+struct/, 'struct'],
    [/^type\s+([A-Za-z_]\w*)\s+interface/, 'interface'],
    [/^type\s+([A-Za-z_]\w*)/, 'type'],
  ],
  java: [
    [/^\s*(?:public|private|protected|static|final|abstract|\s)*(?:class|interface|enum|record)\s+([A-Za-z_]\w*)/, 'class'],
    [/^\s+(?:public|private|protected|static|final|synchronized|abstract|\s)+[\w<>[\],\s]+\s+([A-Za-z_]\w*)\s*\([^;]*$/, 'method'],
  ],
  kt: [
    [/^\s*(?:\w+\s+)*(?:class|interface|object)\s+([A-Za-z_]\w*)/, 'class'],
    [/^\s*(?:\w+\s+)*fun\s+(?:<[^>]*>\s*)?(?:[\w.]+\.)?([A-Za-z_]\w*)/, 'function'],
  ],
  c: [
    [/^(?:static\s+|inline\s+|extern\s+)*[A-Za-z_][\w\s*]*?\b([A-Za-z_]\w*)\s*\([^;]*$/, 'function'],
    [/^\s*(?:typedef\s+)?struct\s+([A-Za-z_]\w*)/, 'struct'],
  ],
  rb: [
    [/^\s*def\s+(?:self\.)?([A-Za-z_]\w*[?!]?)/, 'function'],
    [/^\s*(?:class|module)\s+([A-Z]\w*(?:::\w+)*)/, 'class'],
  ],
  php: [
    [/^\s*(?:\w+\s+)*function\s+([A-Za-z_]\w*)/, 'function'],
    [/^\s*(?:\w+\s+)*(?:class|interface|trait)\s+([A-Za-z_]\w*)/, 'class'],
  ],
  sh: [[/^\s*(?:function\s+)?([A-Za-z_][\w-]*)\s*\(\)\s*\{?/, 'function']],
  css: [[/^([^\s@{][^{]*?)\s*\{\s*$/, 'selector']],
  toml: [[/^\s*\[+([^\]]+)\]+/, 'section']],
  ini: [[/^\s*\[([^\]]+)\]/, 'section']],
  yaml: [[/^([A-Za-z_][\w.-]*):/, 'key']],
  json: [[/^ {2}"([^"]+)"\s*:/, 'key']],
};
RULES.cpp = RULES.cc = RULES.h = RULES.hpp = RULES.c;
RULES.cs = RULES.java;
RULES.bash = RULES.zsh = RULES.sh;
RULES.scss = RULES.less = RULES.css;
RULES.yml = RULES.yaml;
RULES.svelte = JS;

const KEYWORDS = new Set(['if', 'for', 'while', 'switch', 'catch', 'return', 'else', 'do', 'try', 'with', 'function', 'new', 'sizeof']);

export function outline(filename: string, text: string): Symbol[] {
  const ext = filename.includes('.') ? filename.slice(filename.lastIndexOf('.') + 1).toLowerCase() : '';
  const lines = text.split('\n');
  const out: Symbol[] = [];
  if (['md', 'markdown', 'mdx'].includes(ext)) {
    let fence = false;
    lines.forEach((l, i) => {
      if (/^\s*(```|~~~)/.test(l)) fence = !fence;
      const m = !fence && /^(#{1,6})\s+(.+?)\s*#*\s*$/.exec(l);
      if (m) out.push({ name: m[2], kind: 'heading', line: i + 1, depth: m[1].length - 1 });
    });
    return out;
  }
  const rules = RULES[ext];
  if (!rules) return out;
  let block = false;
  for (let i = 0; i < lines.length && out.length < 2000; i++) {
    const l = lines[i];
    // Skip content inside block comments
    if (block) {
      if (l.includes('*/')) block = false;
      continue;
    }
    if (/^\s*\/\*/.test(l) && !l.includes('*/')) {
      block = true;
      continue;
    }
    if (/^\s*(\/\/|#(?!\[)|--|\*)/.test(l) && ext !== 'css' && ext !== 'toml' && ext !== 'ini') continue;
    for (const [re, kind] of rules) {
      const m = re.exec(l);
      if (!m) continue;
      const name = m[1].trim();
      if (!name || KEYWORDS.has(name)) break;
      const indent = /^\s*/.exec(l)![0].replace(/\t/g, '    ').length;
      out.push({ name, kind, line: i + 1, depth: Math.min(4, Math.floor(indent / (ext === 'yaml' ? 2 : 4))) });
      break;
    }
  }
  return out;
}

export const KIND_LABEL: Record<string, string> = {
  function: 'ƒ',
  method: 'm',
  class: 'C',
  struct: 'S',
  interface: 'I',
  type: 'T',
  enum: 'E',
  module: 'M',
  impl: 'i',
  heading: '#',
  selector: '{}',
  section: '[]',
  key: 'k',
};
