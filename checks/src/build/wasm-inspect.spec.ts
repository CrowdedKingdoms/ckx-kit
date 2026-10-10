import {
  clientAbiModule,
  I32,
  I64,
  wasmModule,
} from '../test-support/wasm-module';
import {
  CLIENT_ARTIFACT_MAX_BYTES,
  readDefinedMemoryMaximum,
  readFunctionExportTypes,
  validateClientArtifact,
} from './wasm-inspect';

const exportsWithout = (name: string) =>
  [
    { name: 'memory', kind: 'memory', index: 0 },
    { name: 'ck_fuel', kind: 'global', index: 0 },
    { name: 'init', kind: 'function', index: 1 },
    { name: 'tick', kind: 'function', index: 2 },
    { name: 'ck_alloc', kind: 'function', index: 3 },
    { name: 'ck_free', kind: 'function', index: 4 },
    { name: 'handle_invoke', kind: 'function', index: 5 },
  ].filter((e) => e.name !== name) as Parameters<
    typeof wasmModule
  >[0]['exports'];

describe('validateClientArtifact', () => {
  it('takes a module with the whole CLIENT ABI, on_event included', async () => {
    await expect(validateClientArtifact(clientAbiModule())).resolves.toBe(
      undefined,
    );
    expect(
      readFunctionExportTypes(clientAbiModule()).get('handle_invoke'),
    ).toEqual({ params: [I32, I32], results: [I64] });
  });

  it('refuses a module over 512 KiB before compiling it', async () => {
    await expect(
      validateClientArtifact(new Uint8Array(CLIENT_ARTIFACT_MAX_BYTES + 1)),
    ).rejects.toThrow(/at most 524288/);
    const padded = clientAbiModule({ data: CLIENT_ARTIFACT_MAX_BYTES });
    await expect(validateClientArtifact(padded)).rejects.toThrow(
      /CLIENT half is at most/,
    );
  });

  it('refuses bytes that are not WASM', async () => {
    await expect(
      validateClientArtifact(new Uint8Array([1, 2, 3, 4])),
    ).rejects.toThrow(/malformed/);
  });

  it('refuses any import outside ck::* and wasi random_get', async () => {
    const types = [
      { params: [I32, I32], results: [I32] },
      { params: [], results: [] },
    ];
    for (const [module, name] of [
      ['env', 'fetch'],
      ['wasi_snapshot_preview1', 'fd_write'],
      ['wasi_snapshot_preview1', 'clock_time_get'],
      ['ckx', 'call'],
      ['ck', 'spawn'],
    ]) {
      const bytes = clientAbiModule({
        types: [...clientAbiTypes(), ...types],
        imports: [{ module, name, type: 6 }],
      });
      await expect(validateClientArtifact(bytes)).rejects.toThrow(
        `${module}::${name}`,
      );
    }
    const random = clientAbiModule({
      types: [...clientAbiTypes(), ...types],
      imports: [
        { module: 'wasi_snapshot_preview1', name: 'random_get', type: 6 },
      ],
    });
    await expect(validateClientArtifact(random)).resolves.toBe(undefined);
  });

  it('refuses a module instrument never metered (no ck_fuel global)', async () => {
    await expect(
      validateClientArtifact(
        clientAbiModule({ globals: 0, exports: exportsWithout('ck_fuel') }),
      ),
    ).rejects.toThrow(/ck_fuel/);
  });

  it('refuses a missing entry point or memory', async () => {
    for (const name of [
      'init',
      'tick',
      'ck_alloc',
      'ck_free',
      'handle_invoke',
    ]) {
      await expect(
        validateClientArtifact(
          clientAbiModule({ exports: exportsWithout(name) }),
        ),
      ).rejects.toThrow(`'${name}'`);
    }
    await expect(
      validateClientArtifact(
        clientAbiModule({ exports: exportsWithout('memory') }),
      ),
    ).rejects.toThrow(/memory/);
  });

  it('refuses an entry point with the wrong signature', async () => {
    // handle_invoke returning i32 instead of the packed i64.
    const bytes = clientAbiModule({ functions: [0, 1, 2, 3, 2, 3] });
    await expect(validateClientArtifact(bytes)).rejects.toThrow(
      /'handle_invoke' has the wrong signature/,
    );
    const event = clientAbiModule({ functions: [0, 1, 2, 3, 4, 1] });
    await expect(validateClientArtifact(event)).rejects.toThrow(
      /'on_event' has the wrong signature/,
    );
  });

  it('refuses memory without a maximum, or over the 32 MB clamp', async () => {
    const unbounded = clientAbiModule({ memory: { min: 1 } });
    expect(readDefinedMemoryMaximum(unbounded)).toBeNull();
    await expect(validateClientArtifact(unbounded)).rejects.toThrow(
      /maximum missing/,
    );
    await expect(
      validateClientArtifact(clientAbiModule({ memory: { min: 1, max: 513 } })),
    ).rejects.toThrow(/exceeds the clamp of 512 pages/);
    await expect(
      validateClientArtifact(clientAbiModule({ memory: { min: 1, max: 512 } })),
    ).resolves.toBe(undefined);
  });
});

function clientAbiTypes() {
  return [
    { params: [], results: [] },
    { params: [I32], results: [] },
    { params: [I32], results: [I32] },
    { params: [I32, I32], results: [] },
    { params: [I32, I32], results: [I64] },
    { params: [I32, I32, I32], results: [] },
  ];
}
