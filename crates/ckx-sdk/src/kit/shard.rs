//! Shard keys: one keyed hub per square of the world, so no one hub holds everyone.
//!
//! A shard is `size` by `size` chunks on the x and z axes, and its key is its centre chunk,
//! `"<x>,<z>"`. Players' clients compute the same keys (the kit clients carry this function),
//! so a client finds the hub for where it stands without asking anyone. A hub that sees its
//! neighbours' edges subscribes to [`around`]'s keys.

/// The shard's centre chunk for chunk `(x, z)`. `size` is at least 1.
pub fn centre(x: i64, z: i64, size: i64) -> (i64, i64) {
    let size = size.max(1);
    (x.div_euclid(size) * size + size / 2, z.div_euclid(size) * size + size / 2)
}

/// The key of the shard that holds chunk `(x, z)`.
pub fn key(x: i64, z: i64, size: i64) -> String {
    let (cx, cz) = centre(x, z, size);
    format!("{cx},{cz}")
}

/// The centre chunk a key names, or `None` when it is not `"<x>,<z>"`.
pub fn parse(key: &str) -> Option<(i64, i64)> {
    let (x, z) = key.split_once(',')?;
    Some((x.trim().parse().ok()?, z.trim().parse().ok()?))
}

/// The keys of the shards within `radius` shards of the one holding `(x, z)`, its own first,
/// then row by row.
pub fn around(x: i64, z: i64, size: i64, radius: i64) -> Vec<String> {
    let size = size.max(1);
    let (cx, cz) = centre(x, z, size);
    let mut keys = vec![format!("{cx},{cz}")];
    for dz in -radius..=radius {
        for dx in -radius..=radius {
            if (dx, dz) != (0, 0) {
                keys.push(format!("{},{}", cx + dx * size, cz + dz * size));
            }
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chunk_falls_in_one_shard_named_by_its_centre() {
        assert_eq!(key(10, -4, 8), "12,-4");
        assert_eq!(key(8, -8, 8), "12,-4");
        assert_eq!(key(7, -9, 8), "4,-12");
        assert_eq!(key(-1, 0, 16), "-8,8");
        assert_eq!(key(3, 5, 1), "3,5", "size 1 is the chunk itself");
        assert_eq!(parse("12,-4"), Some((12, -4)));
        assert_eq!(parse("x"), None);
    }

    #[test]
    fn around_starts_with_its_own_shard() {
        let keys = around(0, 0, 8, 1);
        assert_eq!(keys.len(), 9);
        assert_eq!(keys[0], "4,4");
        assert!(keys.contains(&"-4,-4".to_string()) && keys.contains(&"12,12".to_string()));
        assert_eq!(around(0, 0, 8, 0), vec!["4,4".to_string()]);
    }
}
