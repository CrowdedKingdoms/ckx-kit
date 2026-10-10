//! A hub: state that one instance per key owns, changed by one handler at a time.
//!
//! Endpoints: `add` (players add to the total) and `get` (anyone reads it).

use ckx_sdk::prelude::*;

#[derive(Default, Serialize, Deserialize)]
pub struct Counter {
    pub total: i64,
}

impl Hub for Counter {
    fn spawn(_ctx: &Ctx, _seed: &[u8]) -> Result<Self> {
        Ok(Self::default())
    }

    fn load(_ctx: &Ctx, snapshot: &[u8], _from_version: u64) -> Result<Self> {
        decode(snapshot)
    }

    fn persist(&mut self, _ctx: &Ctx) -> Result<Vec<u8>> {
        encode(self)
    }

    fn handle(&mut self, _ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        match call.method {
            "add" => {
                call.player()?;
                self.total += call.decode::<i64>()?;
                encode(&self.total)
            }
            "get" => encode(&self.total),
            other => Err(Error::unknown_method(other)),
        }
    }
}

ckx_sdk::export_hub!(Counter);

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx { app: 1, instance: 1, epoch: 1, version: 1, kind: Kind::Hub, node_type: "counter".into(), key: "main".into() }
    }

    #[test]
    fn players_add_and_anyone_reads() {
        let ctx = ctx();
        let mut hub = Counter::spawn(&ctx, &[]).unwrap();
        let five = encode(&5i64).unwrap();
        hub.handle(&ctx, Call { caller: Caller::Player(7), method: "add", payload: &five }).unwrap();
        let total: i64 = decode(&hub.handle(&ctx, Call { caller: Caller::Instance(2), method: "get", payload: &[] }).unwrap()).unwrap();
        assert_eq!(total, 5);
    }

    #[test]
    fn only_players_add() {
        let ctx = ctx();
        let mut hub = Counter::spawn(&ctx, &[]).unwrap();
        let one = encode(&1i64).unwrap();
        assert!(hub.handle(&ctx, Call { caller: Caller::Instance(2), method: "add", payload: &one }).is_err());
    }

    #[test]
    fn the_snapshot_restores_the_total() {
        let ctx = ctx();
        let mut hub = Counter { total: 42 };
        let snapshot = hub.persist(&ctx).unwrap();
        assert_eq!(Counter::load(&ctx, &snapshot, 1).unwrap().total, 42);
    }
}
