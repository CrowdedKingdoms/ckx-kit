# ckx-kit

Build, check and test [ck-exec](https://docs.crowdedkingdoms.com/exec/intro) mods, hubs and spokes
for Crowded Kingdoms on your own machine, as the platform builds them, without deploying anything.

The kit is the public half of the platform's compute toolchain: the SDKs your crate compiles
against, the checks the platform runs before and after it compiles, the tool that meters a mod's
CLIENT half, the starters, and the Rust release the platform pins. Its `ckx-kit` command runs
those same checks, so a crate that passes here builds on the platform. What you deploy is still
built by the platform from its source.

## What you need

- [rustup](https://rustup.rs). Projects pin the Rust release the platform builds with
  (`rust-toolchain.toml`); install it with
  `rustup toolchain install <that version> --target wasm32-unknown-unknown`.
- Node.js 20 or later.
- [binaryen](https://github.com/WebAssembly/binaryen) (`wasm-opt`), for a mod's CLIENT half only:
  `apt install binaryen` or `brew install binaryen`.

`npx @crowdedkingdoms/ckx-kit doctor` says what is missing.

## Start a mod

```bash
npx @crowdedkingdoms/ckx-kit new mod my-mod
cd my-mod
cargo test                                  # your logic, natively
npx @crowdedkingdoms/ckx-kit build --mod    # the platform's build and checks
```

`build` writes the module to `target/ckx-kit/`. When it passes, build and deploy the same sources
on the platform: Crowdy Studio, or `execModBuild` then `execModDeploy` (see
[mods](https://docs.crowdedkingdoms.com/exec/mods)).

## Commands

| Command | What it does |
|---|---|
| `ckx-kit new <template> <name>` | A crate to start from: `mod`, `client` (a mod's CLIENT half), `hub`, `spoke`, or a starter pack crate (`world-tick`, `matchmaker`, `session`, `npc-mobs`). |
| `ckx-kit check [dir]` | The platform's source checks, on exactly the files it takes (`Cargo.toml`, `README.md`, `.rs` files under `src/`). Lists what it would not send. |
| `ckx-kit build [dir]` | The checks, then the pinned Rust for `wasm32-unknown-unknown` with the platform's release profile and the platform's dependency versions, then the module check. `--mod` adds the mod limits. A CLIENT half is metered and optimized as the platform does it, and its capability summary, the list visitors are asked to agree to, is printed with its hash. `--json` prints a report. |
| `ckx-kit doctor` | What a build here needs, and which platform toolchain this kit matches. |

`CKX_KIT_TARGET_DIR` shares one cargo target directory between crates, so their dependencies
compile once.

## What it does not check

Fuel, memory and deadlines are measured only where the module runs: on the platform. Your
crate's native `cargo test` runs your logic, not the platform's limits. The platform also rebuilds
from your source, so its module's digest differs from yours (the build paths embedded in it
differ); the checks and the CLIENT capability hash do not.

## Versions

Crowded Kingdoms runs three environments, and each has its own builds:

| Environment | npm | `ckx-sdk` and `crowdy-client-sdk` on crates.io |
|---|---|---|
| dev | `@crowdedkingdoms/ckx-kit@dev` | `X.Y.Z-dev.N` |
| test | `@crowdedkingdoms/ckx-kit@test` | `X.Y.Z-test.N` |
| production | `@crowdedkingdoms/ckx-kit` (`latest`) | `X.Y.Z` |

Use the kit of the environment you deploy to. A plain requirement such as `ckx-sdk = "0.8"` only
ever matches production builds; `ckx-kit new` writes the exact version its environment uses.
`kit.pin` names the platform toolchain release a kit was copied from.

## Contributing

Issues and pull requests are welcome; see [CONTRIBUTING.md](CONTRIBUTING.md). The paths `kit.pin`
lists are copies of the platform's own and change upstream.

## License

Licensed under either of the [MIT license](LICENSE-MIT) or the
[Apache License, Version 2.0](LICENSE-APACHE), at your option.
