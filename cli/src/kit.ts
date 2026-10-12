import * as fs from 'node:fs';
import * as path from 'node:path';

/** The package root: this file runs from `dist/cli/src/`. */
export const KIT_ROOT = path.resolve(__dirname, '..', '..', '..');

export function kitPath(...parts: string[]): string {
  return path.join(KIT_ROOT, ...parts);
}

/** `kit.pin`: the platform toolchain release this kit was copied from. */
export function readPin(): Record<string, string> {
  const pin: Record<string, string> = {};
  const file = kitPath('kit.pin');
  if (!fs.existsSync(file)) return pin;
  for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
    const m = line.match(/^([a-z_]+)=(.*)$/);
    if (m) pin[m[1]] = m[2];
  }
  return pin;
}

export function kitVersion(): string {
  return JSON.parse(fs.readFileSync(kitPath('package.json'), 'utf8')).version;
}

/** `[section] key = "value"` from a Cargo manifest; one-line values only, as the platform takes. */
export function manifestValue(manifest: string, section: string, key: string): string | null {
  let current = '';
  for (const raw of manifest.split(/\r?\n/)) {
    const line = raw.replace(/#.*$/, '').trim();
    const header = line.match(/^\[([^\]]+)\]$/);
    if (header) {
      current = header[1].trim();
      continue;
    }
    if (current !== section) continue;
    const kv = line.match(/^([A-Za-z0-9_-]+)\s*=\s*"([^"]*)"$/);
    if (kv && kv[1] === key) return kv[2];
  }
  return null;
}

export type Sdk = 'ckx-sdk' | 'crowdy-client-sdk';

/**
 * The version of an SDK this kit carries. A release stamps the version published for its tier
 * (`0.8.0-dev.1`, `0.8.0-test.1`, `0.8.0`); a checkout of the kit carries the bare one.
 */
export function sdkVersion(sdk: Sdk): string {
  const manifest = fs.readFileSync(kitPath('crates', sdk, 'Cargo.toml'), 'utf8');
  const version = manifestValue(manifest, 'package', 'version');
  if (!version) throw new Error(`crates/${sdk}/Cargo.toml has no version`);
  return version;
}

/** How a crate names this kit's SDK: a prerelease is chosen only by naming it exactly. */
export function sdkRequirement(sdk: Sdk): string {
  const version = sdkVersion(sdk);
  return version.includes('-') ? `=${version}` : version;
}
