#!/usr/bin/env node
// What `npm pack` would publish: everything the CLI reads at run time is in it, and nothing else
// is (no cargo output, no specs, no node_modules). Run after `npm run build`.
import { execFileSync } from 'node:child_process';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const [pack] = JSON.parse(execFileSync('npm', ['pack', '--dry-run', '--json'], { cwd: ROOT, encoding: 'utf8' }));
const files = pack.files.map((f) => f.path);

const REQUIRED = [
  'dist/cli/src/main.js',
  'dist/checks/src/build/exec-source.js',
  'dist/checks/src/build/client-capabilities.js',
  'crates/ckx-sdk/Cargo.toml',
  'crates/ckx-sdk/src/lib.rs',
  'crates/crowdy-client-sdk/src/lib.rs',
  'tools/instrument/Cargo.lock',
  'tools/instrument/src/main.rs',
  'starters/ckx.json',
  'starters/grid-mod/src/lib.rs',
  'templates/client/src/lib.rs',
  'templates/hub/src/lib.rs',
  'templates/spoke/src/lib.rs',
  'checks/compute-toolchain/host-catalog.json',
  'rust-toolchain.toml',
  'vendor.lock',
  'kit.pin',
  'LICENSE-MIT',
  'LICENSE-APACHE',
  'package.json',
];
const FORBIDDEN = [/(^|\/)target\//, /(^|\/)node_modules\//, /\.spec\.(ts|js)$/, /(^|\/)Cargo\.lock$/, /\.wasm$/];

const missing = REQUIRED.filter((f) => !files.includes(f));
const forbidden = files.filter((f) => FORBIDDEN.some((re) => re.test(f)) && f !== 'tools/instrument/Cargo.lock');
console.log(`[pack] ${pack.name}@${pack.version}: ${files.length} files, ${pack.size} bytes packed`);
for (const f of missing) console.error(`  MISSING ${f}`);
for (const f of forbidden) console.error(`  FORBIDDEN ${f}`);
if (missing.length || forbidden.length) process.exit(1);
console.log('[pack] passed.');
