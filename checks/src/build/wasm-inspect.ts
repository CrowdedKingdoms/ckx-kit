/**
 * Readers for the few WASM sections the build pipeline checks without instantiating a module:
 * exported function signatures and the declared memory maximum, and the CLIENT ABI check
 * CLIENT builds pass.
 */

export function readU32Leb(
  bytes: Uint8Array,
  offset: number,
  end: number,
): { value: number; next: number } {
  let value = 0;
  let shift = 0;
  for (let i = 0; i < 5 && offset < end; i++, offset++) {
    const byte = bytes[offset];
    value += (byte & 0x7f) * 2 ** shift;
    if ((byte & 0x80) === 0) {
      return { value, next: offset + 1 };
    }
    shift += 7;
  }
  throw new Error('malformed WASM u32 LEB');
}

export interface WasmFunctionType {
  params: number[];
  results: number[];
}

export function readWasmName(
  bytes: Uint8Array,
  offset: number,
  end: number,
): { value: string; next: number } {
  const length = readU32Leb(bytes, offset, end);
  const next = length.next + length.value;
  if (next > end) throw new Error('truncated WASM name');
  return {
    value: new TextDecoder().decode(bytes.subarray(length.next, next)),
    next,
  };
}

/** Resolve exported function names to their declared core-WASM signatures. */
export function readFunctionExportTypes(
  bytes: Uint8Array,
): Map<string, WasmFunctionType> {
  const types: WasmFunctionType[] = [];
  const functionTypeIndices: number[] = [];
  const exportedFunctions = new Map<string, number>();
  let offset = 8;
  while (offset < bytes.length) {
    const sectionId = bytes[offset++];
    const sectionSize = readU32Leb(bytes, offset, bytes.length);
    offset = sectionSize.next;
    const sectionEnd = offset + sectionSize.value;
    if (sectionEnd > bytes.length) throw new Error('truncated WASM section');

    if (sectionId === 1) {
      const count = readU32Leb(bytes, offset, sectionEnd);
      offset = count.next;
      for (let i = 0; i < count.value; i++) {
        if (bytes[offset++] !== 0x60) {
          throw new Error('unsupported WASM function type');
        }
        const paramCount = readU32Leb(bytes, offset, sectionEnd);
        offset = paramCount.next;
        const params = Array.from(
          bytes.subarray(offset, offset + paramCount.value),
        );
        offset += paramCount.value;
        const resultCount = readU32Leb(bytes, offset, sectionEnd);
        offset = resultCount.next;
        const results = Array.from(
          bytes.subarray(offset, offset + resultCount.value),
        );
        offset += resultCount.value;
        if (offset > sectionEnd)
          throw new Error('truncated WASM function type');
        types.push({ params, results });
      }
    } else if (sectionId === 2) {
      const count = readU32Leb(bytes, offset, sectionEnd);
      offset = count.next;
      for (let i = 0; i < count.value; i++) {
        offset = readWasmName(bytes, offset, sectionEnd).next;
        offset = readWasmName(bytes, offset, sectionEnd).next;
        const kind = bytes[offset++];
        if (kind !== 0) {
          throw new Error(
            'client ABI parser encountered a non-function import',
          );
        }
        const typeIndex = readU32Leb(bytes, offset, sectionEnd);
        offset = typeIndex.next;
        functionTypeIndices.push(typeIndex.value);
      }
    } else if (sectionId === 3) {
      const count = readU32Leb(bytes, offset, sectionEnd);
      offset = count.next;
      for (let i = 0; i < count.value; i++) {
        const typeIndex = readU32Leb(bytes, offset, sectionEnd);
        offset = typeIndex.next;
        functionTypeIndices.push(typeIndex.value);
      }
    } else if (sectionId === 7) {
      const count = readU32Leb(bytes, offset, sectionEnd);
      offset = count.next;
      for (let i = 0; i < count.value; i++) {
        const name = readWasmName(bytes, offset, sectionEnd);
        offset = name.next;
        const kind = bytes[offset++];
        const index = readU32Leb(bytes, offset, sectionEnd);
        offset = index.next;
        if (kind === 0) exportedFunctions.set(name.value, index.value);
      }
    }
    offset = sectionEnd;
  }

  const result = new Map<string, WasmFunctionType>();
  for (const [name, functionIndex] of exportedFunctions) {
    const typeIndex = functionTypeIndices[functionIndex];
    const type = types[typeIndex];
    if (!type) throw new Error(`missing WASM type for export '${name}'`);
    result.set(name, type);
  }
  return result;
}

/** Read the sole defined wasm32 memory maximum; imported/multiple memories fail closed. */
export function readDefinedMemoryMaximum(bytes: Uint8Array): number | null {
  if (
    bytes.length < 8 ||
    bytes[0] !== 0x00 ||
    bytes[1] !== 0x61 ||
    bytes[2] !== 0x73 ||
    bytes[3] !== 0x6d
  ) {
    throw new Error('malformed WASM header');
  }
  let offset = 8;
  while (offset < bytes.length) {
    const sectionId = bytes[offset++];
    const sectionSize = readU32Leb(bytes, offset, bytes.length);
    offset = sectionSize.next;
    const sectionEnd = offset + sectionSize.value;
    if (sectionEnd > bytes.length) throw new Error('truncated WASM section');
    if (sectionId !== 5) {
      offset = sectionEnd;
      continue;
    }
    const count = readU32Leb(bytes, offset, sectionEnd);
    offset = count.next;
    if (count.value !== 1) {
      throw new Error(
        `artifact must define exactly one memory (got ${count.value})`,
      );
    }
    const flags = readU32Leb(bytes, offset, sectionEnd);
    offset = flags.next;
    // Only wasm32, unshared memory with an explicit maximum is accepted.
    if (flags.value !== 1) return null;
    const minimum = readU32Leb(bytes, offset, sectionEnd);
    offset = minimum.next;
    const maximum = readU32Leb(bytes, offset, sectionEnd);
    offset = maximum.next;
    if (minimum.value > maximum.value || offset !== sectionEnd) {
      throw new Error('malformed WASM memory limits');
    }
    return maximum.value;
  }
  return null;
}

