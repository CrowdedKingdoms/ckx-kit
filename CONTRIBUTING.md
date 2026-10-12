# Contributing

Issues and pull requests are welcome.

## What changes here, and what changes upstream

`kit.pin` lists the paths that are byte-identical copies of the platform's own compute toolchain:
the two SDK crates' sources, `instrument`, the build checks and their specs, the host catalog, the
starters, the Rust pin and `vendor.lock`. They are never edited in this repository. A fix to one of
them is made by the platform team in the platform itself and arrives here with the next sync, so a
pull request that changes a copied file is closed with a pointer to where the change landed, or an
issue to track it.

Everything else is this repository's own, and a pull request here changes it directly:

- the `ckx-kit` command (`cli/`) and its tests;
- the templates (`templates/`);
- the crates' READMEs, the docs and the workflows.

## Before a pull request

```bash
npm ci
npm run typecheck
npm test                 # the platform's check specs, unchanged
npm run build
node --test cli/test/cli.test.mjs    # needs Rust (the version in rust-toolchain.toml) and binaryen
npm run policy           # nothing here may name private infrastructure
```

## Branches

`dev`, `test` and `prod` follow the platform's three environments. Pull requests go to `dev`.
