import { assertNoHostReads, rustCodeOnly } from './rust-source-guard';

const refused = (src: string) => () => assertNoHostReads('lib.rs', src);

describe('assertNoHostReads', () => {
  it('refuses the macros that read files or the environment at compile time', () => {
    expect(refused('const S: &str = include_str!("/etc/ck/api.env");')).toThrow(
      /include_str/,
    );
    expect(
      refused('static B: &[u8] = include_bytes!("/proc/1/environ");'),
    ).toThrow(/include_bytes/);
    expect(refused('include!("/etc/passwd");')).toThrow(/include/);
    expect(refused('const K: &str = env!("DATABASE_URL");')).toThrow(/env/);
    expect(refused('const K: Option<&str> = option_env!("X");')).toThrow(
      /option_env/,
    );
    expect(
      refused('core::arch::global_asm!(".incbin \\"/etc/passwd\\"");'),
    ).toThrow(/global_asm/);
  });

  it('refuses a renamed or passed-through macro, and raw identifiers', () => {
    expect(
      refused('use core::include_str as inc;\nconst S: &str = inc!("/x");'),
    ).toThrow(/include_str/);
    expect(
      refused(
        'macro_rules! m { ($i:ident) => { $i!("/x") } }\nm!(include_str);',
      ),
    ).toThrow(/include_str/);
    expect(refused('const S: &str = r#include_str!("/x");')).toThrow(
      /include_str/,
    );
  });

  it('refuses #[path] and #[link], including through cfg_attr', () => {
    expect(refused('#[path = "/etc/passwd"]\nmod leak;')).toThrow(/#\[path\]/);
    expect(
      refused('#[cfg_attr(all(), path = "/etc/passwd")]\nmod leak;'),
    ).toThrow(/#\[path\]/);
    expect(refused('#[cfg_attr(any(x = [1]), path = "/e")] mod l;')).toThrow(
      /#\[path\]/,
    );
    expect(
      refused(
        '#[link(name = "/etc/passwd", kind = "static")]\nunsafe extern "C" {}',
      ),
    ).toThrow(/#\[link\]/);
  });

  it('refuses an out-of-line module declared by a macro, where a metavariable could become #[path]', () => {
    expect(
      refused(
        'macro_rules! m { ($a:ident) => { #[$a = "/etc/passwd"] mod leak; } }\nm!(path);',
      ),
    ).toThrow(/out-of-line/);
    expect(
      refused(
        'macro_rules! m { ($(#[$x:meta])*) => { $(#[$x])* mod $name; } }',
      ),
    ).toThrow(/out-of-line/);
  });

  it('is not fooled by a quote in a char literal or a backslash in a raw string', () => {
    expect(
      refused(
        'let q = \'"\';\nconst X: &str = include_str!("/etc/passwd");\nlet y = "";',
      ),
    ).toThrow(/include_str/);
    expect(
      refused(
        'let a = r"\\";\nconst X: &str = include_str!("/etc/passwd");\nlet b = "";',
      ),
    ).toThrow(/include_str/);
    expect(refused("let e = '\\'';\nconst K: &str = env!(\"X\");")).toThrow(
      /env/,
    );
  });

  it('accepts ordinary guest code: a `path` binding, and the names in comments and strings', () => {
    const ok = [
      'fn find() -> Vec<(i32, i32)> {\n    let mut path = vec![(0, 0)];\n    path.push((1, 1));\n    path\n}',
      '// include_str! would read the host, so we do not\nfn f() {}',
      '/* env!("X") /* nested */ still a comment */\nfn g() {}',
      'const HELP: &str = "use env! and include_str! at your peril";',
      'const RAW: &str = r#"include_str!("x") "quoted""#;',
      "fn h<'a>(s: &'a str) -> char { let _ = s; 'x' }",
      'mod combat;\nmod world { pub fn tick() {} }',
      'macro_rules! point { ($x:expr, $y:expr) => { ($x, $y) } }',
    ];
    for (const src of ok) expect(refused(src)).not.toThrow();
  });

  it('reports the line of the offending construct', () => {
    expect(refused('fn a() {}\n\nconst K: &str = env!("X");')).toThrow(
      /^lib\.rs:3:/,
    );
  });
});

describe('rustCodeOnly', () => {
  it('blanks comments and literals and keeps newlines', () => {
    const out = rustCodeOnly('a // c\n"s\ntr" b /* x\ny */ c');
    expect(out.split('\n')).toHaveLength(4);
    expect(out.replace(/\s+/g, ' ').trim()).toBe('a b c');
  });
});
