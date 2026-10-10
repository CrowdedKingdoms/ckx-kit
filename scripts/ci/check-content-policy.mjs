// Public content policy: this repository, the crates and the npm package it publishes are
// public. Nothing in them may name a private repository, internal infrastructure, a builder
// machine or an operator credential. CI fails if a denylisted term appears anywhere in the
// corpus, and the release runs it again on exactly what it is about to upload.
//
// Adapted from crowdy-dsh's check: it walks the tree rather than asking git, so generated output
// (`dist/`, what npm ships) is policed too; it skips directories by name, each with its reason,
// and prints the corpus size so a clean tree can be told from an unopened one.
//
// Usage:
//   node scripts/ci/check-content-policy.mjs              # the repository root (dist/ when present)
//   node scripts/ci/check-content-policy.mjs <dir>        # another root: an unpacked package
//   node scripts/ci/check-content-policy.mjs --self-test  # prove the gate refuses a planted reference
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, extname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(here, '..', '..');

// Private repositories and the privileged wrapper, the realtime service's internals, and this
// platform's infrastructure. The public repositories may be named.
const DENYLIST = [
  ['cks', 'udp', 'api'].join('-'),
  'cks-michael-root',
  'cks-project-root',
  'cks-game-api',
  'cks-management-ui',
  'cks-platform-core',
  'infra-control-plane',
  'crowd-altar',
  'Crowdy-Games',
  'MessageType.hpp',
  'wire-protocol-reference',
  'P2P_SECRET',
  'P2P_TOKEN',
  'CHANNEL_MUTATION',
  'port 9081',
  ':9081',
  'buddydev',
  'BUDDY_BUILDER',
  'dev-run-buddy',
  '/home/ubuntu/',
  'tailscale',
  '.ts.net',
  'infra-cp',
  '317700178317',
  'secrets/.env',
  'CRATE_ADMIN',
  'GITHUB_PAT',
  'cks-env',
  'ck-exec-mgr',
  '-ck-api-',
  '/opt/cks',
];

// A term allowed in one file, for a stated reason and only until the reason goes away. Anything
// else that matches fails, in this file or any other.
const CODEOWNERS_HEADER =
  "the org's CODEOWNERS header, derived by the org's tooling for every three-branch repository (public ones carry it too); it is never published";
const EXCEPTIONS = [
  { file: '.github/CODEOWNERS', term: 'cks-game-api', reason: CODEOWNERS_HEADER },
  { file: '.github/CODEOWNERS', term: 'infra-control-plane', reason: CODEOWNERS_HEADER },
  {
    file: 'checks/src/build/exec-source.spec.ts',
    term: '/opt/cks',
    reason:
      "fixture paths in the platform's own spec, copied byte-identical; upstream moves them to neutral paths, and this entry goes with that sync",
  },
  {
    file: 'crates/ckx-sdk/src/cron.rs',
    term: 'cks-game-api',
    reason:
      'a test comment in ckx-sdk 0.8.0, byte-identical to the SDK the platform compiles; the SDK drops it in 0.9.0, and this entry goes with that sync',
  },
];

const SKIP_DIRS = new Map([
  ['node_modules', 'third-party dependencies; not our text to police'],
  ['target', 'cargo build output; never committed or published'],
  ['coverage', 'test-run output, regenerated per run'],
  ['.audit', 'security.yml scratch manifest'],
]);

// `.git` is a directory in a clone and a file in a worktree; skipped in both forms.
const SKIP_ANY = new Set(['.git']);

const SKIP_EXT = new Set(['.png', '.jpg', '.jpeg', '.gif', '.webp', '.ico', '.pdf', '.tgz', '.zip', '.gz', '.wasm']);

const SELF = relative(ROOT, fileURLToPath(import.meta.url));

function walk(dir, out) {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    if (SKIP_ANY.has(entry.name)) continue;
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      if (!SKIP_DIRS.has(entry.name)) walk(full, out);
    } else if (entry.isFile() && !SKIP_EXT.has(extname(entry.name).toLowerCase())) {
      out.push(full);
    }
  }
  return out;
}

