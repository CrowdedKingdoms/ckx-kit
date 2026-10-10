import { spawnSync } from 'node:child_process';
import { kitVersion, readPin, sdkVersion } from './kit';
import { which } from './tools';

interface Finding {
  ok: boolean;
  what: string;
  detail: string;
}

function output(cmd: string, args: string[]): string | null {
  const r = spawnSync(cmd, args, { encoding: 'utf8' });
  return r.status === 0 ? r.stdout.trim() : null;
}

/** What a build on this machine needs, and which platform toolchain this kit matches. */
export function doctor(): { findings: Finding[]; ok: boolean } {
  const pin = readPin();
  const rust = pin.rust_version ?? '';
  const findings: Finding[] = [];
  const node = Number(process.versions.node.split('.')[0]);
  findings.push({ ok: node >= 20, what: 'node', detail: `${process.versions.node} (20 or later)` });
  const cargo = which('cargo') ? output('cargo', ['--version']) : null;
  findings.push({ ok: Boolean(cargo), what: 'cargo', detail: cargo ?? 'not installed: https://rustup.rs' });
  if (which('rustup')) {
    const toolchains = output('rustup', ['toolchain', 'list']) ?? '';
    const installed = toolchains.split('\n').some((l) => l.startsWith(`${rust}-`) || l.startsWith(`${rust} `));
    findings.push({
      ok: installed,
      what: `Rust ${rust}`,
      detail: installed ? 'installed' : `missing: rustup toolchain install ${rust} --target wasm32-unknown-unknown`,
    });
    const targets = installed ? (output('rustup', ['target', 'list', '--installed', '--toolchain', rust]) ?? '') : '';
    const wasm = targets.split('\n').includes('wasm32-unknown-unknown');
    findings.push({
      ok: wasm,
      what: 'wasm32-unknown-unknown',
      detail: wasm ? 'installed' : `missing: rustup target add --toolchain ${rust} wasm32-unknown-unknown`,
    });
  } else {
    findings.push({ ok: false, what: 'rustup', detail: 'not installed: the kit pins the Rust release the platform builds with' });
  }
  const opt = which('wasm-opt') ? output('wasm-opt', ['--version']) : null;
  findings.push({
    ok: Boolean(opt),
    what: 'wasm-opt',
    detail: opt ?? 'not installed (needed for CLIENT halves): apt install binaryen, brew install binaryen',
  });
  findings.push({
    ok: true,
    what: 'kit',
    detail: `ckx-kit ${kitVersion()}: platform toolchain ${pin.toolchain_version ?? 'unknown'}, ckx-sdk ${sdkVersion('ckx-sdk')}, crowdy-client-sdk ${sdkVersion('crowdy-client-sdk')}, Rust ${rust}`,
  });
  return { findings, ok: findings.every((f) => f.ok) };
}
