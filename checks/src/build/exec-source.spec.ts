import {
  clientTickIntervalMs,
  EXEC_ARTIFACT_MAX_BYTES,
  rewriteExecManifest,
  validateExecArtifact,
  validateExecCrates,
  validateExecManifest,
  validateExecPath,
} from './exec-source';

const CARGO = `[package]
name = "lobby"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
ckx-sdk = { path = "../../crates/ckx-sdk" }
serde = { version = "1", features = ["derive"] }
`;

const crate = (
  over: Partial<{ name: string; cargo: string; lib: string }> = {},
) => ({
  name: over.name ?? 'lobby',
  files: {
    'Cargo.toml': over.cargo ?? CARGO,
    'src/lib.rs': over.lib ?? '#![no_std]\n',
  },
});

describe('validateExecPath', () => {
  it('takes Cargo.toml, README.md and .rs under src/', () => {
    for (const p of ['Cargo.toml', 'README.md', 'src/lib.rs', 'src/a/b.rs']) {
      expect(() => validateExecPath(p)).not.toThrow();
    }
  });

  it('refuses traversal, absolute paths and files outside src/', () => {
    for (const p of [
      '../x',
      'src/../etc',
      '/etc/passwd',
      'build.rs',
      'src/x.txt',
      'x.rs',
    ]) {
      expect(() => validateExecPath(p)).toThrow();
    }
  });
});

describe('validateExecManifest', () => {
  it('takes a cdylib crate with allowlisted deps', () => {
    expect(() => validateExecManifest(CARGO)).not.toThrow();
  });

  it('demands [lib] crate-type = ["cdylib"]', () => {
    expect(() =>
      validateExecManifest(
        CARGO.replace('crate-type = ["cdylib"]', 'crate-type = ["rlib"]'),
      ),
    ).toThrow(/cdylib/);
    expect(() =>
      validateExecManifest(
        CARGO.replace(/\[lib\]\ncrate-type = \["cdylib"\]\n\n/, ''),
      ),
    ).toThrow(/cdylib/);
  });

  it('refuses build scripts and out-of-registry deps', () => {
    expect(() =>
      validateExecManifest(
        CARGO.replace('[dependencies]', 'build = "build.rs"\n[dependencies]'),
      ),
    ).toThrow(/build/);
    expect(() =>
      validateExecManifest(`${CARGO}\n[build-dependencies]\ncc = "1"\n`),
    ).toThrow(/\[build-dependencies\]/);
    expect(() =>
      validateExecManifest(`${CARGO}\n[profile.release]\nlto = true\n`),
    ).toThrow(/\[profile.release\]/);
    expect(() =>
      validateExecManifest(
        CARGO.replace(
          'serde = { version = "1", features = ["derive"] }',
          'rand = "0.9"',
        ),
      ),
    ).toThrow(/allowlist/);
    expect(() =>
      validateExecManifest(
        CARGO.replace(
          'serde = { version = "1", features = ["derive"] }',
          'serde = { path = "../serde" }',
        ),
      ),
    ).toThrow(/vendored registry/);
  });

  it('ignores a commented dependency', () => {
    expect(() =>
      validateExecManifest(
        CARGO.replace('[dependencies]', '[dependencies]\n# rand = "0.9"'),
      ),
    ).not.toThrow();
  });

  it('refuses a quoted key in a dependency table, which is the same key to cargo', () => {
    const dep = (table: string) =>
      CARGO.replace('serde = { version = "1", features = ["derive"] }', table);
    for (const table of [
      'serde = { version = "1", "path" = "/toolchain/ckx-starters/world-tick" }',
      "serde = { version = \"1\", 'path' = '../serde' }",
      'serde = { "package" = "syn", version = "2", "path" = "/x" }',
      'serde = { version = "1", "pa\\u0074h" = "/x" }',
      'serde = { version = "1", features = ["derive"], }',
      'serde = { version = "1" "path" = "/x" }',
    ]) {
      expect(() => validateExecManifest(dep(table))).toThrow(
        /vendored registry|bare keys/,
      );
    }
    // Commas and quotes inside values are values.
    expect(() =>
      validateExecManifest(
        dep(
          'serde = { version = "1", features = ["derive", "std"], default-features = false }',
        ),
      ),
    ).not.toThrow();
  });
});

describe('validateExecCrates', () => {
  it('takes a small batch', () => {
    expect(() =>
      validateExecCrates([crate(), crate({ name: 'mobs' })]),
    ).not.toThrow();
  });

  it('demands Cargo.toml and src/lib.rs, unique names, and a cap', () => {
    expect(() => validateExecCrates([])).toThrow(/at least one/);
    expect(() => validateExecCrates([crate(), crate()])).toThrow(/twice/);
    expect(() =>
      validateExecCrates([{ name: 'x', files: { 'Cargo.toml': CARGO } }]),
    ).toThrow(/src\/lib\.rs/);
    expect(() =>
      validateExecCrates([{ name: 'x', files: { 'src/lib.rs': '' } }]),
    ).toThrow(/Cargo\.toml/);
    expect(() => validateExecCrates([crate({ name: 'BAD' })])).toThrow(
      /lowercase/,
    );
    const many = Array.from({ length: 17 }, (_, i) => crate({ name: `c${i}` }));
    expect(() => validateExecCrates(many)).toThrow(/at most/);
  });
});

