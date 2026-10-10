#!/usr/bin/env node
import * as path from 'node:path';
import { parseArgs } from 'node:util';
import { build, check } from './build';
import { readCrate } from './crate';
import { doctor } from './doctor';
import { kitVersion } from './kit';
import { newProject, TEMPLATES } from './new';

const USAGE = `ckx-kit: build, check and test ck-exec code as the Crowded Kingdoms platform builds it.

  ckx-kit new <template> <name> [--dir <path>]
      Start a crate. Templates: ${Object.keys(TEMPLATES).join(', ')}.
  ckx-kit check [dir] [--client | --exec]
      The platform's source checks, on exactly the files it takes (Cargo.toml, README.md,
      and .rs files under src/).
  ckx-kit build [dir] [--mod | --client | --exec] [--out <file>] [--json]
      What execBuild does: the checks, the pinned Rust for wasm32-unknown-unknown with the
      platform's release profile, then the module check. --mod adds the mod limits; a CLIENT
      half (a crowdy-client-sdk crate) is metered and optimized, and its capability summary
      printed.
  ckx-kit doctor
      What a build here needs, and the platform toolchain this kit matches.

The platform still builds what you deploy from its source: execModBuild, execModClientBuild,
execBuild or Crowdy Studio. Docs: https://docs.crowdedkingdoms.com/exec/intro`;

function kindOf(values: { client?: boolean; exec?: boolean; mod?: boolean }): 'client' | 'exec' | undefined {
  if (values.client && (values.exec || values.mod)) throw new Error('--client is a CLIENT half; it cannot be --exec or --mod');
  if (values.client) return 'client';
  if (values.exec || values.mod) return 'exec';
  return undefined;
}

async function main(argv: string[]): Promise<number> {
  const { values, positionals } = parseArgs({
    args: argv,
    allowPositionals: true,
    options: {
      mod: { type: 'boolean' },
      client: { type: 'boolean' },
      exec: { type: 'boolean' },
      out: { type: 'string' },
      dir: { type: 'string' },
      json: { type: 'boolean' },
      help: { type: 'boolean', short: 'h' },
      version: { type: 'boolean', short: 'v' },
    },
  });
  const [command, ...rest] = positionals;
  if (values.version) {
    console.log(kitVersion());
    return 0;
  }
  if (values.help || !command) {
    console.log(USAGE);
    return command || values.help ? 0 : 2;
  }
  switch (command) {
    case 'new': {
      const [template, name] = rest;
      if (!template || !name) throw new Error('usage: ckx-kit new <template> <name> [--dir <path>]');
      const dir = newProject(template, name, values.dir);
      const rel = path.relative(process.cwd(), dir) || '.';
      const flag = template === 'mod' ? ' --mod' : '';
      console.log(`created ${rel} (${TEMPLATES[template].about})\n\n  cd ${rel}\n  cargo test\n  npx ckx-kit build${flag}`);
      return 0;
    }
    case 'check': {
      const crate = readCrate(rest[0] ?? '.', kindOf(values));
      check(crate);
      console.log(`${crate.name}: passes the platform's checks (${crate.kind === 'client' ? 'a CLIENT half' : 'a ck-exec crate'})`);
      if (crate.notSent.length) console.log(`not sent to the platform: ${crate.notSent.join(', ')}`);
      return 0;
    }
    case 'build': {
      const crate = readCrate(rest[0] ?? '.', kindOf(values));
      const report = await build(crate, { mod: values.mod, out: values.out });
      if (values.json) {
        console.log(JSON.stringify(report, null, 2));
        return 0;
      }
      console.log(`${report.crate}: ${report.sizeBytes} bytes, sha256 ${report.sha256}\n  ${report.wasm}`);
      if (report.capabilitySummary) {
        console.log(`  capabilities ${report.capabilityHash}; host calls: ${report.capabilitySummary.hostFunctions.join(', ') || 'none'}; ticks every ${report.tickIntervalMs} ms`);
      }
      if (report.notSent.length) console.log(`  not sent to the platform: ${report.notSent.join(', ')}`);
      console.log(`  built as platform toolchain ${report.kit.toolchain} builds it (${report.kit.sdk} ${report.kit.sdkVersion}, Rust ${report.kit.rust})`);
      return 0;
    }
    case 'doctor': {
      const { findings, ok } = doctor();
      for (const f of findings) console.log(`${f.ok ? 'ok  ' : 'FAIL'} ${f.what}: ${f.detail}`);
      return ok ? 0 : 1;
    }
    default:
      throw new Error(`unknown command '${command}'\n\n${USAGE}`);
  }
}

main(process.argv.slice(2)).then(
  (code) => process.exit(code),
  (err: unknown) => {
    console.error(`error: ${err instanceof Error ? err.message : String(err)}`);
    process.exit(1);
  },
);
