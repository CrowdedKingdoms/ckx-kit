/**
 * What a ck-exec build may contain, and how its manifest is rewritten for the toolchain.
 *
 * A build is one or more crates, each a `ckx-sdk` library built as a `cdylib` for
 * `wasm32-unknown-unknown`: one module per crate, which the app's manifest names by digest.
 * A CLIENT build is the browser half of a player's mod instead: one `crowdy-client-sdk` crate.
 * Cargo runs build scripts and proc-macros at compile time, so every dependency is one the
 * platform vendored and reviewed, and nothing may add another.
 */

import { assertNoHostReads } from './rust-source-guard';

/** `exec`: ck-exec modules. `client`: the CLIENT half of a mod, which runs in browsers. */
export type BuildKind = 'exec' | 'client';

/** Dependencies a ck-exec crate may declare (the toolchain vendors them and their deps). */
export const EXEC_ALLOWED_CRATES: ReadonlySet<string> = new Set([
  'ckx-sdk',
  'serde',
  'serde_json',
]);

/** Dependencies a CLIENT crate may declare: the client SDK and the same serde crates. */
export const CLIENT_ALLOWED_CRATES: ReadonlySet<string> = new Set([
  'crowdy-client-sdk',
  'serde',
  'serde_json',
]);

/** The SDK each kind builds against; its dependency line is pointed at the toolchain's copy. */
export const BUILD_SDK: Readonly<Record<BuildKind, string>> = {
  exec: 'ckx-sdk',
  client: 'crowdy-client-sdk',
};

const ALLOWED_CRATES: Readonly<Record<BuildKind, ReadonlySet<string>>> = {
  exec: EXEC_ALLOWED_CRATES,
  client: CLIENT_ALLOWED_CRATES,
};

/**
 * The browser tick Crowdy Studio reads from a CLIENT crate, `[package.metadata.crowdy]
 * tick_interval_ms`: 1000 ms when absent, clamped to 16-1000 ms.
 */
export const CLIENT_TICK_INTERVAL_MS = { default: 1_000, min: 16, max: 1_000 };
const CLIENT_METADATA_SECTION = 'package.metadata.crowdy';

/** Manifest sections a crate may contain; everything else (build scripts, patches) is refused. */
export const EXEC_ALLOWED_SECTIONS: ReadonlySet<string> = new Set([
  'package',
  'lib',
  'dependencies',
]);

export const EXEC_MAX_CRATES = 16;
export const EXEC_MAX_FILES_PER_CRATE = 64;
export const EXEC_MAX_SOURCE_BYTES = 2 * 1024 * 1024;

const CRATE_NAME = /^[a-z][a-z0-9_-]{0,63}$/;
const SOURCE_PATH = /^[A-Za-z0-9_\-./]+$/;
const TOP_FILES: ReadonlySet<string> = new Set(['Cargo.toml', 'README.md']);

export interface ExecCrateSource {
  name: string;
  files: Record<string, string>;
}

function fail(message: string): never {
  throw new Error(message);
}

/** A crate file path: `Cargo.toml`, `README.md`, or a Rust file under `src/`. */
export function validateExecPath(path: string): void {
  if (!SOURCE_PATH.test(path) || path.length > 200)
    fail(`bad source path '${path}'`);
  const parts = path.split('/');
  if (parts.some((p) => p === '' || p === '.' || p === '..'))
    fail(`bad source path '${path}'`);
  if (TOP_FILES.has(path)) return;
  if (parts[0] !== 'src' || !path.endsWith('.rs')) {
    fail(`'${path}' is not Cargo.toml, README.md or a .rs file under src/`);
  }
}

function stripComment(line: string): string {
  let inString = false;
  for (let i = 0; i < line.length; i++) {
    const c = line[i];
    if (c === '"' && line[i - 1] !== '\\') inString = !inString;
    if (c === '#' && !inString) return line.slice(0, i);
  }
  return line;
}

/**
 * Keys each section may hold. Anything else is refused: several Cargo keys name a file on
 * the build host (`[lib] path`, `[package] workspace`, `readme`, `build`).
 */
