// The CLI end to end: every starter and template builds and passes the platform's checks, and
// crates the platform refuses are refused with its messages. Needs `npm run build`, Rust with
// the wasm32-unknown-unknown target, and wasm-opt.
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { cpSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const CLI = join(ROOT, 'dist', 'cli', 'src', 'main.js');
const scratch = mkdtempSync(join(tmpdir(), 'ckx-kit-test-'));
const env = {
  ...process.env,
  CKX_KIT_TARGET_DIR: process.env.CKX_KIT_TARGET_DIR ?? join(scratch, 'target'),
  CKX_KIT_CACHE: process.env.CKX_KIT_CACHE ?? join(scratch, 'cache'),
};

function kit(args) {
  return spawnSync(process.execPath, [CLI, ...args], { cwd: scratch, encoding: 'utf8', env });
}

function built(args) {
  const r = kit([...args, '--json']);
  assert.equal(r.status, 0, r.stderr.slice(-4000));
  return JSON.parse(r.stdout);
}

const starters = readdirSync(join(ROOT, 'starters'), { withFileTypes: true })
  .filter((e) => e.isDirectory())
  .map((e) => e.name);

for (const starter of starters) {
  test(`the ${starter} starter builds as the platform builds it`, () => {
    const dir = join(scratch, `starter-${starter}`);
    cpSync(join(ROOT, 'starters', starter), dir, { recursive: true });
    const report = built(['build', dir, ...(starter === 'grid-mod' ? ['--mod'] : [])]);
    assert.equal(report.kind, 'exec');
    assert.ok(report.sizeBytes > 0);
    assert.match(report.sha256, /^[0-9a-f]{64}$/);
  });
}

for (const [template, flags] of [
  ['mod', ['--mod']],
  ['hub', []],
  ['spoke', []],
  ['client', ['--client']],
]) {
  test(`new ${template} names this kit's SDK and builds`, () => {
    const name = `t-${template}`;
    const created = kit(['new', template, name]);
    assert.equal(created.status, 0, created.stderr);
    const manifest = readFileSync(join(scratch, name, 'Cargo.toml'), 'utf8');
    const sdk = template === 'client' ? 'crowdy-client-sdk' : 'ckx-sdk';
    const version = /^version = "([^"]+)"/m.exec(readFileSync(join(ROOT, 'crates', sdk, 'Cargo.toml'), 'utf8'))[1];
    assert.ok(manifest.includes(`${sdk} = "${version.includes('-') ? '=' : ''}${version}"`), manifest);
    assert.match(manifest, new RegExp(`^name = "${name}"$`, 'm'));
    const report = built(['build', join(scratch, name), ...flags]);
    if (template === 'client') {
      assert.equal(report.kind, 'client');
      assert.match(report.capabilityHash, /^[0-9a-f]{64}$/);
      assert.ok(report.capabilitySummary.exportedFunctions.includes('tick'));
      assert.equal(report.tickIntervalMs, 1000);
    } else {
      assert.equal(report.kind, 'exec');
    }
    assert.deepEqual(report.notSent, ['.gitignore', 'rust-toolchain.toml']);
  });
}

// A new crate's own tests, natively, against the SDK this kit carries (an unreleased kit names a
// version crates.io does not have yet; the release's smoke job tests the published one).
for (const template of ['mod', 'hub', 'spoke', 'client']) {
  test(`cargo test passes in a new ${template}`, () => {
    const name = `u-${template}`;
    assert.equal(kit(['new', template, name]).status, 0);
    const sdk = template === 'client' ? 'crowdy-client-sdk' : 'ckx-sdk';
    const r = spawnSync('cargo', ['test', '--config', `patch.crates-io.${sdk}.path="${join(ROOT, 'crates', sdk)}"`], {
      cwd: join(scratch, name),
      encoding: 'utf8',
      env: { ...env, CARGO_TARGET_DIR: join(scratch, 'native-target') },
    });
    assert.equal(r.status, 0, (r.stdout + r.stderr).slice(-4000));
  });
}

function crate(name, files) {
  const dir = join(scratch, name);
  mkdirSync(join(dir, 'src'), { recursive: true });
  const all = {
    'Cargo.toml': `[package]\nname = "${name}"\nversion = "0.1.0"\nedition = "2024"\n\n[lib]\ncrate-type = ["cdylib"]\n\n[dependencies]\nckx-sdk = "0.8.0"\n`,
    'src/lib.rs': 'pub fn ok() {}\n',
    ...files,
  };
  for (const [rel, content] of Object.entries(all)) {
    mkdirSync(dirname(join(dir, rel)), { recursive: true });
    writeFileSync(join(dir, rel), content);
  }
  return dir;
}

const manifest = (extra) =>
  `[package]\nname = "refused"\nversion = "0.1.0"\nedition = "2024"\n\n[lib]\ncrate-type = ["cdylib"]\n\n${extra}`;

for (const [what, files, refusal] of [
  ['a dependency off the allowlist', { 'Cargo.toml': manifest('[dependencies]\nckx-sdk = "0.8.0"\nrand = "0.8"\n') }, /dependency 'rand' is not in the allowlist/],
  ['include_str!', { 'src/lib.rs': 'pub const X: &str = include_str!("/etc/hostname");\n' }, /`include_str` is not allowed in platform builds/],
  ['a [profile] section', { 'Cargo.toml': manifest('[dependencies]\nckx-sdk = "0.8.0"\n\n[profile.release]\nopt-level = 0\n') }, /section \[profile\.release\] is not allowed/],
  ['a #[path] attribute', { 'src/lib.rs': '#[path = "/tmp/x.rs"]\nmod x;\n' }, /the #\[path\] attribute is not allowed in platform builds/],
  ['a build script', { 'Cargo.toml': manifest('[dependencies]\nckx-sdk = "0.8.0"\n').replace('edition = "2024"\n', 'edition = "2024"\nbuild = "build.rs"\n') }, /\[package\] build is not allowed/],
]) {
  test(`check refuses ${what}, as the platform does`, () => {
    const dir = crate(`refused-${what.replace(/[^a-z]+/g, '-').replace(/^-|-$/g, '')}`, files);
    const r = kit(['check', dir]);
    assert.equal(r.status, 1, r.stdout);
    assert.match(r.stderr, refusal);
  });
}

test('check names the files a platform build never sees', () => {
  const dir = crate('extra-files', { 'tests/it.rs': '#[test] fn t() {}\n', 'Cargo.lock': '' });
  const r = kit(['check', dir]);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /not sent to the platform: Cargo\.lock, tests\/it\.rs/);
});
