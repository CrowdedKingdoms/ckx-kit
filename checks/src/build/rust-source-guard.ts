/**
 * Rust that reads the build host at compile time. Crates are compiled on the API's hosts,
 * so `include_str!("/etc/…")`, `env!("…")` or `#[path = "/proc/…"] mod x;` would put the
 * host's files or environment into the module, or into the compiler errors the developer
 * reads back. Refused:
 *   - the reading macros by name, anywhere in code (a `use … as` rename, or passing the
 *     name into a macro, still names them);
 *   - `#[path]` and `#[link]` attributes, including inside `cfg_attr`;
 *   - an out-of-line `mod x;` inside a `macro_rules!` body, where a metavariable could
 *     become a `#[path]` the attribute check never sees.
 *
 * Comments and string literals are skipped. Anything the scanner cannot classify counts as
 * code, so where it and rustc disagree it is only stricter. This is one layer: the build
 * also runs with a scrubbed environment, and a filesystem sandbox is the backstop.
 */

const BANNED_IDENTS = [
  'include',
  'include_str',
  'include_bytes',
  'env',
  'option_env',
  'asm',
  'global_asm',
  'naked_asm',
];
const BANNED_IDENT = new RegExp(`\\b(${BANNED_IDENTS.join('|')})\\b`);
const ATTR_START = /#\s*!?\s*\[/g;
const BANNED_ATTR = /\b(path|link|link_args)\b/;
const MACRO_RULES = /\bmacro_rules\s*!\s*[A-Za-z_]\w*\s*[{([]/g;
const OUT_OF_LINE_MOD = /\bmod\s+[$A-Za-z_][\w$]*\s*;/;

const CLOSE: Record<string, string> = { '{': '}', '(': ')', '[': ']' };

/** The index of the bracket closing the one at `open`, or the end of `code`. */
function closing(code: string, open: number): number {
  const o = code[open];
  const c = CLOSE[o];
  let depth = 0;
  for (let j = open; j < code.length; j++) {
    if (code[j] === o) depth++;
    else if (code[j] === c && --depth === 0) return j;
  }
  return code.length - 1;
}

function isIdentChar(c: string | undefined): boolean {
  return c !== undefined && /[A-Za-z0-9_]/.test(c);
}

/** `src` with comments and string/char literal contents blanked; newlines are kept. */
export function rustCodeOnly(src: string): string {
  const out: string[] = [];
  const blank = (from: number, to: number) => {
    for (let k = from; k < to; k++) out.push(src[k] === '\n' ? '\n' : ' ');
  };
  let i = 0;
  const n = src.length;
  while (i < n) {
    const c = src[i];
    const d = src[i + 1];
    if (c === '/' && d === '/') {
      const end = src.indexOf('\n', i);
      const stop = end === -1 ? n : end;
      blank(i, stop);
      i = stop;
      continue;
    }
    if (c === '/' && d === '*') {
      let depth = 0;
      let j = i;
      while (j < n) {
        if (src[j] === '/' && src[j + 1] === '*') {
          depth++;
          j += 2;
        } else if (src[j] === '*' && src[j + 1] === '/') {
          depth--;
          j += 2;
          if (depth === 0) break;
        } else {
          j++;
        }
      }
      blank(i, j);
      i = j;
      continue;
    }
    const prefixed = !isIdentChar(src[i - 1]);
    if (prefixed && (c === 'r' || ((c === 'b' || c === 'c') && d === 'r'))) {
      let j = c === 'r' ? i + 1 : i + 2;
      let hashes = 0;
      while (src[j] === '#') {
        hashes++;
        j++;
      }
      if (src[j] === '"') {
        const close = `"${'#'.repeat(hashes)}`;
        const end = src.indexOf(close, j + 1);
        if (end !== -1) {
          blank(i, end + close.length);
          i = end + close.length;
          continue;
        }
      }
    }
    if (c === '"') {
      let j = i + 1;
      while (j < n && src[j] !== '"') j += src[j] === '\\' ? 2 : 1;
      blank(i, Math.min(n, j + 1));
      i = j + 1;
      continue;
    }
    if (c === "'") {
      let j = -1;
      if (d === '\\') {
        const e = src[i + 2];
        const k =
          e === 'u' ? src.indexOf('}', i + 2) + 1 : e === 'x' ? i + 5 : i + 3;
        if (k > 0 && src[k] === "'") j = k;
      } else if (src[i + 2] === "'") {
        j = i + 2;
      }
      if (j !== -1) {
        blank(i, j + 1);
        i = j + 1;
        continue;
      }
    }
    out.push(c);
    i++;
  }
  return out.join('');
}

/** Throws with the file and line of the first construct that could read the build host. */
export function assertNoHostReads(file: string, src: string): void {
  const code = rustCodeOnly(src);
  const lineOf = (at: number) => code.slice(0, at).split('\n').length;
  const ident = BANNED_IDENT.exec(code);
  if (ident) {
    throw new Error(
      `${file}:${lineOf(ident.index)}: \`${ident[1]}\` is not allowed in platform builds (it can read the build host at compile time)`,
    );
  }
  for (const m of code.matchAll(ATTR_START)) {
    const open = m.index + m[0].length - 1;
    const body = code.slice(open, closing(code, open) + 1);
    const attr = BANNED_ATTR.exec(body);
    if (attr) {
      throw new Error(
        `${file}:${lineOf(m.index)}: the #[${attr[1]}] attribute is not allowed in platform builds`,
      );
    }
  }
  for (const m of code.matchAll(MACRO_RULES)) {
    const open = m.index + m[0].length - 1;
    const body = code.slice(open, closing(code, open) + 1);
    const decl = OUT_OF_LINE_MOD.exec(body);
    if (decl) {
      throw new Error(
        `${file}:${lineOf(open + decl.index)}: a macro may not declare an out-of-line module (\`mod name;\`); declare it outside the macro`,
      );
    }
  }
}
