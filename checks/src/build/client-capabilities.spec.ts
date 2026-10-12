import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { clientAbiModule, I32, wasmModule } from '../test-support/wasm-module';
import {
  aggregateClientCapabilities,
  canonicalJson,
  capabilityHash,
  capabilitySummaryWidens,
  deriveClientCapabilities,
  parseHostCatalog,
} from './client-capabilities';

const CATALOG_JSON = readFileSync(
  join(__dirname, '..', '..', 'compute-toolchain', 'host-catalog.json'),
  'utf8',
);
const catalog = parseHostCatalog(CATALOG_JSON);

describe('parseHostCatalog', () => {
  it('reads the toolchain catalog', () => {
    expect(catalog.catalogVersion).toBe(1);
    expect(catalog.functions.map((f) => f.name)).toContain('hud_set');
  });

  it('refuses a function twice, an unknown group or no targets', () => {
    const raw = JSON.parse(CATALOG_JSON) as { functions: unknown[] };
    const twice = { ...raw, functions: [...raw.functions, raw.functions[0]] };
    expect(() => parseHostCatalog(JSON.stringify(twice))).toThrow(/twice/);
    const group = {
      ...raw,
      functions: [{ name: 'x', group: 'nope', targets: ['client'] }],
    };
    expect(() => parseHostCatalog(JSON.stringify(group))).toThrow(/group/);
    const targets = {
      ...raw,
      functions: [{ name: 'x', group: 'meta', targets: [] }],
    };
    expect(() => parseHostCatalog(JSON.stringify(targets))).toThrow(/targets/);
  });
});

describe('deriveClientCapabilities', () => {
  it('finds the client host calls named in the module, their groups and hooks', () => {
    const s = deriveClientCapabilities(
      clientAbiModule({ custom: 'hud_set voxel_set actors_list_radius' }),
      catalog,
    );
    expect(s.target).toBe('client');
    expect(s.imports).toEqual(['ck.log']);
    // actors_list is named inside actors_list_radius: counted, conservatively.
    expect(s.hostFunctions).toEqual([
      'actors_list',
      'actors_list_radius',
      'hud_set',
      'voxel_set',
    ]);
    expect(s.capabilityGroups).toEqual([
      'present',
      'world_read',
      'world_write',
    ]);
    expect(s.presentationHooks).toEqual(['hud_set']);
    expect(s.exportedFunctions).toEqual([
      'ck_alloc',
      'ck_free',
      'handle_invoke',
      'init',
      'on_event',
      'tick',
    ]);
  });

  it('counts a name that only appears as a host_call argument', () => {
    const s = deriveClientCapabilities(
      clientAbiModule({
        custom: '{"fn":"send_client_event","args":{}} host("pose_set")',
      }),
      catalog,
    );
    expect(s.imports).toEqual(['ck.log']);
    expect(s.hostFunctions).toEqual(
      expect.arrayContaining(['pose_set', 'send_client_event']),
    );
    expect(s.hostFunctions).not.toContain('pose_get');
  });

  it('counts only functions offered to CLIENT code', () => {
    // container_get_by_key is server-only; the client container_get inside it still counts.
    const s = deriveClientCapabilities(
      clientAbiModule({ custom: 'container_get_by_key' }),
      catalog,
    );
    expect(s.hostFunctions).toEqual(['container_get']);
  });

  it('derives nothing from a module that names nothing', () => {
    const s = deriveClientCapabilities(
      wasmModule({ types: [{ params: [I32], results: [] }] }),
      catalog,
    );
    expect(s).toEqual({
      version: 1,
      target: 'client',
      imports: [],
      hostFunctions: [],
      capabilityGroups: [],
      presentationHooks: [],
      exportedFunctions: [],
    });
  });
});

describe('capability hashes and trust', () => {
  const hud = deriveClientCapabilities(
    clientAbiModule({ custom: 'hud_set' }),
    catalog,
  );
  const world = deriveClientCapabilities(
    clientAbiModule({ custom: 'voxel_set' }),
    catalog,
  );

  it('hashes the canonical form, whatever the key order', () => {
    const shuffled = Object.fromEntries(Object.entries(hud).reverse());
    expect(canonicalJson(shuffled)).toBe(canonicalJson(hud));
    expect(capabilityHash(shuffled)).toBe(capabilityHash(hud));
    expect(capabilityHash(hud)).toMatch(/^[0-9a-f]{64}$/);
    expect(capabilityHash(hud)).not.toBe(capabilityHash(world));
  });

  it('an author\u2019s union holds every summary once', () => {
    const union = aggregateClientCapabilities([hud, world, hud]);
    expect(union.hostFunctions).toEqual(['hud_set', 'voxel_set']);
    expect(union.capabilityGroups).toEqual(['present', 'world_write']);
    expect(aggregateClientCapabilities([hud])).toEqual(hud);
  });

  it('a new import, host call, group or hook widens; less does not', () => {
    const union = aggregateClientCapabilities([hud, world]);
    expect(capabilitySummaryWidens(hud, union)).toBe(true);
    expect(capabilitySummaryWidens(union, hud)).toBe(false);
    expect(capabilitySummaryWidens(union, union)).toBe(false);
    expect(
      capabilitySummaryWidens(hud, {
        ...hud,
        imports: [...hud.imports, 'ck.host_call'],
      }),
    ).toBe(true);
    expect(
      capabilitySummaryWidens(hud, {
        ...hud,
        presentationHooks: ['overlay_draw'],
      }),
    ).toBe(true);
    expect(capabilitySummaryWidens({}, {})).toBe(false);
  });

  it('a new exported entry point widens: the page feeds `on_event`', () => {
    expect(hud.exportedFunctions).toContain('on_event');
    const noEvents = {
      ...hud,
      exportedFunctions: hud.exportedFunctions.filter((f) => f !== 'on_event'),
    };
    expect(capabilitySummaryWidens(noEvents, hud)).toBe(true);
    expect(capabilitySummaryWidens(hud, noEvents)).toBe(false);
  });
});
