import { spawnSync, type SpawnSyncOptions } from 'node:child_process';
import { createHash } from 'node:crypto';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { kitPath } from './kit';

/** A program on PATH, resolved; null when it is not installed. */
export function which(cmd: string): string | null {
  for (const dir of (process.env.PATH ?? '').split(path.delimiter).filter(Boolean)) {
    const candidate = path.join(dir, cmd);
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return candidate;
    } catch {
      // not in this directory
    }
  }
  return null;
}

/**
 * Runs a step. Its output goes to stderr, so `--json` keeps stdout for the report; a step that
 * fails throws with the step's name.
 */
export function run(label: string, cmd: string, args: string[], options: SpawnSyncOptions = {}): void {
  const result = spawnSync(cmd, args, { stdio: ['ignore', process.stderr, process.stderr], ...options });
  if (result.error) {
    throw new Error(`${label}: ${(result.error as NodeJS.ErrnoException).code === 'ENOENT' ? `${cmd} is not installed` : result.error.message}`);
  }
  if (result.status !== 0) throw new Error(`${label} failed (exit ${result.status ?? result.signal})`);
}

export function cargo(): string {
  const bin = which('cargo');
  if (!bin) throw new Error('cargo is not installed: install Rust with rustup (https://rustup.rs)');
  return bin;
}

export function cacheDir(): string {
  return (
    process.env.CKX_KIT_CACHE ??
    path.join(process.env.XDG_CACHE_HOME ?? path.join(os.homedir(), '.cache'), 'ckx-kit')
  );
}

function copyTree(from: string, to: string): void {
  fs.mkdirSync(to, { recursive: true });
  for (const entry of fs.readdirSync(from, { withFileTypes: true })) {
    if (entry.name === 'target') continue;
    const src = path.join(from, entry.name);
    const dst = path.join(to, entry.name);
    if (entry.isDirectory()) copyTree(src, dst);
    else fs.copyFileSync(src, dst);
  }
}

function treeHash(dir: string, h = createHash('sha256'), rel = ''): ReturnType<typeof createHash> {
  for (const entry of fs.readdirSync(path.join(dir, rel), { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
    if (entry.name === 'target') continue;
    const child = rel ? `${rel}/${entry.name}` : entry.name;
    if (entry.isDirectory()) treeHash(dir, h, child);
    else h.update(child).update('\0').update(fs.readFileSync(path.join(dir, child))).update('\0');
  }
  return h;
}

/**
 * The platform's `instrument`, which meters a CLIENT half: built from the kit's copy with the
 * pinned Rust and its lock on first use, and cached by the copy's contents.
 */
export function instrumentBin(): string {
  const source = kitPath('tools', 'instrument');
  const key = treeHash(source).update(fs.readFileSync(kitPath('rust-toolchain.toml'))).digest('hex').slice(0, 16);
  const dir = path.join(cacheDir(), `instrument-${key}`);
  const bin = path.join(dir, 'target', 'release', process.platform === 'win32' ? 'instrument.exe' : 'instrument');
  if (fs.existsSync(bin)) return bin;
  copyTree(source, dir);
  fs.copyFileSync(kitPath('rust-toolchain.toml'), path.join(dir, 'rust-toolchain.toml'));
  process.stderr.write('== instrument (first use: building the platform\'s metering tool)\n');
  run('cargo build (instrument)', cargo(), ['build', '--release', '--locked'], { cwd: dir });
  if (!fs.existsSync(bin)) throw new Error(`instrument did not build at ${bin}`);
  return bin;
}

export function wasmOpt(): string {
  const bin = which('wasm-opt');
  if (!bin) throw new Error('wasm-opt is not installed: install binaryen (apt install binaryen, brew install binaryen)');
  return bin;
}

export function sha256(bytes: Uint8Array): string {
  return createHash('sha256').update(bytes).digest('hex');
}
