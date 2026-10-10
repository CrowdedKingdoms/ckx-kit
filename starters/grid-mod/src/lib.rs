//! A mod: a player's code on a grid they own, deployed with `execModDeploy` rather than in the
//! app's manifest. One instance runs per grid, keyed by the grid id, in its owner's own
//! sandbox; its node API calls reach only that grid, and only while the owner still owns it.
//! It calls no other node, and only players call it.
//!
//! Endpoints: `visit` (players), `state` (anyone) and `probe`, which tries to call the app's
//! root hub and returns the refusal. What happens in the grid arrives at `on_world`.

use ckx_sdk::prelude::*;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct GridMod {
    pub grid: u64,
    pub visits: u64,
    pub last_visitor: Option<u64>,
    /// World events heard, by kind, and the chunk of the last one.
    pub actors: u64,
    pub voxels: u64,
    pub last_chunk: Option<[i64; 3]>,
}

impl GridMod {
    fn heard(&mut self, events: &[WorldEvent]) {
        for e in events {
            match e.kind.as_str() {
                WorldEvent::ACTORS => self.actors += 1,
                WorldEvent::VOXELS => self.voxels += 1,
                _ => continue,
            }
            self.last_chunk = Some(e.chunk);
        }
    }
}

impl Hub for GridMod {
    fn spawn(ctx: &Ctx, _seed: &[u8]) -> Result<Self> {
        let grid = ctx.key.parse().map_err(|_| Error::new("a mod's key is its grid id"))?;
        Ok(Self { grid, ..Self::default() })
    }

    fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
        decode(snapshot)
    }

    fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
        encode(self)
    }

    fn handle(&mut self, ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        match call.method {
            "visit" => {
                self.last_visitor = Some(call.player()?);
                self.visits += 1;
                encode(&format!("welcome to grid {}, visitor {}", self.grid, self.visits))
            }
            "state" => encode(&*self),
            "probe" => encode(&ctx.call("world", "", "state", &encode(&())?).err().map(|e| e.to_string())),
            m => Err(Error(format!("no endpoint `{m}`"))),
        }
    }

    fn on_world(&mut self, ctx: &Ctx, events: &[WorldEvent]) -> Result<()> {
        self.heard(events);
        ctx.persist_now();
        Ok(())
    }
}

ckx_sdk::export_hub!(GridMod);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn world_events_are_counted_by_kind() {
        let mut m = GridMod { grid: 7, ..GridMod::default() };
        let e = |kind: &str, x| WorldEvent { grid: 7, kind: kind.into(), chunk: [x, 0, 0] };
        m.heard(&[e(WorldEvent::ACTORS, 1), e(WorldEvent::VOXELS, 2), e("weather", 3)]);
        assert_eq!((m.actors, m.voxels, m.last_chunk), (1, 1, Some([2, 0, 0])));
    }
}
