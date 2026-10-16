//! Realtime ability casts: cooldown/resource/range validation books,
//! server-stepped projectiles with tolerance windows, and AoE resolution
//! with falloff. Pure state machines — the hub feeds live
//! actor positions in and applies the resulting damage itself (see
//! [`super::combat`]). All stepping is bounded (max lifetime / max range) so fuel
//! stays predictable at a hub's tick rate.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What a cast does once validated.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum AbilityKind {
    /// Immediate hit on a target in range (hitscan).
    Instant { damage: i64 },
    /// A stepped projectile along a direction; first actor within
    /// `hit_radius` takes the damage.
    Projectile { speed: f32, damage: i64, max_range: f32, hit_radius: f32 },
    /// A delayed explosion at a target point; damage within `radius`,
    /// optionally scaled down linearly with distance.
    Aoe { radius: f32, damage: i64, delay_ms: u64, falloff: bool },
}

/// One ability's rules (usually from the hub's config).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AbilitySpec {
    pub ability_id: String,
    pub cooldown_ms: u64,
    pub resource_cost: i64,
    /// Max cast distance (target point / initial direction anchor).
    pub range: f32,
    pub kind: AbilityKind,
}

/// Per-caster server-held book: cooldown stamps + a regenerating resource
/// pool. One book per user, kept in the hub's state.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CasterBook {
    /// ability id -> earliest next cast (ms).
    pub ready_at: BTreeMap<String, u64>,
    pub resource: i64,
    pub resource_max: i64,
    /// Resource points regenerated per second.
    pub regen_per_sec: i64,
    pub last_regen_ms: u64,
}

impl Default for CasterBook {
    fn default() -> Self {
        Self { ready_at: BTreeMap::new(), resource: 100, resource_max: 100, regen_per_sec: 5, last_regen_ms: 0 }
    }
}

impl CasterBook {
    /// Apply resource regeneration up to `now_ms`.
    pub fn regen(&mut self, now_ms: u64) {
        if self.last_regen_ms == 0 {
            self.last_regen_ms = now_ms;
            return;
        }
        let elapsed = now_ms.saturating_sub(self.last_regen_ms);
        let gained = (elapsed as i64 * self.regen_per_sec) / 1000;
        if gained > 0 {
            self.resource = (self.resource + gained).min(self.resource_max);
            // Keep the remainder window so slow regen still accrues.
            self.last_regen_ms += (gained * 1000 / self.regen_per_sec.max(1)) as u64;
        }
    }
}

fn dist(a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dz) = (a.0 - b.0, a.1 - b.1);
    (dx * dx + dz * dz).sqrt()
}

/// A validated cast, ready for the hub to act on.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidCast {
    pub ability_id: String,
    /// Normalized aim direction (projectiles).
    pub dir: (f32, f32),
    /// Clamped target point (instant/AoE).
    pub target: (f32, f32),
}

/// Validate a cast against the book: cooldown, resource, range. On success
/// the cooldown is armed and the resource deducted (the cast is committed).
pub fn validate_cast(
    spec: &AbilitySpec,
    book: &mut CasterBook,
    caster_pos: (f32, f32),
    target: (f32, f32),
    now_ms: u64,
) -> Result<ValidCast, &'static str> {
    book.regen(now_ms);
    if book.ready_at.get(&spec.ability_id).copied().unwrap_or(0) > now_ms {
        return Err("ability on cooldown");
    }
    if book.resource < spec.resource_cost {
        return Err("not enough resource");
    }
    let to_target = dist(caster_pos, target);
    if to_target > spec.range && !matches!(spec.kind, AbilityKind::Projectile { .. }) {
        return Err("target out of range");
    }
    // Aim direction: toward the target; degenerate aim defaults to +x.
    let dir = if to_target > 1e-3 { ((target.0 - caster_pos.0) / to_target, (target.1 - caster_pos.1) / to_target) } else { (1.0, 0.0) };
    book.ready_at.insert(spec.ability_id.clone(), now_ms + spec.cooldown_ms);
    book.resource -= spec.resource_cost;
    Ok(ValidCast { ability_id: spec.ability_id.clone(), dir, target })
}

// ---------------------------------------------------------------------------
// Projectiles
// ---------------------------------------------------------------------------

/// A live projectile the hub steps each tick.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Projectile {
    pub ability_id: String,
    pub caster_id: String,
    pub x: f32,
    pub z: f32,
    pub dir: (f32, f32),
    pub speed: f32,
    pub damage: i64,
    pub hit_radius: f32,
    pub traveled: f32,
    pub max_range: f32,
    pub launched_at_ms: u64,
}

/// The outcome of one projectile step.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectileStep {
    /// Still flying (new position recorded on the projectile).
    Flying,
    /// Hit `target_id` at the recorded position; despawn.
    Hit { target_id: String },
    /// Ran out of range or was blocked; despawn.
    Expired,
}

