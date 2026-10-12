# crowdy-client-sdk

Write the CLIENT half of a Crowded Kingdoms [mod](https://docs.crowdedkingdoms.com/exec/mods) in
Rust: WebAssembly that runs in the browser of each player who visits the mod's grid and agrees to
it. It draws a HUD or an overlay, reads the world around the player, and acts in the grid as that
player. CrowdyJS runs it; see [CLIENT halves](https://docs.crowdedkingdoms.com/exec/client-halves).

## Building

The platform builds the crate from its source (`execModClientBuild`, or Crowdy Studio's CLIENT
target) against its own copy of this crate, meters it, and derives the capability summary that
visitors consent to. [`@crowdedkingdoms/ckx-kit`](https://www.npmjs.com/package/@crowdedkingdoms/ckx-kit)
does the same on your machine.

## Versions

Each Crowded Kingdoms environment has its own builds: `X.Y.Z-dev.N` for dev, `X.Y.Z-test.N` for
test, and plain `X.Y.Z` for production. A plain requirement such as `crowdy-client-sdk = "0.1"`
only ever matches production builds; name a dev or test build exactly, for example
`crowdy-client-sdk = "=0.1.0-dev.1"`.

## License

Licensed under either of the [MIT license](LICENSE-MIT) or the
[Apache License, Version 2.0](LICENSE-APACHE), at your option.
