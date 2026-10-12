import * as fs from 'node:fs';
import * as path from 'node:path';
import type { BuildKind, ExecCrateSource } from '../../checks/src/build/exec-source';
import { manifestValue } from './kit';

/** A crate on disk, as the platform would receive it. */
export interface LocalCrate {
  dir: string;
  /** `[package] name`: the crate name the platform builds it under. */
  name: string;
  /** The module cargo writes: `[lib] name`, or the package name with `-` as `_`. */
  libName: string;
  kind: BuildKind;
  /** What the platform takes: `Cargo.toml`, `README.md` and `.rs` files under `src/`. */
  source: ExecCrateSource;
  /** Everything else in the directory, which a platform build never sees. */
  notSent: string[];
}

const SKIP_DIRS = new Set(['target', '.git', 'node_modules']);

function walk(dir: string, rel: string, out: string[]): void {
  for (const entry of fs.readdirSync(path.join(dir, rel), { withFileTypes: true })) {
    const child = rel ? `${rel}/${entry.name}` : entry.name;
    if (entry.isDirectory()) {
      if (!SKIP_DIRS.has(entry.name)) walk(dir, child, out);
    } else if (entry.isFile()) {
      out.push(child);
    }
  }
}

function sent(rel: string): boolean {
  return rel === 'Cargo.toml' || rel === 'README.md' || (rel.startsWith('src/') && rel.endsWith('.rs'));
}

/** A CLIENT half is the crate that depends on `crowdy-client-sdk`. */
function detectKind(manifest: string): BuildKind {
  let section = '';
  for (const raw of manifest.split(/\r?\n/)) {
    const line = raw.replace(/#.*$/, '').trim();
    const header = line.match(/^\[([^\]]+)\]$/);
    if (header) section = header[1].trim();
    else if (section === 'dependencies' && /^crowdy-client-sdk\s*=/.test(line)) return 'client';
  }
  return 'exec';
}

export function readCrate(dir: string, kind?: BuildKind): LocalCrate {
  const root = path.resolve(dir);
  const manifestPath = path.join(root, 'Cargo.toml');
  if (!fs.existsSync(manifestPath)) throw new Error(`${root} has no Cargo.toml`);
  const all: string[] = [];
  walk(root, '', all);
  const files: Record<string, string> = {};
  const notSent: string[] = [];
  for (const rel of all.sort()) {
    if (sent(rel)) files[rel] = fs.readFileSync(path.join(root, rel), 'utf8');
    else notSent.push(rel);
  }
  const manifest = files['Cargo.toml'];
  const name = manifestValue(manifest, 'package', 'name');
  if (!name) throw new Error('Cargo.toml has no [package] name');
  const libName = manifestValue(manifest, 'lib', 'name') ?? name.replace(/-/g, '_');
  return {
    dir: root,
    name,
    libName,
    kind: kind ?? detectKind(manifest),
    source: { name, files },
    notSent,
  };
}
