//! Small helpers we would otherwise pull a crate in for.

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

thread_local! {
    static RNG_STATE: Cell<u64> = const { Cell::new(0) };
}

/// A thread-local xorshift64* generator.
///
/// Balancing needs cheap, unbiased-enough randomness for power-of-two-choices;
/// it does not need cryptographic quality, and `rand` would be the single
/// largest dependency in the tree for this one use.
pub fn fast_rand() -> u64 {
    RNG_STATE.with(|cell| {
        let mut x = cell.get();
        if x == 0 {
            // Seed from the clock plus the address of the cell itself, so two
            // threads starting in the same millisecond diverge.
            x = now_millis()
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(cell as *const _ as u64)
                | 1;
        }
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        cell.set(x);
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    })
}

/// Uniform value in `0..n`. Returns 0 for n == 0.
pub fn rand_below(n: usize) -> usize {
    if n <= 1 {
        return 0;
    }
    (fast_rand() % n as u64) as usize
}

/// Compare two secrets without leaking their contents through timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Format a float for Prometheus exposition without scientific notation.
pub fn fmt_metric(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{:.0}", v)
    } else {
        format!("{:.3}", v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rand_below_stays_in_range() {
        for n in [1usize, 2, 3, 7, 64] {
            for _ in 0..500 {
                assert!(rand_below(n) < n, "out of range for n={n}");
            }
        }
        assert_eq!(rand_below(0), 0);
    }

    #[test]
    fn rand_is_not_constant() {
        let first = fast_rand();
        assert!((0..100).any(|_| fast_rand() != first));
    }

    #[test]
    fn rand_below_two_covers_both_arms() {
        let mut seen = [false; 2];
        for _ in 0..200 {
            seen[rand_below(2)] = true;
        }
        assert_eq!(seen, [true, true]);
    }

    #[test]
    fn constant_time_eq_matches_normal_eq() {
        assert!(constant_time_eq(b"sk-abc", b"sk-abc"));
        assert!(!constant_time_eq(b"sk-abc", b"sk-abd"));
        assert!(!constant_time_eq(b"sk-abc", b"sk-abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn metric_formatting_is_plain() {
        assert_eq!(fmt_metric(3.0), "3");
        assert_eq!(fmt_metric(3.25), "3.250");
    }
}
