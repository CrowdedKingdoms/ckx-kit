/**
 * Encodes small WASM modules for tests of the checks the build pipeline makes, without a WAT
 * toolchain. Every defined function's body returns zeros of its result types.
 */

export const I32 = 0x7f;
export const I64 = 0x7e;

export interface WasmType {
  params: number[];
  results: number[];
}

export interface WasmModuleSpec {
  types: WasmType[];
  imports?: Array<{ module: string; name: string; type: number }>;
  /** The type index of each defined function, after the imported ones. */
  functions?: number[];
  memory?: { min: number; max?: number } | null;
  /** Mutable i64 globals initialised to zero, as instrument's `ck_fuel` is. */
  globals?: number;
  exports?: Array<{
    name: string;
    kind: 'function' | 'memory' | 'global';
    index: number;
  }>;
  /** A custom section's payload, e.g. strings a capability scan should find. */
  custom?: string;
  /** Bytes of a passive data segment, to pad the module. */
  data?: number;
}

function leb(n: number): number[] {
  const out: number[] = [];
  do {
    let b = n & 0x7f;
    n = Math.floor(n / 128);
    if (n > 0) b |= 0x80;
    out.push(b);
  } while (n > 0);
  return out;
}

const name = (s: string) => [...leb(Buffer.byteLength(s)), ...Buffer.from(s)];
const vec = (items: number[][]) => [...leb(items.length), ...items.flat()];
const section = (id: number, body: number[]) => [
  id,
  ...leb(body.length),
  ...body,
];
const zero = (t: number) => (t === I64 ? [0x42, 0x00] : [0x41, 0x00]);
const KIND = { function: 0, memory: 2, global: 3 } as const;

export function wasmModule(spec: WasmModuleSpec): Uint8Array {
  const out: number[] = [0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
  out.push(
    ...section(
      1,
      vec(
        spec.types.map((t) => [
          0x60,
          ...vec(t.params.map((p) => [p])),
          ...vec(t.results.map((r) => [r])),
        ]),
      ),
    ),
  );
  if (spec.imports?.length) {
    out.push(
      ...section(
        2,
        vec(
          spec.imports.map((i) => [
            ...name(i.module),
            ...name(i.name),
            0x00,
            ...leb(i.type),
          ]),
        ),
      ),
    );
  }
  const functions = spec.functions ?? [];
  if (functions.length)
    out.push(...section(3, vec(functions.map((t) => leb(t)))));
  if (spec.memory) {
    const { min, max } = spec.memory;
    out.push(
      ...section(
        5,
        vec([
          max === undefined
            ? [0x00, ...leb(min)]
            : [0x01, ...leb(min), ...leb(max)],
        ]),
      ),
    );
  }
  if (spec.globals) {
    out.push(
      ...section(
        6,
        vec(
          Array.from({ length: spec.globals }, () => [
            I64,
            0x01,
            0x42,
            0x00,
            0x0b,
          ]),
        ),
      ),
    );
  }
  if (spec.exports?.length) {
    out.push(
      ...section(
        7,
        vec(
          spec.exports.map((e) => [
            ...name(e.name),
            KIND[e.kind],
            ...leb(e.index),
          ]),
        ),
      ),
    );
  }
  if (spec.data) out.push(...section(12, leb(1)));
  if (functions.length) {
    out.push(
      ...section(
        10,
        vec(
          functions.map((t) => {
            const body = [0x00, ...spec.types[t].results.flatMap(zero), 0x0b];
            return [...leb(body.length), ...body];
          }),
        ),
      ),
    );
  }
  const tail: Buffer[] = [];
  if (spec.data) {
    const segment = [...leb(1), 0x01, ...leb(spec.data)];
    tail.push(
      Buffer.from([11, ...leb(segment.length + spec.data), ...segment]),
      Buffer.alloc(spec.data, 7),
    );
  }
  if (spec.custom) {
    tail.push(
      Buffer.from(section(0, [...name('c'), ...Buffer.from(spec.custom)])),
    );
  }
  return new Uint8Array(Buffer.concat([Buffer.from(out), ...tail]));
}

/**
 * A module with the whole CLIENT ABI: `ck.log` imported, memory, the `ck_fuel` global and
 * every entry point; `over` replaces parts of it.
 */
export function clientAbiModule(
  over: Partial<WasmModuleSpec> = {},
): Uint8Array {
  const types: WasmType[] = [
    { params: [], results: [] }, // 0 init
    { params: [I32], results: [] }, // 1 tick
    { params: [I32], results: [I32] }, // 2 ck_alloc
    { params: [I32, I32], results: [] }, // 3 ck_free, on_event
    { params: [I32, I32], results: [I64] }, // 4 handle_invoke
    { params: [I32, I32, I32], results: [] }, // 5 ck.log
  ];
  return wasmModule({
    types,
    imports: [{ module: 'ck', name: 'log', type: 5 }],
    functions: [0, 1, 2, 3, 4, 3],
    memory: { min: 1, max: 16 },
    globals: 1,
    exports: [
      { name: 'memory', kind: 'memory', index: 0 },
      { name: 'ck_fuel', kind: 'global', index: 0 },
      { name: 'init', kind: 'function', index: 1 },
      { name: 'tick', kind: 'function', index: 2 },
      { name: 'ck_alloc', kind: 'function', index: 3 },
      { name: 'ck_free', kind: 'function', index: 4 },
      { name: 'handle_invoke', kind: 'function', index: 5 },
      { name: 'on_event', kind: 'function', index: 6 },
    ],
    ...over,
  });
}
