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
