// Line endings and indentation of a text file, so the editor writes files back the way they were

/** Line ending the file mostly uses. CodeMirror splits on \r\n, \r and \n but joins with \n, so without this,
 *  editing one line of a CRLF file would turn every line ending into LF (VS Code keeps the file's own EOL) */
export function detectEol(text: string): '\r\n' | '\n' {
  const crlf = text.split('\r\n').length - 1;
  if (!crlf) return '\n';
  const lf = text.split('\n').length - 1 - crlf;
  return crlf >= lf ? '\r\n' : '\n';
}

/** Indentation the file uses, like VS Code's editor.detectIndentation: compare each line's leading whitespace with the
 *  previous line's and take the most common step. Defaults to 4 spaces (VS Code's default) */
export function detectIndent(text: string): { unit: string; tabSize: number } {
  let tabs = 0;
  let spaces = 0;
  const steps = new Map<number, number>();
  let prev = 0;
  let n = 0;
  for (const line of text.split('\n', 10000)) {
    if (!line.trim()) continue;
    if (++n > 2000) break;
    const ws = /^[\t ]*/.exec(line)![0];
    if (ws.startsWith('\t')) {
      tabs++;
      prev = 0;
      continue;
    }
    const w = ws.length;
    if (w >= 2) spaces++;
    // A lone space before * is the continuation of a /** block comment, not indentation
    if (w > prev && !(w - prev === 1 && line[w] === '*')) steps.set(w - prev, (steps.get(w - prev) ?? 0) + 1);
    prev = w;
  }
  if (tabs > spaces) return { unit: '\t', tabSize: 4 };
  let best = 0;
  let count = 0;
  for (const [step, c] of steps) if (step >= 2 && step <= 8 && c > count) [best, count] = [step, c];
  return best ? { unit: ' '.repeat(best), tabSize: best } : { unit: '    ', tabSize: 4 };
}
