//! Monotonic single-increasing revision counter.
//!
//! Semantics match `rev_clock.dart` — every write increments the counter so
//! sync can determine "who is newer" without wall-clock comparison.

use serde::{Deserialize, Serialize};

/// A monotonically increasing revision counter.
///
/// Two clocks can be compared: the higher value is "newer". Equality means
/// they represent the same revision. Crossing language boundaries (Dart ↔ Rust)
/// the comparison must be done on the same serialised integer value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RevClock(pub u64);

impl RevClock {
    pub const ZERO: RevClock = RevClock(0);

    pub fn new(v: u64) -> Self {
        RevClock(v)
    }

    pub fn increment(self) -> Self {
        RevClock(self.0.saturating_add(1))
    }

    pub fn value(self) -> u64 {
        self.0
    }
}

impl Default for RevClock {
    fn default() -> Self {
        RevClock::ZERO
    }
}

impl std::fmt::Display for RevClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "r{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering() {
        let a = RevClock::new(5);
        let b = RevClock::new(6);
        assert!(b > a);
        assert_eq!(a.increment(), b);
    }

    #[test]
    fn saturation() {
        assert_eq!(RevClock(u64::MAX).increment(), RevClock(u64::MAX));
    }
}
