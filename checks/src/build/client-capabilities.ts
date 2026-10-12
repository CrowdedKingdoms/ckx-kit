import { createHash } from 'node:crypto';
import { promises as fs } from 'node:fs';

/**
 * The capability summary of a CLIENT half: what visitors consent to before their browser runs
 * it. It is DERIVED from the built module, never declared by its author: the module's imports,
 * the client host calls it can reach, their capability groups, the presentation hooks and the
 * exported entry points. A visitor's consent binds to the summary's hash, and trusting an
 * author binds to the hash of the union of that author's summaries in a grid.
 *
 * Host calls go through one generic `host_call` import that dispatches on a name string, so a
 * function counts as reachable when its name appears anywhere in the module's bytes. A wrapper
 * such as `host("pose_set", …)` or `api::send_client_event` still leaves that literal in the
 * module, and the literal is what counts: the summary does not need a direct `ck.pose_set`
 * import. Presence means "may call". A name assembled at run time is not seen, so the summary
 * bounds what a module calls only where the browser broker refuses calls outside it.
 */

/** One entry of `compute-toolchain/host-catalog.json`. */
export interface HostCatalogFunction {
  name: string;
  group: string;
  targets: ReadonlyArray<'server' | 'client'>;
}

export interface HostCatalog {
  catalogVersion: number;
  groups: Readonly<Record<string, string>>;
  functions: ReadonlyArray<HostCatalogFunction>;
}

export interface ClientCapabilitySummary {
  version: 1;
  target: 'client';
  /** The module's WASM imports as `module.name`. */
  imports: string[];
  /** Client host calls the module can reach. */
  hostFunctions: string[];
  /** The host catalog groups of those calls. */
  capabilityGroups: string[];
  /** HUD and overlay hooks among them. */
  presentationHooks: string[];
  /** The functions the module exports. */
  exportedFunctions: string[];
}

/** The host catalog, checked: known groups, valid targets, no name twice. */
export function parseHostCatalog(json: string): HostCatalog {
  const raw = JSON.parse(json) as {
    catalogVersion?: unknown;
    groups?: Record<string, string>;
    functions?: unknown;
  };
  const groups = raw.groups ?? {};
  if (!Array.isArray(raw.functions) || raw.functions.length === 0) {
    throw new Error('host catalog lists no functions');
  }
  const functions: HostCatalogFunction[] = [];
  const seen = new Set<string>();
  for (const entry of raw.functions as unknown[]) {
    const fn = (entry ?? {}) as {
      name?: unknown;
      group?: unknown;
      targets?: unknown;
    };
    const name = fn.name;
    if (typeof name !== 'string' || seen.has(name)) {
      throw new Error(`host catalog names ${String(name)} badly or twice`);
    }
    seen.add(name);
    if (typeof fn.group !== 'string' || !(fn.group in groups)) {
      throw new Error(
        `host catalog: ${name} is in unknown group ${String(fn.group)}`,
      );
    }
    const targets: unknown = fn.targets;
    if (
      !Array.isArray(targets) ||
      targets.length === 0 ||
      targets.some((t) => t !== 'server' && t !== 'client')
    ) {
      throw new Error(`host catalog: ${name} has invalid targets`);
    }
    functions.push({
      name,
      group: fn.group,
      targets: targets as Array<'server' | 'client'>,
    });
  }
  return { catalogVersion: Number(raw.catalogVersion), groups, functions };
}

export async function readHostCatalog(file: string): Promise<HostCatalog> {
  return parseHostCatalog(await fs.readFile(file, 'utf8'));
}

const sorted = (values: Iterable<string>): string[] =>
  [...new Set(values)].sort();

/**
 * A built CLIENT half's summary. Reads the module's import and export sections through
 * `WebAssembly.Module`, which compiles it and runs nothing.
 */
export function deriveClientCapabilities(
  bytes: Uint8Array,
  catalog: HostCatalog,
): ClientCapabilitySummary {
  const module = new WebAssembly.Module(bytes);
  const imports = WebAssembly.Module.imports(module).map(
    (i) => `${i.module}.${i.name}`,
  );
  const exportedFunctions = WebAssembly.Module.exports(module)
    .filter((e) => e.kind === 'function')
    .map((e) => e.name);
  const haystack = Buffer.from(bytes.buffer, bytes.byteOffset, bytes.length);
  const reachable = catalog.functions.filter(
    (fn) =>
      fn.targets.includes('client') &&
      (imports.includes(`ck.${fn.name}`) ||
        haystack.includes(Buffer.from(fn.name, 'utf8'))),
  );
  return {
    version: 1,
    target: 'client',
    imports: sorted(imports),
    hostFunctions: sorted(reachable.map((fn) => fn.name)),
    capabilityGroups: sorted(reachable.map((fn) => fn.group)),
    presentationHooks: sorted(
      reachable.filter((fn) => fn.group === 'present').map((fn) => fn.name),
    ),
    exportedFunctions: sorted(exportedFunctions),
  };
}

/** The union of several summaries: one author's CLIENT halves in a grid. */
export function aggregateClientCapabilities(
  summaries: ReadonlyArray<Partial<ClientCapabilitySummary>>,
): ClientCapabilitySummary {
  const union = (
    key: keyof Omit<ClientCapabilitySummary, 'version' | 'target'>,
  ) =>
    sorted(
      summaries.flatMap((s) => {
        const values = s[key];
        return Array.isArray(values) ? values : [];
      }),
    );
  return {
    version: 1,
    target: 'client',
    imports: union('imports'),
    hostFunctions: union('hostFunctions'),
    capabilityGroups: union('capabilityGroups'),
    presentationHooks: union('presentationHooks'),
    exportedFunctions: union('exportedFunctions'),
  };
}

/** JSON with every object's keys sorted, so equal summaries hash equally. */
export function canonicalJson(value: unknown): string {
  return JSON.stringify(sortKeys(value));
}

function sortKeys(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value && typeof value === 'object') {
    return Object.fromEntries(
      Object.keys(value as Record<string, unknown>)
        .sort()
        .map((k) => [k, sortKeys((value as Record<string, unknown>)[k])]),
    );
  }
  return value;
}

/** What consent binds to: the SHA-256 of the canonical summary, hex. */
export function capabilityHash(summary: unknown): string {
  return createHash('sha256').update(canonicalJson(summary)).digest('hex');
}

/**
 * Whether `next` asks for anything `prev` did not: an import, a host call, a capability group,
 * a presentation hook or an exported entry point (an export such as `on_event` is a channel the
 * page feeds). Trust in an author stops covering their CLIENT halves in a grid once their union
 * widens; narrowing keeps it.
 */
export function capabilitySummaryWidens(
  prev: Partial<ClientCapabilitySummary>,
  next: Partial<ClientCapabilitySummary>,
): boolean {
  const widens = (a?: string[], b?: string[]) => {
    const have = new Set(a ?? []);
    return (b ?? []).some((v) => !have.has(v));
  };
  return (
    widens(prev.imports, next.imports) ||
    widens(prev.hostFunctions, next.hostFunctions) ||
    widens(prev.capabilityGroups, next.capabilityGroups) ||
    widens(prev.presentationHooks, next.presentationHooks) ||
    widens(prev.exportedFunctions, next.exportedFunctions)
  );
}
