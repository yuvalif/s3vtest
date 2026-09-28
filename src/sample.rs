//! Numbers that are either constant or sampled from a range, and the seeded RNG.

use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};

pub type SeededRng = ChaCha8Rng;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum NumSpec {
    Const(u64),
    Range { min: u64, max: u64 },
}

impl NumSpec {
    pub fn sample(&self, rng: &mut impl Rng) -> u64 {
        match *self {
            NumSpec::Const(v) => v,
            NumSpec::Range { min, max } => rng.random_range(min..=max),
        }
    }
}

impl std::fmt::Display for NumSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NumSpec::Const(v) => write!(f, "{v}"),
            NumSpec::Range { min, max } => write!(f, "[{min}, {max}]"),
        }
    }
}

/// Derive an independent RNG for a work item from the job seed and a set of
/// discriminators, so that concurrent items are deterministic regardless of
/// scheduling order.
pub fn derive_rng(seed: u64, parts: &[u64]) -> SeededRng {
    // FNV-1a style mixing; quality is irrelevant here, determinism is.
    let mut h: u64 = 0xcbf29ce484222325 ^ seed;
    for p in parts {
        for b in p.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    SeededRng::seed_from_u64(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_forms() {
        let c: NumSpec = serde_yaml_ng::from_str("5").unwrap();
        assert_eq!(c, NumSpec::Const(5));
        let r: NumSpec = serde_yaml_ng::from_str("{min: 1, max: 3}").unwrap();
        assert_eq!(r, NumSpec::Range { min: 1, max: 3 });
    }

    #[test]
    fn derived_rng_is_deterministic() {
        let a: u64 = derive_rng(1, &[2, 3]).random();
        let b: u64 = derive_rng(1, &[2, 3]).random();
        let c: u64 = derive_rng(1, &[3, 2]).random();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
