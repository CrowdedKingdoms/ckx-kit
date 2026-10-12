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
    use ckx_sdk::testing::TestHub;

    use super::*;

    #[test]
    fn players_add_and_anyone_reads() {
        let mut hub = TestHub::<Counter>::spawn("counter", "main").unwrap();
        let total: i64 = hub.call_typed(Caller::Player(7), "add", &5i64).unwrap();
        assert_eq!(total, 5);
        let read: i64 = hub.call_typed(Caller::Instance(2), "get", &()).unwrap();
        assert_eq!(read, 5);
    }

    #[test]
    fn only_players_add() {
        let mut hub = TestHub::<Counter>::spawn("counter", "main").unwrap();
        let refused = hub.call_typed::<_, i64>(Caller::Instance(2), "add", &1i64).unwrap_err();
        assert_eq!(refused.status, Status::AppError);
        assert_eq!(hub.hub().total, 0);
    }

    #[test]
    fn the_total_survives_a_restart() {
        let mut hub = TestHub::<Counter>::spawn("counter", "main").unwrap();
        hub.call_typed::<_, i64>(Caller::Player(7), "add", &42i64).unwrap();
        hub.restart().unwrap();
        assert_eq!(hub.hub().total, 42);
    }
}