/** wasm-opt feature flags rustc-emitted modules need (Phase 0 finding). */
export const WASM_OPT_ARGS = [
  '-O3',
  '--enable-bulk-memory',
  '--enable-sign-ext',
  '--enable-mutable-globals',
  '--enable-nontrapping-float-to-int',
];

const WASM_PAGE_BYTES = 64 * 1024;
const I32 = 0x7f;
const I64 = 0x7e;

/**
 * What a CLIENT half may import: the `ck` module the CrowdyJS browser glue implements, and
 * `random_get`, the one WASI call it answers for real (crowdy-client-sdk's `random_bytes`).
 */
export const CLIENT_ABI_IMPORTS: Readonly<Record<string, ReadonlySet<string>>> =
  {
    ck: new Set(['log', 'now_ms', 'state_get', 'state_set', 'host_call']),
    wasi_snapshot_preview1: new Set(['random_get']),
  };

/** The functions every CLIENT half exports, with their core-WASM signatures. */
const CLIENT_ABI_FUNCTIONS: Readonly<Record<string, WasmFunctionType>> = {
  init: { params: [], results: [] },
  tick: { params: [I32], results: [] },
  ck_alloc: { params: [I32], results: [I32] },
  ck_free: { params: [I32, I32], results: [] },
  handle_invoke: { params: [I32, I32], results: [I64] },
};
const CLIENT_ABI_OPTIONAL: Readonly<Record<string, WasmFunctionType>> = {
  on_event: { params: [I32, I32], results: [] },
};

/** The largest CLIENT half this API builds and serves. */
export const CLIENT_ARTIFACT_MAX_BYTES = 512 * 1024;

/** The linear memory `instrument` clamps a CLIENT half to. */
export const CLIENT_MAX_MEMORY_MB = 32;

/**
 * A built CLIENT half fit for the browser glue: at most 512 KiB, compiles, imports only the
 * client ABI, exports `memory`, the `ck_fuel` global `instrument` injects and the entry
 * points with their signatures, and declares a memory maximum within the clamp. Compiling
 * does not run the module.
 */
export async function validateClientArtifact(
  bytes: Uint8Array,
  maxMemoryMb = CLIENT_MAX_MEMORY_MB,
): Promise<void> {
  if (bytes.length > CLIENT_ARTIFACT_MAX_BYTES) {
    throw new Error(
      `module is ${bytes.length} bytes; a CLIENT half is at most ${CLIENT_ARTIFACT_MAX_BYTES}`,
    );
  }
  let module: WebAssembly.Module;
  try {
    module = await WebAssembly.compile(bytes);
  } catch (err) {
    throw new Error(`malformed WASM module: ${(err as Error).message}`);
  }
  for (const imp of WebAssembly.Module.imports(module)) {
    if (
      imp.kind !== 'function' ||
      !CLIENT_ABI_IMPORTS[imp.module]?.has(imp.name)
    ) {
      throw new Error(
        `module import ${imp.module}::${imp.name} (${imp.kind}) is not part of the CLIENT ABI`,
      );
    }
  }
  const exported = new Map(
    WebAssembly.Module.exports(module).map((e) => [e.name, e.kind]),
  );
  if (exported.get('memory') !== 'memory') {
    throw new Error("module does not export memory 'memory'");
  }
  if (exported.get('ck_fuel') !== 'global') {
    throw new Error(
      "module does not export the global 'ck_fuel'; it was not instrumented",
    );
  }
  const expected: Record<string, WasmFunctionType> = {
    ...CLIENT_ABI_FUNCTIONS,
  };
  for (const [name, type] of Object.entries(CLIENT_ABI_OPTIONAL)) {
    if (exported.has(name)) expected[name] = type;
  }
  const types = readFunctionExportTypes(bytes);
  for (const [name, want] of Object.entries(expected)) {
    if (exported.get(name) !== 'function') {
      throw new Error(
        `module does not export function '${name}'; build it with crowdy-client-sdk's register_module!`,
      );
    }
    const got = types.get(name);
    if (
      !got ||
      got.params.join(',') !== want.params.join(',') ||
      got.results.join(',') !== want.results.join(',')
    ) {
      throw new Error(`module export '${name}' has the wrong signature`);
    }
  }
  const maxPages = readDefinedMemoryMaximum(bytes);
  const clampPages = Math.floor((maxMemoryMb * 1024 * 1024) / WASM_PAGE_BYTES);
  if (maxPages === null || maxPages > clampPages) {
    throw new Error(
      `module memory maximum ${maxPages ?? 'missing'} exceeds the clamp of ${clampPages} pages`,
    );
  }
}
