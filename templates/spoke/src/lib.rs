//! A spoke: stateless work that replicas run anywhere, changing hubs only through their
//! endpoints.
//!
//! Endpoint: `damage { attack, defense }` answers the damage of one hit.

use ckx_sdk::prelude::*;

#[derive(Serialize, Deserialize)]
pub struct Hit {
    pub attack: u32,
    pub defense: u32,
}

pub fn damage(hit: &Hit) -> u32 {
    hit.attack.saturating_sub(hit.defense / 2).max(1)
}

pub struct Combat;

impl Spoke for Combat {
    fn start(_ctx: &Ctx) -> Result<Self> {
        Ok(Self)
    }

    fn handle(&mut self, _ctx: &Ctx, call: Call<'_>) -> Result<Vec<u8>> {
        match call.method {
            "damage" => encode(&damage(&call.decode::<Hit>()?)),
            other => Err(Error::unknown_method(other)),
        }
    }
}

ckx_sdk::export_spoke!(Combat);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hit_does_at_least_one_damage() {
        assert_eq!(damage(&Hit { attack: 10, defense: 4 }), 8);
        assert_eq!(damage(&Hit { attack: 1, defense: 100 }), 1);
    }

    #[test]
    fn the_endpoint_answers_in_messagepack() {
        let ctx = Ctx { app: 1, instance: 1, epoch: 1, version: 1, kind: Kind::Spoke, node_type: "combat".into(), key: String::new() };
        let mut spoke = Combat::start(&ctx).unwrap();
        let hit = encode(&Hit { attack: 10, defense: 4 }).unwrap();
        let reply = spoke.handle(&ctx, Call { caller: Caller::Instance(3), method: "damage", payload: &hit }).unwrap();
        assert_eq!(decode::<u32>(&reply).unwrap(), 8);
    }
}
