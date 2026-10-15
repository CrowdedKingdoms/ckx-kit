import * as fs from 'node:fs';
import * as path from 'node:path';
import { kitPath, sdkRequirement, type Sdk } from './kit';

/** What `new` can start from: the platform's starters, and the kit's own templates. */
export const TEMPLATES: Record<string, { dir: string[]; sdk: Sdk; about: string }> = {
  mod: { dir: ['starters', 'grid-mod'], sdk: 'ckx-sdk', about: "a mod: a player's code on a grid they own" },
  client: { dir: ['templates', 'client'], sdk: 'crowdy-client-sdk', about: "a mod's CLIENT half, run in visitors' browsers" },
  hub: { dir: ['templates', 'hub'], sdk: 'ckx-sdk', about: 'a hub: state and one handler at a time' },
  spoke: { dir: ['templates', 'spoke'], sdk: 'ckx-sdk', about: 'a spoke: stateless, scaled out' },
  'world-tick': { dir: ['starters', 'world-tick'], sdk: 'ckx-sdk', about: "the starter pack's root hub: clock, weather, resources, events" },
  matchmaker: { dir: ['starters', 'matchmaker'], sdk: 'ckx-sdk', about: 'a rating queue that reserves sessions' },
  session: { dir: ['starters', 'session'], sdk: 'ckx-sdk', about: 'one game from lobby to result' },
  'npc-mobs': { dir: ['starters', 'npc-mobs'], sdk: 'ckx-sdk', about: "an area's mobs and NPCs" },
  leaderboard: { dir: ['starters', 'leaderboard'], sdk: 'ckx-sdk', about: 'a board other hubs submit to, with seasons' },
  deck: { dir: ['starters', 'deck'], sdk: 'ckx-sdk', about: 'a card table with hands only their player reads' },
  tactics: { dir: ['starters', 'tactics'], sdk: 'ckx-sdk', about: "a grid battle in phases against the hub's units" },
  director: { dir: ['starters', 'director'], sdk: 'ckx-sdk', about: 'waves down a lane, towers, a boss with phases' },
  minigame: { dir: ['starters', 'minigame'], sdk: 'ckx-sdk', about: 'a duel of hidden throws, revealed together' },
  instance: { dir: ['starters', 'instance'], sdk: 'ckx-sdk', about: "a party's private run: seeded floors, permadeath" },
  arena: { dir: ['starters', 'arena'], sdk: 'ckx-sdk', about: 'classes and abilities in real time' },
  racing: { dir: ['starters', 'racing'], sdk: 'ckx-sdk', about: 'races with laps and a ghost, and ball games' },
  territory: { dir: ['starters', 'territory'], sdk: 'ckx-sdk', about: 'control points, and a shrinking circle' },
  player: { dir: ['starters', 'player'], sdk: 'ckx-sdk', about: "a player's own hub: items, currency, crafting, quests (not a ledger)" },
  market: { dir: ['starters', 'market'], sdk: 'ckx-sdk', about: 'vendors, an order book, trades between players (not a ledger)' },
};

const CRATE_NAME = /^[a-z][a-z0-9_-]{0,63}$/;

function copyTree(from: string, to: string): void {
  fs.mkdirSync(to, { recursive: true });
  for (const entry of fs.readdirSync(from, { withFileTypes: true })) {
    if (entry.name === 'target') continue;
    const src = path.join(from, entry.name);
    const dst = path.join(to, entry.name);
    if (entry.isDirectory()) copyTree(src, dst);
    else fs.copyFileSync(src, dst);
  }
}

/** The manifest with `[package] name` set and the SDK's line naming this kit's version. */
function personalise(manifest: string, name: string, sdk: Sdk): string {
  let section = '';
  let renamed = false;
  return manifest
    .split('\n')
    .map((line) => {
      const header = /^\s*\[([^\]]+)\]\s*$/.exec(line);
      if (header) {
        section = header[1].trim();
        return line;
      }
      if (section === 'package' && !renamed && /^\s*name\s*=/.test(line)) {
        renamed = true;
        return `name = "${name}"`;
      }
      if (section === 'dependencies' && new RegExp(`^\\s*${sdk}\\s*=`).test(line)) {
        return `${sdk} = "${sdkRequirement(sdk)}"`;
      }
      return line;
    })
    .join('\n');
}

export function newProject(template: string, name: string, into?: string): string {
  const spec = TEMPLATES[template];
  if (!spec) throw new Error(`no template '${template}'; one of: ${Object.keys(TEMPLATES).join(', ')}`);
  if (!CRATE_NAME.test(name)) {
    throw new Error(`crate name '${name}' must be lowercase letters, digits, '-' or '_', starting with a letter`);
  }
  if (template === 'mod' && name.length > 48) throw new Error("a mod's name is at most 48 characters");
  const dir = path.resolve(into ?? name);
  if (fs.existsSync(dir) && fs.readdirSync(dir).length > 0) throw new Error(`${dir} exists and is not empty`);
  copyTree(kitPath(...spec.dir), dir);
  const manifest = path.join(dir, 'Cargo.toml');
  fs.writeFileSync(manifest, personalise(fs.readFileSync(manifest, 'utf8'), name, spec.sdk));
  fs.copyFileSync(kitPath('rust-toolchain.toml'), path.join(dir, 'rust-toolchain.toml'));
  fs.writeFileSync(path.join(dir, '.gitignore'), 'target/\n');
  return dir;
}
