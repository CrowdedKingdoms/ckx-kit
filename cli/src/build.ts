import * as fs from 'node:fs';
import * as path from 'node:path';
import {
  canonicalJson,
  capabilityHash,
  deriveClientCapabilities,
  readHostCatalog,
  type ClientCapabilitySummary,
} from '../../checks/src/build/client-capabilities';
import {
  BUILD_SDK,
  clientTickIntervalMs,
  rewriteExecManifest,
  validateExecArtifact,
  validateExecCrates,
} from '../../checks/src/build/exec-source';
import { CLIENT_MAX_MEMORY_MB, validateClientArtifact, WASM_OPT_ARGS } from '../../checks/src/build/wasm-inspect';
import type { LocalCrate } from './crate';
import { kitPath, kitVersion, readPin, sdkVersion, type Sdk } from './kit';
import { cargo, instrumentBin, run, sha256, wasmOpt } from './tools';

const TARGET = 'wasm32-unknown-unknown';

/** The largest module a mod may deploy, the platform's mod limit (4 MiB). */
export const MOD_ARTIFACT_MAX_BYTES = 4 * 1024 * 1024;

export interface BuildReport {
  crate: string;
  kind: 'exec' | 'client';
  mod: boolean;
  wasm: string;
  sizeBytes: number;
  sha256: string;
  notSent: string[];
  capabilitySummary?: ClientCapabilitySummary;
  capabilityHash?: string;
  tickIntervalMs?: number;
  kit: { version: string; toolchain: string; sdk: string; sdkVersion: string; rust: string };
}

/** The platform's source checks, on the files it would receive. Throws its refusal. */
export function check(crate: LocalCrate): void {
  validateExecCrates([crate.source], crate.kind);
}

/** `execBuild`'s steps for one crate, without the platform's sandbox. */
export async function build(crate: LocalCrate, options: { mod?: boolean; out?: string } = {}): Promise<BuildReport> {
  check(crate);
  const sdk = BUILD_SDK[crate.kind] as Sdk;
  const work = path.join(crate.dir, 'target', 'ckx-kit', crate.kind);
  const src = path.join(work, 'crate');
  fs.rmSync(src, { recursive: true, force: true });
  for (const [rel, content] of Object.entries(crate.source.files)) {
    const file = path.join(src, rel);
    fs.mkdirSync(path.dirname(file), { recursive: true });
    fs.writeFileSync(
      file,
      rel === 'Cargo.toml' ? rewriteExecManifest(content, kitPath('crates', sdk), crate.kind) : content,
    );
  }
  fs.copyFileSync(kitPath('rust-toolchain.toml'), path.join(src, 'rust-toolchain.toml'));
  // The registry crates' versions the platform vendors; cargo adds this crate's own entries.
  fs.copyFileSync(kitPath('vendor.lock'), path.join(src, 'Cargo.lock'));
  // One target directory can serve many crates (CKX_KIT_TARGET_DIR), so their shared
  // dependencies compile once.
  const targetDir = process.env.CKX_KIT_TARGET_DIR
    ? path.resolve(process.env.CKX_KIT_TARGET_DIR)
    : path.join(work, 'target');
  run(`cargo build --release --target ${TARGET}`, cargo(), ['build', '--release', '--target', TARGET], {
    cwd: src,
    env: { ...process.env, CARGO_TARGET_DIR: targetDir },
  });
  const raw = path.join(targetDir, TARGET, 'release', `${crate.libName}.wasm`);
  if (!fs.existsSync(raw)) {
    throw new Error(`${crate.name}: no .wasm produced; [lib] crate-type must be ["cdylib"]`);
  }

  let bytes: Buffer;
  let summary: ClientCapabilitySummary | undefined;
  if (crate.kind === 'client') {
    const step = path.join(work, 'client');
    fs.rmSync(step, { recursive: true, force: true });
    fs.mkdirSync(step, { recursive: true });
    fs.copyFileSync(raw, path.join(step, 'raw.wasm'));
    run(`instrument --max-memory-mb ${CLIENT_MAX_MEMORY_MB}`, instrumentBin(), [
      'raw.wasm',
      'metered.wasm',
      '--max-memory-mb',
      String(CLIENT_MAX_MEMORY_MB),
    ], { cwd: step });
    run(`wasm-opt ${WASM_OPT_ARGS.join(' ')}`, wasmOpt(), [...WASM_OPT_ARGS, 'metered.wasm', '-o', 'final.wasm'], { cwd: step });
    bytes = fs.readFileSync(path.join(step, 'final.wasm'));
    await validateClientArtifact(bytes);
    summary = deriveClientCapabilities(bytes, await readHostCatalog(kitPath('checks', 'compute-toolchain', 'host-catalog.json')));
  } else {
    bytes = fs.readFileSync(raw);
    await validateExecArtifact(bytes);
    if (options.mod && bytes.length > MOD_ARTIFACT_MAX_BYTES) {
      throw new Error(`module is ${bytes.length} bytes; a mod's module is at most ${MOD_ARTIFACT_MAX_BYTES}`);
    }
  }

  const out = path.resolve(options.out ?? path.join(crate.dir, 'target', 'ckx-kit', `${crate.name}.wasm`));
  fs.mkdirSync(path.dirname(out), { recursive: true });
  fs.writeFileSync(out, bytes);
  const pin = readPin();
  const report: BuildReport = {
    crate: crate.name,
    kind: crate.kind,
    mod: Boolean(options.mod),
    wasm: out,
    sizeBytes: bytes.length,
    sha256: sha256(bytes),
    notSent: crate.notSent,
    kit: {
      version: kitVersion(),
      toolchain: pin.toolchain_version ?? 'unknown',
      sdk,
      sdkVersion: sdkVersion(sdk),
      rust: pin.rust_version ?? 'unknown',
    },
  };
  if (summary) {
    report.capabilitySummary = JSON.parse(canonicalJson(summary));
    report.capabilityHash = capabilityHash(summary);
    report.tickIntervalMs = clientTickIntervalMs(crate.source.files['Cargo.toml']);
  }
  return report;
}
