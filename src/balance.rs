//! Upstream selection.
//!
//! Selection is deliberately pure: it takes the candidate list and returns one
//! of them. Filtering (health, model, capacity) happens before we get here, so
//! the strategies stay easy to reason about and easy to test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::config::Strategy;
use crate::upstream::Upstream;
use crate::util::rand_below;

/// Latency assumed for an upstream that has never answered, in microseconds.
/// Optimistic on purpose: a fresh box should get a chance to prove itself.
const UNKNOWN_LATENCY_US: u64 = 1_000;

#[derive(Debug)]
pub struct Balancer {
    strategy: Strategy,
    cursor: AtomicUsize,
}

impl Balancer {
    pub fn new(strategy: Strategy) -> Self {
        Self {
            strategy,
            cursor: AtomicUsize::new(0),
        }
    }

    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// Pick one upstream out of `candidates`, or `None` if the list is empty.
    pub fn select(&self, candidates: &[Arc<Upstream>]) -> Option<Arc<Upstream>> {
        match candidates.len() {
            0 => return None,
            1 => return Some(candidates[0].clone()),
            _ => {}
        }
        let chosen = match self.strategy {
            Strategy::FirstAvailable => 0,
            Strategy::RoundRobin => self.next_cursor() % candidates.len(),
            Strategy::LeastConn => {
                // Rotate the starting point so equal-load upstreams share the
                // traffic instead of all landing on index 0.
                let offset = self.next_cursor();
                let mut best = 0usize;
                let mut best_load = usize::MAX;
                for i in 0..candidates.len() {
                    let idx = (offset + i) % candidates.len();
                    let load = candidates[idx].inflight();
                    if load < best_load {
                        best_load = load;
                        best = idx;
                    }
                }
                best
            }
            Strategy::Weighted => {
                let total: u64 = candidates.iter().map(|c| c.cfg.weight.max(1) as u64).sum();
                let mut ticket = (crate::util::fast_rand() % total.max(1)) as i64;
                let mut chosen = candidates.len() - 1;
                for (i, c) in candidates.iter().enumerate() {
                    ticket -= c.cfg.weight.max(1) as i64;
                    if ticket < 0 {
                        chosen = i;
                        break;
                    }
                }
                chosen
            }
            Strategy::P2cLatency => {
                let a = rand_below(candidates.len());
                let mut b = rand_below(candidates.len());
                if b == a {
                    b = (a + 1) % candidates.len();
                }
                if cost(&candidates[a]) <= cost(&candidates[b]) {
                    a
                } else {
                    b
                }
            }
        };
        Some(candidates[chosen].clone())
    }

    fn next_cursor(&self) -> usize {
        self.cursor.fetch_add(1, Ordering::Relaxed)
    }
}

/// Estimated time this upstream would need to answer: queue depth times the
/// latency it has been showing. This is what makes a slow board shed load to a
/// fast one instead of both being hammered equally.
fn cost(up: &Upstream) -> u64 {
    let latency = match up.ewma_us() {
        0 => UNKNOWN_LATENCY_US,
        v => v,
    };
    latency.saturating_mul(up.inflight() as u64 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UpstreamConfig;
    use std::collections::HashMap;
    use std::time::Duration;

    fn upstream(name: &str, weight: u32) -> Arc<Upstream> {
        Arc::new(Upstream::new(UpstreamConfig {
            name: name.into(),
            url: "http://127.0.0.1:1/v1".into(),
            api_key: None,
            api_key_env: None,
            weight,
            max_concurrency: 4,
            models: vec![],
            fallback_only: false,
        }))
    }

    fn counts(picks: impl Iterator<Item = Arc<Upstream>>) -> HashMap<String, usize> {
        let mut m = HashMap::new();
        for p in picks {
            *m.entry(p.name.clone()).or_insert(0) += 1;
        }
        m
    }

    #[test]
    fn empty_candidates_select_nothing() {
        let b = Balancer::new(Strategy::RoundRobin);
        assert!(b.select(&[]).is_none());
    }

    #[test]
    fn round_robin_rotates_in_order() {
        let b = Balancer::new(Strategy::RoundRobin);
        let ups = vec![upstream("a", 1), upstream("b", 1), upstream("c", 1)];
        let seq: Vec<String> = (0..6)
            .map(|_| b.select(&ups).unwrap().name.clone())
            .collect();
        assert_eq!(seq, ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn least_conn_prefers_the_idle_upstream() {
        let b = Balancer::new(Strategy::LeastConn);
        let ups = vec![upstream("busy", 1), upstream("idle", 1)];
        for _ in 0..3 {
            ups[0].incr_inflight();
        }
        for _ in 0..10 {
            assert_eq!(b.select(&ups).unwrap().name, "idle");
        }
    }

    #[test]
    fn least_conn_spreads_ties() {
        let b = Balancer::new(Strategy::LeastConn);
        let ups = vec![upstream("a", 1), upstream("b", 1)];
        let c = counts((0..100).map(|_| b.select(&ups).unwrap()));
        assert_eq!(c["a"], 50);
        assert_eq!(c["b"], 50);
    }

    #[test]
    fn weighted_follows_the_weights() {
        let b = Balancer::new(Strategy::Weighted);
        let ups = vec![upstream("big", 9), upstream("small", 1)];
        let c = counts((0..4000).map(|_| b.select(&ups).unwrap()));
        let big = *c.get("big").unwrap_or(&0);
        // Expect ~90%; allow generous slack so the test is not flaky.
        assert!((3200..3900).contains(&big), "big got {big}/4000");
    }

    #[test]
    fn p2c_prefers_the_faster_upstream() {
        let b = Balancer::new(Strategy::P2cLatency);
        let ups = vec![upstream("slow", 1), upstream("fast", 1)];
        ups[0].record_latency(Duration::from_millis(900));
        ups[1].record_latency(Duration::from_millis(30));
        let c = counts((0..200).map(|_| b.select(&ups).unwrap()));
        assert_eq!(*c.get("fast").unwrap_or(&0), 200);
    }

    #[test]
    fn p2c_accounts_for_queue_depth() {
        let b = Balancer::new(Strategy::P2cLatency);
        let ups = vec![upstream("queued", 1), upstream("free", 1)];
        // Same latency, but one already has three requests waiting.
        ups[0].record_latency(Duration::from_millis(100));
        ups[1].record_latency(Duration::from_millis(100));
        for _ in 0..3 {
            ups[0].incr_inflight();
        }
        let c = counts((0..200).map(|_| b.select(&ups).unwrap()));
        assert_eq!(*c.get("free").unwrap_or(&0), 200);
    }

    #[test]
    fn first_available_is_sticky() {
        let b = Balancer::new(Strategy::FirstAvailable);
        let ups = vec![upstream("primary", 1), upstream("spare", 1)];
        for _ in 0..10 {
            assert_eq!(b.select(&ups).unwrap().name, "primary");
        }
    }

    #[test]
    fn single_candidate_is_returned_by_every_strategy() {
        let ups = vec![upstream("only", 1)];
        for s in [
            Strategy::RoundRobin,
            Strategy::LeastConn,
            Strategy::Weighted,
            Strategy::P2cLatency,
            Strategy::FirstAvailable,
        ] {
            let b = Balancer::new(s);
            assert_eq!(b.select(&ups).unwrap().name, "only", "strategy {s}");
        }
    }
}