describe('rewriteExecManifest', () => {
  it('points ckx-sdk at the toolchain and appends the release profile', () => {
    const out = rewriteExecManifest(CARGO, '/toolchain/ckx-sdk');
    expect(out).toContain('ckx-sdk = { path = "/toolchain/ckx-sdk" }');
    expect(out).not.toContain('../../crates/ckx-sdk');
    expect(out).toContain('[profile.release]');
    expect(out).toContain('panic = "abort"');
  });
});

describe('CLIENT crates', () => {
  const CLIENT_CARGO = `[package]
name = "hud"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[package.metadata.crowdy]
tick_interval_ms = 250

[dependencies]
crowdy-client-sdk = "0.1.0"
serde_json = "1"
`;
  const client = (cargo = CLIENT_CARGO, lib = '') => ({
    name: 'hud',
    files: { 'Cargo.toml': cargo, 'src/lib.rs': lib },
  });

  it('take crowdy-client-sdk and the serde crates, and the Studio tick metadata', () => {
    expect(() => validateExecManifest(CLIENT_CARGO, 'client')).not.toThrow();
    expect(() => validateExecCrates([client()], 'client')).not.toThrow();
  });

  it('refuse every other SDK and dependency', () => {
    for (const dep of [
      'ckx-sdk = "0.7.0"',
      'crowdy-compute-sdk = "0.1.8"',
      'rand = "0.9"',
    ]) {
      expect(() =>
        validateExecManifest(
          CLIENT_CARGO.replace('serde_json = "1"', dep),
          'client',
        ),
      ).toThrow(/allowlist \(crowdy-client-sdk, serde, serde_json\)/);
    }
  });

  it('are not ck-exec crates, and a ck-exec crate carries no metadata', () => {
    expect(() => validateExecManifest(CLIENT_CARGO)).toThrow(
      /\[package\.metadata\.crowdy\] is not allowed/,
    );
    expect(() =>
      validateExecManifest(
        CLIENT_CARGO.replace(
          /\[package\.metadata\.crowdy\]\ntick_interval_ms = 250\n/,
          '',
        ),
      ),
    ).toThrow(/crowdy-client-sdk' is not in the allowlist/);
  });

  it('take only tick_interval_ms, as a whole number, in the metadata', () => {
    expect(() =>
      validateExecManifest(
        CLIENT_CARGO.replace('tick_interval_ms = 250', 'script = "x.js"'),
        'client',
      ),
    ).toThrow(/only tick_interval_ms/);
    expect(() =>
      validateExecManifest(
        CLIENT_CARGO.replace(
          'tick_interval_ms = 250',
          'tick_interval_ms = "fast"',
        ),
        'client',
      ),
    ).toThrow(/whole number/);
    expect(() =>
      validateExecManifest(
        `${CLIENT_CARGO}\n[package.metadata.other]\nx = 1\n`,
        'client',
      ),
    ).toThrow(/\[package\.metadata\.other\] is not allowed/);
  });

  it('are one crate, through the same host-read guard', () => {
    expect(() =>
      validateExecCrates([client(), { ...client(), name: 'hud2' }], 'client'),
    ).toThrow(/exactly one crate/);
    expect(() =>
      validateExecCrates(
        [
          client(
            CLIENT_CARGO,
            'const K: &str = include_str!("/etc/hostname");',
          ),
        ],
        'client',
      ),
    ).toThrow(/include_str/);
    expect(() =>
      validateExecCrates(
        [
          client(
            CLIENT_CARGO,
            '#[link(wasm_import_module = "env")] extern "C" {}',
          ),
        ],
        'client',
      ),
    ).toThrow(/#\[link\]/);
  });

  it('point crowdy-client-sdk at the toolchain', () => {
    const out = rewriteExecManifest(
      CLIENT_CARGO,
      '/toolchain/crowdy-client-sdk',
      'client',
    );
    expect(out).toContain(
      'crowdy-client-sdk = { path = "/toolchain/crowdy-client-sdk" }',
    );
    expect(out).toContain('[profile.release]');
    expect(out).toContain('[package.metadata.crowdy]');
  });

  it('read the tick as Studio does: 1000 when absent, clamped to 16-1000', () => {
    expect(clientTickIntervalMs(CLIENT_CARGO)).toBe(250);
    expect(clientTickIntervalMs(CARGO)).toBe(1000);
    expect(clientTickIntervalMs(CLIENT_CARGO.replace('= 250', '= 5'))).toBe(16);
    expect(clientTickIntervalMs(CLIENT_CARGO.replace('= 250', '= 60000'))).toBe(
      1000,
    );
    expect(clientTickIntervalMs(CLIENT_CARGO.replace('= 250', '= 0'))).toBe(
      1000,
    );
    // Only inside the metadata section.
    expect(
      clientTickIntervalMs(
        CARGO.replace('[lib]', 'tick_interval_ms = 20\n[lib]'),
      ),
    ).toBe(1000);
  });
});

describe('validateExecArtifact', () => {
  // (module) with only a memory export: missing the ABI exports.
  const bare = new Uint8Array([0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]);

  it('refuses a module that is too big before compiling it', async () => {
    await expect(
      validateExecArtifact(new Uint8Array(EXEC_ARTIFACT_MAX_BYTES + 1)),
    ).rejects.toThrow(/limit/);
  });

  it('refuses malformed bytes', async () => {
    await expect(
      validateExecArtifact(new Uint8Array([1, 2, 3, 4])),
    ).rejects.toThrow(/malformed/);
  });

  it('refuses a module missing the ck-exec exports', async () => {
    await expect(validateExecArtifact(bare)).rejects.toThrow(/does not export/);
  });
});