export function scan(root) {
  const files = walk(root, []);
  const findings = [];
  let scanned = 0;
  for (const file of files) {
    const rel = relative(root, file).split('\\').join('/');
    if (root === ROOT && rel === SELF) continue;
    let text;
    try {
      text = readFileSync(file, 'utf8');
    } catch {
      continue;
    }
    if (text.includes('\u0000')) continue;
    scanned += 1;
    text.split('\n').forEach((line, i) => {
      for (const term of DENYLIST) {
        if (!line.includes(term)) continue;
        if (EXCEPTIONS.some((e) => e.file === rel && e.term === term)) continue;
        findings.push({ term, file: rel, line: i + 1, text: line.trim().slice(0, 160) });
      }
    });
  }
  return { corpus: files.length, scanned, findings, distPresent: files.some((f) => relative(root, f).startsWith('dist')) };
}

function report(root, label) {
  const { corpus, scanned, findings, distPresent } = scan(root);
  console.log(`[content-policy] ${label}: corpus ${corpus} files, ${scanned} scanned as text, ${DENYLIST.length} terms, ${EXCEPTIONS.length} exception(s)`);
  if (root === ROOT) {
    console.log(distPresent ? '[content-policy] dist/ present and INCLUDED' : '[content-policy] dist/ absent: run `npm run build` first to cover the published code');
  }
  for (const f of findings) console.error(`  DENYLISTED '${f.term}'  ${f.file}:${f.line}: ${f.text}`);
  if (findings.length) {
    console.error(`[content-policy] FAILED: ${findings.length} reference(s) to private repositories or internal infrastructure.`);
    return 1;
  }
  console.log('[content-policy] passed.');
  return 0;
}

function selfTest() {
  const fixture = mkdtempSync(join(tmpdir(), 'content-policy-'));
  let failures = 0;
  const check = (name, got, want) => {
    const ok = got === want;
    console.log(`  ${ok ? 'ok  ' : 'FAIL'} ${name}`);
    if (!ok) failures += 1;
  };
  try {
    mkdirSync(join(fixture, 'dist', 'cli'), { recursive: true });
    mkdirSync(join(fixture, 'crates', 'ckx-sdk', 'src'), { recursive: true });
    writeFileSync(join(fixture, 'README.md'), '# clean\n');
    check('a clean tree passes', scan(fixture).findings.length, 0);
    writeFileSync(join(fixture, 'dist', 'cli', 'main.js'), `// see ${DENYLIST[3]}/docs\n`);
    const planted = scan(fixture);
    check('a reference in dist/ (the published code) is refused', planted.findings.length > 0 ? 1 : 0, 1);
    const cron = EXCEPTIONS.find((e) => e.file.endsWith('cron.rs'));
    writeFileSync(join(fixture, 'crates', 'ckx-sdk', 'src', 'cron.rs'), `// ${cron.term}\n`);
    check('the stated exception passes in its own file', scan(fixture).findings.length, planted.findings.length);
    writeFileSync(join(fixture, 'crates', 'ckx-sdk', 'src', 'lib.rs'), `// ${cron.term}\n`);
    check('the same term in another file is refused', scan(fixture).findings.length, planted.findings.length + 1);
    writeFileSync(join(fixture, 'crates', 'ckx-sdk', 'src', 'cron.rs'), `// ${DENYLIST[0]}\n`);
    check('another term in the excepted file is refused', scan(fixture).findings.length, planted.findings.length + 2);
  } finally {
    rmSync(fixture, { recursive: true, force: true });
  }
  if (failures) {
    console.error(`[content-policy] self-test FAILED: ${failures} case(s)`);
    return 1;
  }
  console.log('[content-policy] self-test passed.');
  return 0;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const arg = process.argv[2];
  if (arg === '--self-test') process.exit(selfTest());
  const root = arg ? resolve(arg) : ROOT;
  process.exit(selfTest() || report(root, arg ? `ckx-kit (${arg})` : 'ckx-kit'));
}