const PACKAGE_KEYS: ReadonlySet<string> = new Set([
  'name',
  'version',
  'edition',
  'description',
  'publish',
  'license',
  'authors',
  'rust-version',
]);
const LIB_KEYS: ReadonlySet<string> = new Set(['name', 'crate-type']);
const DEPENDENCY_KEYS: ReadonlySet<string> = new Set([
  'version',
  'features',
  'default-features',
]);

/**
 * The keys of a one-line inline table's body, split at its top-level commas; null unless every
 * entry is a BARE key and a value. A quoted key (`"path" = …`) is the same key to cargo, so a
 * check that reads only bare words before `=` would let it through.
 */
function inlineTableKeys(body: string): string[] | null {
  const entries: string[] = [];
  let depth = 0;
  let quote: string | null = null;
  let start = 0;
  for (let i = 0; i < body.length; i++) {
    const c = body[i];
    if (quote) {
      if (c === '\\' && quote === '"') i++;
      else if (c === quote) quote = null;
      continue;
    }
    if (c === '"' || c === "'") quote = c;
    else if (c === '[' || c === '{') depth++;
    else if (c === ']' || c === '}') depth--;
    else if (c === ',' && depth === 0) {
      entries.push(body.slice(start, i));
      start = i + 1;
    }
    if (depth < 0) return null;
  }
  if (quote || depth !== 0) return null;
  entries.push(body.slice(start));
  if (entries.length === 1 && entries[0].trim() === '') return [];
  const keys: string[] = [];
  for (const entry of entries) {
    const kv = entry.match(/^\s*([A-Za-z0-9_-]+)\s*=\s*(.*?)\s*$/);
    if (!kv || !DEPENDENCY_VALUE.test(kv[2])) return null;
    keys.push(kv[1]);
  }
  return keys;
}

const TOML_STRING = `(?:"(?:[^"\\\\]|\\\\.)*"|'[^']*')`;
/** One value of a dependency table: a string, a boolean, or an array of strings. */
const DEPENDENCY_VALUE = new RegExp(
  `^(?:${TOML_STRING}|true|false|\\[\\s*(?:${TOML_STRING}\\s*(?:,\\s*${TOML_STRING}\\s*)*,?)?\\s*\\])$`,
);

/**
 * The crate's Cargo.toml: only [package], [lib] as a cdylib, and [dependencies] from the
 * allowlist, one key per line. The SDK's line is replaced by the toolchain's copy, so its
 * value does not matter; the others are a version or `{ version, features,
 * default-features }` from the vendored registry. A CLIENT crate may also carry
 * `[package.metadata.crowdy]` with `tick_interval_ms`, which cargo ignores.
 */
export function validateExecManifest(
  manifest: string,
  kind: BuildKind = 'exec',
): void {
  const allowed = ALLOWED_CRATES[kind];
  const sdk = BUILD_SDK[kind];
  let section = '';
  let cdylib = false;
  for (const raw of manifest.split(/\r?\n/)) {
    const line = stripComment(raw).trim();
    if (!line) continue;
    const header = line.match(/^\[([^\]]+)\]$/);
    if (header) {
      section = header[1].trim();
      const metadata = kind === 'client' && section === CLIENT_METADATA_SECTION;
      if (!EXEC_ALLOWED_SECTIONS.has(section) && !metadata) {
        fail(
          `Cargo.toml section [${section}] is not allowed (only ${[...EXEC_ALLOWED_SECTIONS].join(', ')}${kind === 'client' ? `, ${CLIENT_METADATA_SECTION}` : ''})`,
        );
      }
      continue;
    }
    const kv = line.match(/^([A-Za-z0-9_-]+)\s*=\s*(.+)$/);
    if (!kv) fail(`Cargo.toml line '${line}' is not a single-line key = value`);
    const [, key, value] = kv;
    if (section === '')
      fail(`Cargo.toml key '${key}' must be inside a section`);
    if (section === 'package' && !PACKAGE_KEYS.has(key)) {
      fail(
        `Cargo.toml [package] ${key} is not allowed (only ${[...PACKAGE_KEYS].join(', ')})`,
      );
    }
    if (section === 'lib') {
      if (!LIB_KEYS.has(key)) {
        fail(
          `Cargo.toml [lib] ${key} is not allowed (only ${[...LIB_KEYS].join(', ')})`,
        );
      }
      if (key === 'crate-type') {
        if (!/^\[\s*"cdylib"\s*(,\s*"rlib"\s*)?\]$/.test(value))
          fail('[lib] crate-type must be ["cdylib"]');
        cdylib = true;
      }
    }
    if (section === CLIENT_METADATA_SECTION) {
      if (key !== 'tick_interval_ms') {
        fail(
          `Cargo.toml [${CLIENT_METADATA_SECTION}] ${key} is not allowed (only tick_interval_ms)`,
        );
      }
      if (!/^\d{1,6}$/.test(value)) {
        fail('tick_interval_ms must be a whole number of milliseconds');
      }
    }
    if (section === 'dependencies') {
      if (!allowed.has(key)) {
        fail(
          `Cargo.toml dependency '${key}' is not in the allowlist (${[...allowed].join(', ')})`,
        );
      }
      if (key !== sdk && !/^"[^"]*"$/.test(value)) {
        const table = value.match(/^\{(.*)\}$/);
        const keys = table ? inlineTableKeys(table[1]) : null;
        if (!keys) {
          fail(
            `dependency '${key}' must be a version or a one-line { version, features } table with bare keys`,
          );
        }
        for (const k of keys) {
          if (!DEPENDENCY_KEYS.has(k)) {
            fail(
              `dependency '${key}' may not set '${k}': only a version from the vendored registry, features and default-features`,
            );
          }
        }
      }
    }
  }
  if (!cdylib) fail('Cargo.toml needs [lib] crate-type = ["cdylib"]');
}

