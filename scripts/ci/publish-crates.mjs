#!/usr/bin/env node
/**
 * The two SDK crates for a `<tier>/vX.Y.Z` release of the kit.
 *
 * A crate's version in `crates/<crate>/Cargo.toml` is bare and moves only when the SDK does
 * (upstream, then a sync). Each tier publishes it under its own version: `X.Y.Z-dev.N` on dev,
 * `X.Y.Z-test.N` on test, plain `X.Y.Z` on prod, so a plain requirement never reaches a dev or
 * test build. crates.io accepts a version once, forever, so a release publishes a crate only
 * when its source (`src/`) or its dependencies differ from the newest version that tier already
 * has, as the next N; otherwise it reuses that version. On prod a changed source under a version
 * already published is refused: the SDK's version must move.
 *
 * Whatever it decides, it stamps the tier's version into the manifest, which the npm package
 * carries: `ckx-kit new` names that version.
 *
 *   node scripts/ci/publish-crates.mjs plan <tier>      # decides; needs_token=true|false
 *   node scripts/ci/publish-crates.mjs publish <tier>   # CARGO_REGISTRY_TOKEN when a crate is new
 */
import { execFileSync } from 'node:child_process';
import { appendFileSync, existsSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, relative, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..', '..');
const CRATES = ['ckx-sdk', 'crowdy-client-sdk'];
const TIERS = new Set(['dev', 'test', 'prod']);
const UA = { 'user-agent': 'ckx-kit-release (https://github.com/CrowdedKingdoms/ckx-kit)' };

function manifestPath(crate) {
  return join(ROOT, 'crates', crate, 'Cargo.toml');
}

function bareVersion(crate) {
  const m = readFileSync(manifestPath(crate), 'utf8').match(/^version = "([^"]+)"$/m);
  if (!m) throw new Error(`${crate}: no version in Cargo.toml`);
  const bare = m[1].replace(/-.*$/, '');
  if (!/^\d+\.\d+\.\d+$/.test(bare)) throw new Error(`${crate}: version '${m[1]}' is not MAJOR.MINOR.PATCH`);
  return bare;
}

function stamp(crate, version) {
  const file = manifestPath(crate);
  writeFileSync(file, readFileSync(file, 'utf8').replace(/^version = "[^"]+"$/m, `version = "${version}"`));
}

function dependencies(manifest) {
  const out = [];
  let inDeps = false;
  for (const line of manifest.split('\n')) {
    if (/^\[/.test(line)) inDeps = line.trim() === '[dependencies]';
    else if (inDeps && line.trim()) out.push(line.trim());
  }
  return out.join('\n');
}

function sources(dir) {
  const files = {};
  const walk = (rel) => {
    for (const e of readdirSync(join(dir, rel), { withFileTypes: true })) {
      const child = rel ? `${rel}/${e.name}` : e.name;
      if (e.isDirectory()) walk(child);
      else files[child] = readFileSync(join(dir, child), 'utf8');
    }
  };
  walk('src');
  return files;
}

async function publishedVersions(crate) {
  const versions = [];
  let url = `https://crates.io/api/v1/crates/${crate}/versions?per_page=100`;
  while (url) {
    const res = await fetch(url, { headers: UA });
    if (res.status === 404) return [];
    if (!res.ok) throw new Error(`${crate}: crates.io answered ${res.status}; refusing to guess what is published`);
    const body = await res.json();
    versions.push(...body.versions.map((v) => v.num));
    const next = body.meta?.next_page;
    url = next ? `https://crates.io/api/v1/crates/${crate}/versions${next}` : null;
  }
  return versions;
}

/** The published crate's `src/` and dependencies, from its `.crate` file. */
async function publishedSource(crate, version) {
  const res = await fetch(`https://crates.io/api/v1/crates/${crate}/${version}/download`, { headers: UA, redirect: 'follow' });
  if (!res.ok) throw new Error(`${crate} ${version}: download answered ${res.status}`);
  const dir = mkdtempSync(join(tmpdir(), `${crate}-`));
  try {
    const archive = join(dir, 'crate.tar.gz');
    writeFileSync(archive, Buffer.from(await res.arrayBuffer()));
    execFileSync('tar', ['-xzf', archive, '-C', dir]);
    const root = join(dir, `${crate}-${version}`);
    return { src: sources(root), deps: dependencies(readFileSync(join(root, 'Cargo.toml.orig'), 'utf8')) };
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

function same(a, b) {
  const ka = Object.keys(a.src).sort();
  const kb = Object.keys(b.src).sort();
  return a.deps === b.deps && ka.length === kb.length && ka.every((k, i) => k === kb[i] && a.src[k] === b.src[k]);
}

async function decide(crate, tier) {
  const bare = bareVersion(crate);
  const local = { src: sources(join(ROOT, 'crates', crate)), deps: dependencies(readFileSync(manifestPath(crate), 'utf8')) };
  const versions = await publishedVersions(crate);
  if (tier === 'prod') {
    if (!versions.includes(bare)) return { crate, version: bare, publish: true };
    if (same(local, await publishedSource(crate, bare))) return { crate, version: bare, publish: false };
    throw new Error(`${crate} ${bare} is already on crates.io with other source; raise the SDK's version upstream`);
  }
  const pattern = new RegExp(`^${bare.replace(/\./g, '\\.')}-${tier}\\.(\\d+)$`);
  let highest = 0;
  for (const v of versions) {
    const m = pattern.exec(v);
    if (m) highest = Math.max(highest, Number(m[1]));
  }
  if (highest === 0) return { crate, version: `${bare}-${tier}.1`, publish: true };
  const latest = `${bare}-${tier}.${highest}`;
  if (same(local, await publishedSource(crate, latest))) return { crate, version: latest, publish: false };
  return { crate, version: `${bare}-${tier}.${highest + 1}`, publish: true };
}

async function main() {
  const [mode, tier] = process.argv.slice(2);
  if (!['plan', 'publish'].includes(mode) || !TIERS.has(tier)) {
    console.error('usage: publish-crates.mjs plan|publish dev|test|prod');
    process.exit(2);
  }
  const plans = [];
  for (const crate of CRATES) plans.push(await decide(crate, tier));
  for (const p of plans) console.log(`${p.crate}: ${p.publish ? 'publish' : 'reuse'} ${p.version}`);
  const needsToken = plans.some((p) => p.publish);
  if (mode === 'plan') {
    if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `needs_token=${needsToken}\n`);
    return;
  }
  if (needsToken && !process.env.CARGO_REGISTRY_TOKEN) throw new Error('a crate needs publishing and CARGO_REGISTRY_TOKEN is not set');
  for (const p of plans) {
    stamp(p.crate, p.version);
    if (!p.publish) continue;
    execFileSync('cargo', ['publish', '--allow-dirty', '--manifest-path', manifestPath(p.crate)], { stdio: 'inherit' });
    console.log(`published ${p.crate} ${p.version}`);
  }
  const summary = plans.map((p) => `${p.crate}=${p.version}`).join(' ');
  if (process.env.GITHUB_OUTPUT) appendFileSync(process.env.GITHUB_OUTPUT, `crate_versions=${summary}\n`);
  console.log(`crate versions for ${tier}: ${summary}`);
  if (existsSync(join(ROOT, 'Cargo.lock'))) console.warn(`stray ${relative(ROOT, join(ROOT, 'Cargo.lock'))}`);
}

main().catch((e) => {
  console.error(`::error::${e.message}`);
  process.exit(1);
});