impl Projectile {
    pub fn launch(spec: &AbilitySpec, caster_id: &str, from: (f32, f32), cast: &ValidCast, now_ms: u64) -> Option<Self> {
        let AbilityKind::Projectile { speed, damage, max_range, hit_radius } = spec.kind else {
            return None;
        };
        Some(Self {
            ability_id: spec.ability_id.clone(),
            caster_id: caster_id.to_string(),
            x: from.0,
            z: from.1,
            dir: cast.dir,
            speed,
            damage,
            hit_radius,
            traveled: 0.0,
            max_range,
            launched_at_ms: now_ms,
        })
    }

    /// Advance `dt_ms`, sub-stepping so tick-rate simulation cannot tunnel
    /// through targets (tolerance window = hit_radius per sub-step).
    /// `hit_test(x, z, radius)` returns a hit target (never the caster) or
    /// None; `blocked(x, z)` marks walls.
    pub fn step(
        &mut self,
        dt_ms: u64,
        hit_test: &mut impl FnMut(f32, f32, f32) -> Option<String>,
        blocked: &mut impl FnMut(f32, f32) -> bool,
    ) -> ProjectileStep {
        let total = self.speed * dt_ms as f32 / 1000.0;
        let sub = self.hit_radius.max(0.25);
        let mut moved = 0.0f32;
        while moved < total {
            let advance = sub.min(total - moved);
            self.x += self.dir.0 * advance;
            self.z += self.dir.1 * advance;
            self.traveled += advance;
            moved += advance;
            if blocked(self.x, self.z) {
                return ProjectileStep::Expired;
            }
            if let Some(target_id) = hit_test(self.x, self.z, self.hit_radius) {
                return ProjectileStep::Hit { target_id };
            }
            if self.traveled >= self.max_range {
                return ProjectileStep::Expired;
            }
        }
        ProjectileStep::Flying
    }
}

// ---------------------------------------------------------------------------
// AoE
// ---------------------------------------------------------------------------

/// A delayed area effect awaiting detonation.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct PendingAoe {
    pub ability_id: String,
    pub caster_id: String,
    pub x: f32,
    pub z: f32,
    pub radius: f32,
    pub damage: i64,
    pub falloff: bool,
    pub detonate_at_ms: u64,
}

impl PendingAoe {
    pub fn from_cast(spec: &AbilitySpec, caster_id: &str, cast: &ValidCast, now_ms: u64) -> Option<Self> {
        let AbilityKind::Aoe { radius, damage, delay_ms, falloff } = spec.kind else {
            return None;
        };
        Some(Self {
            ability_id: spec.ability_id.clone(),
            caster_id: caster_id.to_string(),
            x: cast.target.0,
            z: cast.target.1,
            radius,
            damage,
            falloff,
            detonate_at_ms: now_ms + delay_ms,
        })
    }

    pub fn ready(&self, now_ms: u64) -> bool {
        now_ms >= self.detonate_at_ms
    }