/** Every crate of a build: names, paths, sizes, manifests. */
export function validateExecCrates(
  crates: ExecCrateSource[],
  kind: BuildKind = 'exec',
): void {
  if (crates.length === 0) fail('a build names at least one crate');
  if (kind === 'client' && crates.length !== 1)
    fail('a CLIENT half is built from exactly one crate');
  if (crates.length > EXEC_MAX_CRATES)
    fail(`a build has at most ${EXEC_MAX_CRATES} crates`);
  const names = new Set<string>();
  let total = 0;
  for (const c of crates) {
    if (!CRATE_NAME.test(c.name))
      fail(
        `crate name '${c.name}' must be lowercase letters, digits, '-' or '_'`,
      );
    if (names.has(c.name)) fail(`crate '${c.name}' appears twice`);
    names.add(c.name);
    const paths = Object.keys(c.files);
    if (paths.length > EXEC_MAX_FILES_PER_CRATE)
      fail(`crate '${c.name}' has more than ${EXEC_MAX_FILES_PER_CRATE} files`);
    for (const p of paths) {
      validateExecPath(p);
      if (p.endsWith('.rs')) assertNoHostReads(`${c.name}/${p}`, c.files[p]);
      total += Buffer.byteLength(c.files[p], 'utf8');
    }
    if (!('Cargo.toml' in c.files)) fail(`crate '${c.name}' has no Cargo.toml`);
    if (!('src/lib.rs' in c.files)) fail(`crate '${c.name}' has no src/lib.rs`);
    validateExecManifest(c.files['Cargo.toml'], kind);
  }
  if (total > EXEC_MAX_SOURCE_BYTES)
    fail(`a build's sources are at most ${EXEC_MAX_SOURCE_BYTES} bytes`);
}

/**
 * The file name cargo gives a validated crate's module: its `[lib] name`, else its package
 * name with `-` as `_`, then `.wasm`. A build's crates share one target directory, so the
 * module has to be found by name rather than as the only `.wasm` there.
 */
export function execArtifactName(manifest: string): string {
  let section = '';
  let pkg: string | null = null;
  let lib: string | null = null;
  for (const raw of manifest.split(/\r?\n/)) {
    const line = stripComment(raw).trim();
    const header = line.match(/^\[([^\]]+)\]$/);
    if (header) {
      section = header[1].trim();
      continue;
    }
    const kv = line.match(/^name\s*=\s*"([^"]+)"$/);
    if (!kv) continue;
    if (section === 'package') pkg = kv[1];
    if (section === 'lib') lib = kv[1];
  }
  const name = lib ?? pkg;
  if (!name) fail('Cargo.toml has no [package] name');
  // The name becomes a file name under the build's target directory: a cargo identifier and
  // nothing a path could be made of, before cargo itself would refuse it.
  if (!/^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$/.test(name))
    fail(`Cargo.toml name '${name}' is not a crate name`);
  return `${name.replace(/-/g, '_')}.wasm`;
}

