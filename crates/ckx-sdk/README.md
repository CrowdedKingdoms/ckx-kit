# ckx-sdk

Write [ck-exec](https://docs.crowdedkingdoms.com/exec/intro) hubs, spokes and mods for Crowded
Kingdoms in Rust. A hub owns state and runs one handler at a time; a spoke holds no authoritative
state and scales out; a [mod](https://docs.crowdedkingdoms.com/exec/mods) is a player's code on a
grid they own, run as a hub. Each is a plain Rust type exported with `export_hub!` or
`export_spoke!` and built for `wasm32-unknown-unknown`.

```rust
use ckx_sdk::prelude::*;

#[derive(Default, Serialize, Deserialize)]
struct Counter { total: i64 }

impl Hub for Counter {
    fn spawn(_ctx: &Ctx, _seed: &[u8]) -> Result<Self> { Ok(Self::default()) }
    fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> { decode(snapshot) }
    fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> { encode(self) }
    fn handle(&mut self, _ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        match call.method {
            "add" => { self.total += call.decode::<i64>()?; encode(&self.total) }
            other => Err(Error::unknown_method(other)),
        }
    }
}

ckx_sdk::export_hub!(Counter);
```

## Building

The platform builds your crate from its source (`execBuild`, `execModBuild`, or Crowdy Studio)
against its own copy of this crate, so the version you name here is for building, testing and
editing on your machine. [`@crowdedkingdoms/ckx-kit`](https://www.npmjs.com/package/@crowdedkingdoms/ckx-kit)
runs the platform's checks and build locally.

## Versions

Each Crowded Kingdoms environment has its own builds: `X.Y.Z-dev.N` for dev, `X.Y.Z-test.N` for
test, and plain `X.Y.Z` for production. A plain requirement such as `ckx-sdk = "0.7"` only ever
matches production builds; name a dev or test build exactly, for example
`ckx-sdk = "=0.7.0-dev.1"`.

## License

Licensed under either of the [MIT license](LICENSE-MIT) or the
[Apache License, Version 2.0](LICENSE-APACHE), at your option.