    /// Damage per victim inside the radius (linear falloff to 25% at the
    /// edge when enabled). Victims = (id, position); the caster is exempt.
    pub fn resolve(&self, victims: &[(String, (f32, f32))]) -> Vec<(String, i64)> {
        victims
            .iter()
            .filter(|(id, _)| *id != self.caster_id)
            .filter_map(|(id, pos)| {
                let d = dist((self.x, self.z), *pos);
                if d > self.radius {
                    return None;
                }
                let damage = if self.falloff && self.radius > 0.0 {
                    let scale = 1.0 - 0.75 * (d / self.radius);
                    ((self.damage as f32) * scale).round() as i64
                } else {
                    self.damage
                };
                Some((id.clone(), damage.max(1)))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bolt() -> AbilitySpec {
        AbilitySpec {
            ability_id: "bolt".into(),
            cooldown_ms: 1_000,
            resource_cost: 10,
            range: 20.0,
            kind: AbilityKind::Projectile { speed: 10.0, damage: 5, max_range: 12.0, hit_radius: 0.5 },
        }
    }

    fn nova() -> AbilitySpec {
        AbilitySpec {
            ability_id: "nova".into(),
            cooldown_ms: 5_000,
            resource_cost: 30,
            range: 8.0,
            kind: AbilityKind::Aoe { radius: 4.0, damage: 20, delay_ms: 500, falloff: true },
        }
    }

    #[test]
    fn cast_validation_cooldown_resource_range() {
        let spec = nova();
        let mut book = CasterBook::default();
        // Out of range for a targeted AoE.
        assert_eq!(validate_cast(&spec, &mut book, (0.0, 0.0), (20.0, 0.0), 1_000).unwrap_err(), "target out of range");
        let cast = validate_cast(&spec, &mut book, (0.0, 0.0), (5.0, 0.0), 1_000).unwrap();
        assert_eq!(cast.target, (5.0, 0.0));
        assert_eq!(book.resource, 70);
        // Cooldown blocks an immediate re-cast.
        assert_eq!(validate_cast(&spec, &mut book, (0.0, 0.0), (5.0, 0.0), 1_100).unwrap_err(), "ability on cooldown");
        // After cooldown, resource gate: drain (regen off) then fail.
        book.resource = 5;
        book.regen_per_sec = 0;
        assert_eq!(validate_cast(&spec, &mut book, (0.0, 0.0), (5.0, 0.0), 7_000).unwrap_err(), "not enough resource");
    }

    #[test]
    fn resource_regenerates_over_time() {
        let mut book = CasterBook { resource: 0, last_regen_ms: 1_000, ..Default::default() };
        book.regen(3_000); // 2s at 5/s
        assert_eq!(book.resource, 10);
        book.regen(3_000);
        assert_eq!(book.resource, 10, "no double counting");
        book.regen(100_000);
        assert_eq!(book.resource, book.resource_max, "clamped at max");
    }

    #[test]
    fn projectile_flies_hits_and_expires() {
        let spec = bolt();
        let mut book = CasterBook::default();
        let cast = validate_cast(&spec, &mut book, (0.0, 0.0), (10.0, 0.0), 0).unwrap();
        let mut projectile = Projectile::launch(&spec, "caster", (0.0, 0.0), &cast, 0).unwrap();

        // A target at x=5 gets hit during the first second of flight.
        let mut hit = |x: f32, z: f32, r: f32| (dist((x, z), (5.0, 0.0)) <= r).then(|| "victim".to_string());
        let mut open = |_x: f32, _z: f32| false;
        let step = projectile.step(1_000, &mut hit, &mut open);
        assert_eq!(step, ProjectileStep::Hit { target_id: "victim".into() });
        assert!(projectile.traveled <= 5.5, "stops at the hit, no overshoot");

        // No targets: expires at max_range (12 units at 10 u/s).
        let cast = validate_cast(&spec, &mut book, (0.0, 0.0), (10.0, 0.0), 5_000).unwrap();
        let mut projectile = Projectile::launch(&spec, "caster", (0.0, 0.0), &cast, 5_000).unwrap();
        let mut none = |_x: f32, _z: f32, _r: f32| None;
        assert_eq!(projectile.step(1_000, &mut none, &mut open), ProjectileStep::Flying);
        assert_eq!(projectile.step(1_000, &mut none, &mut open), ProjectileStep::Expired);

        // Walls block flight.
        let cast = validate_cast(&spec, &mut book, (0.0, 0.0), (10.0, 0.0), 20_000).unwrap();
        let mut projectile = Projectile::launch(&spec, "caster", (0.0, 0.0), &cast, 20_000).unwrap();
        let mut wall = |x: f32, _z: f32| x >= 2.0;
        assert_eq!(projectile.step(1_000, &mut none, &mut wall), ProjectileStep::Expired);
        assert!(projectile.x <= 2.5);
    }

    #[test]
    fn aoe_delays_resolves_with_falloff_and_exempts_caster() {
        let spec = nova();
        let mut book = CasterBook::default();
        let cast = validate_cast(&spec, &mut book, (0.0, 0.0), (5.0, 0.0), 1_000).unwrap();
        let aoe = PendingAoe::from_cast(&spec, "caster", &cast, 1_000).unwrap();
        assert!(!aoe.ready(1_400));
        assert!(aoe.ready(1_500));
        let victims = vec![
            ("caster".to_string(), (5.0, 0.0)),   // exempt
            ("center".to_string(), (5.0, 0.0)),   // full damage
            ("edge".to_string(), (9.0, 0.0)),     // r=4 away -> 25%
            ("outside".to_string(), (10.5, 0.0)), // out
        ];
        let hits = aoe.resolve(&victims);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], ("center".to_string(), 20));
        assert_eq!(hits[1], ("edge".to_string(), 5));
    }

    #[test]
    fn serde_roundtrip() {
        let spec = bolt();
        let mut book = CasterBook::default();
        let cast = validate_cast(&spec, &mut book, (0.0, 0.0), (3.0, 4.0), 0).unwrap();
        let projectile = Projectile::launch(&spec, "c", (0.0, 0.0), &cast, 0).unwrap();
        let json = serde_json::to_string(&(&book, &projectile)).unwrap();
        let (book2, projectile2): (CasterBook, Projectile) = serde_json::from_str(&json).unwrap();
        assert_eq!(book2.resource, book.resource);
        assert_eq!(projectile2.dir, projectile.dir);
    }
}