/**
 * Points the kind's SDK at the toolchain's pinned copy and appends the platform release
 * profile (a crate may not declare its own; the validator refuses [profile]).
 */
export function rewriteExecManifest(
  manifest: string,
  sdkPath: string,
  kind: BuildKind = 'exec',
): string {
  const sdk = BUILD_SDK[kind];
  const line = new RegExp(`^\\s*${sdk}\\s*=`);
  const lines = manifest
    .split(/\r?\n/)
    .map((l) => (line.test(l) ? `${sdk} = { path = "${sdkPath}" }` : l));
  return `${lines.join('\n')}\n\n[profile.release]\nopt-level = 3\nlto = true\ncodegen-units = 1\npanic = "abort"\nstrip = "symbols"\n`;
}

/** A validated CLIENT manifest's `tick_interval_ms`, clamped as Crowdy Studio clamps it. */
export function clientTickIntervalMs(manifest: string): number {
  let section = '';
  for (const raw of manifest.split(/\r?\n/)) {
    const line = stripComment(raw).trim();
    const header = line.match(/^\[([^\]]+)\]$/);
    if (header) {
      section = header[1].trim();
      continue;
    }
    const kv = line.match(/^tick_interval_ms\s*=\s*(\d+)$/);
    if (section === CLIENT_METADATA_SECTION && kv && Number(kv[1]) > 0) {
      return Math.min(
        CLIENT_TICK_INTERVAL_MS.max,
        Math.max(CLIENT_TICK_INTERVAL_MS.min, Number(kv[1])),
      );
    }
  }
  return CLIENT_TICK_INTERVAL_MS.default;
}

/** The guest ABI's imports (`ckx` module), from ck-exec's docs/GUEST-ABI.md. */
export const EXEC_ABI_IMPORTS: ReadonlySet<string> = new Set([
  'log',
  'now_ms',
  'call',
  'send',
  'publish',
  'node',
  'wake_at',
  'subscribe',
  'unsubscribe',
  'persist_now',
  'emit',
]);

/** Exports every ck-exec module has. */
export const EXEC_ABI_EXPORTS: ReadonlyArray<
  readonly [string, 'function' | 'memory']
> = [
  ['memory', 'memory'],
  ['ckx_abi', 'function'],
  ['ckx_alloc', 'function'],
  ['ckx_init', 'function'],
  ['ckx_handle', 'function'],
  ['ckx_persist', 'function'],
  ['ckx_evict', 'function'],
];

/** The largest module the execution manager accepts (ck-exec `bounds::ARTIFACT_MAX_BYTES`). */
export const EXEC_ARTIFACT_MAX_BYTES = 16 * 1024 * 1024;

/**
 * A built module fit for ck-exec: compiles, imports nothing but the guest ABI, exports its
 * entry points, and fits the manager's limit.
 */
export async function validateExecArtifact(bytes: Uint8Array): Promise<void> {
  if (bytes.length > EXEC_ARTIFACT_MAX_BYTES)
    fail(
      `module is ${bytes.length} bytes; the limit is ${EXEC_ARTIFACT_MAX_BYTES}`,
    );
  let module: WebAssembly.Module;
  try {
    module = await WebAssembly.compile(bytes);
  } catch (err) {
    fail(`malformed WASM module: ${(err as Error).message}`);
  }
  for (const imp of WebAssembly.Module.imports(module)) {
    if (
      imp.module !== 'ckx' ||
      imp.kind !== 'function' ||
      !EXEC_ABI_IMPORTS.has(imp.name)
    ) {
      fail(
        `module import ${imp.module}::${imp.name} (${imp.kind}) is not part of the ck-exec guest ABI`,
      );
    }
  }
  const exported = new Map(
    WebAssembly.Module.exports(module).map((e) => [e.name, e.kind]),
  );
  for (const [name, kind] of EXEC_ABI_EXPORTS) {
    if (exported.get(name) !== kind)
      fail(
        `module does not export ${kind} '${name}'; build it with ckx-sdk's export macro`,
      );
  }
}
