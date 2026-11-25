// Line endings and indentation of a text file, so the editor writes files back the way they were

const LINE_SCAN_LIMIT = 2000;

/** Line ending the file mostly uses. CodeMirror splits on \r\n, \r and \n but joins with \n, so without this,
 *  editing one line of a CRLF file would turn every line ending into LF (VS Code keeps the file's own EOL) */
export function detectEol(text: string): '\r\n' | '\n' {
  let crlf = 0;
  let lf = 0;
  let pos = 0;
  while (true) {
    const nl = text.indexOf('\n', pos);
    if (nl < 0) break;
    if (nl > 0 && text.charCodeAt(nl - 1) === 13) crlf++;
    else lf++;
    pos = nl + 1;
  }
  if (!crlf) return '\n';
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
  const len = Math.min(text.length, 262144);
  let pos = 0;
  while (pos <= len) {
    const nl = text.indexOf('\n', pos);
    const end = nl < 0 ? text.length : nl;
    let firstNonWs = pos;
    let isTab = false;
    while (firstNonWs < end) {
      const ch = text.charCodeAt(firstNonWs);
      if (ch === 9) {
        if (firstNonWs === pos) isTab = true;
        firstNonWs++;
      } else if (ch === 32) {
        firstNonWs++;
      } else {
        break;
      }
    }
    const blank = firstNonWs === end || (firstNonWs === end - 1 && text.charCodeAt(firstNonWs) === 13);
    if (!blank) {
      if (++n > LINE_SCAN_LIMIT) break;
      if (isTab) {
        tabs++;
        prev = 0;
      } else {
        const w = firstNonWs - pos;
        if (w >= 2) spaces++;
        // A lone space before * is the continuation of a /** block comment, not indentation
        if (w > prev && !(w - prev === 1 && text.charCodeAt(firstNonWs) === 42)) {
          steps.set(w - prev, (steps.get(w - prev) ?? 0) + 1);
        }
        prev = w;
      }
    }
    if (nl < 0) break;
    pos = nl + 1;
  }
  if (tabs > spaces) return { unit: '\t', tabSize: 4 };
  let best = 0;
  let count = 0;
  for (const [step, c] of steps) if (step >= 2 && step <= 8 && c > count) [best, count] = [step, c];
  return best ? { unit: ' '.repeat(best), tabSize: best } : { unit: '    ', tabSize: 4 };
}
