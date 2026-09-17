//! Candidate ordering.
//!
//! Selection produces an *ordered* list rather than a single pick, because
//! spillover is the point: when the first candidate fails the router walks to
//! the next one, and the next, until something answers or the list runs out.
//!
//! Ordering is pure -- it takes candidates and returns them rearranged -- so
//! each strategy stays easy to reason about and easy to test.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::config::Strategy;
use crate::protocol::Protocol;
use crate::upstream::Upstream;
use crate::util::{fast_rand, rand_below};

/// Latency assumed for a provider that has never answered, in microseconds.
/// Optimistic on purpose: a fresh provider should get a chance to prove itself.
const UNKNOWN_LATENCY_US: u64 = 1_000;

/// One place a request could go: a provider, and the model id to ask it for.
#[derive(Debug, Clone)]
pub struct Target {
    pub upstream: Arc<Upstream>,
    /// The provider-side model id, which is rarely what the client asked for.
    pub model: String,
    /// Relative share under the `weighted` strategy.
    pub weight: u32,
}

impl Target {
    pub fn new(upstream: Arc<Upstream>, model: impl Into<String>, weight: u32) -> Self {
        Self {
            upstream,
            model: model.into(),
            weight: weight.max(1),
        }
    }

    pub fn protocol(&self) -> Protocol {
        self.upstream.cfg.protocol
    }

    /// How this target identifies itself in logs and errors.
    pub fn label(&self) -> String {
        format!("{}:{}", self.upstream.name, self.model)
    }
}

#[derive(Debug)]
pub struct Balancer {
    default_strategy: Strategy,
    cursor: AtomicUsize,
}

impl Balancer {
    pub fn new(default_strategy: Strategy) -> Self {
        Self {
            default_strategy,
            cursor: AtomicUsize::new(0),
        }
    }

    pub fn strategy(&self) -> Strategy {
        self.default_strategy
    }

    /// Order the candidates for one request. The first entry is tried first;
    /// the rest are the spillover path, in order.
    pub fn order(&self, strategy: Option<Strategy>, mut targets: Vec<Target>) -> Vec<Target> {
        let strategy = strategy.unwrap_or(self.default_strategy);
        if targets.len() < 2 {
            return targets;
        }
        match strategy {
            // Declaration order is the configuration talking. Leave it alone.
            Strategy::Priority => targets,
            Strategy::RoundRobin => {
                let offset = self.next_cursor() % targets.len();
                targets.rotate_left(offset);
                targets
            }
            Strategy::LeastConn => {
                // Rotate first so equal-load candidates share the traffic
                // instead of all landing on index 0.
                let offset = self.next_cursor() % targets.len();
                targets.rotate_left(offset);
                targets.sort_by_key(|t| t.upstream.inflight());
                targets
            }
            Strategy::Weighted => weighted_shuffle(targets),
            Strategy::P2cLatency => {
                // Power of two choices decides who goes first; the rest fall
                // in behind by the same cost estimate.
                let a = rand_below(targets.len());
                let mut b = rand_below(targets.len());
                if b == a {
                    b = (a + 1) % targets.len();
                }
                let winner = if cost(&targets[a]) <= cost(&targets[b]) {
                    a
                } else {
                    b
                };
                let first = targets.remove(winner);
                targets.sort_by_key(cost);
                targets.insert(0, first);
                targets
            }
        }
    }

    fn next_cursor(&self) -> usize {
        self.cursor.fetch_add(1, Ordering::Relaxed)
    }
}

/// Repeated weighted draws without replacement.
fn weighted_shuffle(mut targets: Vec<Target>) -> Vec<Target> {
    let mut out = Vec::with_capacity(targets.len());
    while !targets.is_empty() {
        let total: u64 = targets.iter().map(|t| t.weight.max(1) as u64).sum();
        let mut ticket = (fast_rand() % total.max(1)) as i64;
        let mut chosen = targets.len() - 1;
        for (i, t) in targets.iter().enumerate() {
            ticket -= t.weight.max(1) as i64;
            if ticket < 0 {
                chosen = i;
                break;
            }
        }
        out.push(targets.remove(chosen));
    }
    out
}

/// Estimated time this provider would need to answer: queue depth times the
/// latency it has been showing. This is what makes a struggling provider shed
/// load to a fast one instead of both being hammered equally.
fn cost(target: &Target) -> u64 {
    let latency = match target.upstream.ewma_us() {
        0 => UNKNOWN_LATENCY_US,
        v => v,
    };
    latency.saturating_mul(target.upstream.inflight() as u64 + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::UpstreamConfig;
    use std::collections::HashMap;
    use std::time::Duration;

    fn upstream(name: &str) -> Arc<Upstream> {
        Arc::new(Upstream::new(UpstreamConfig {
            name: name.into(),
            url: "http://127.0.0.1:1/v1".into(),
            protocol: Protocol::Openai,
            api_key: None,
            api_key_env: None,
            weight: 1,
            max_concurrency: 4,
            models: vec![],
            fallback_only: false,
            headers: Default::default(),
        }))
    }

    fn target(name: &str, weight: u32) -> Target {
        Target::new(upstream(name), format!("{name}-model"), weight)
    }

    fn names(targets: &[Target]) -> Vec<String> {
        targets.iter().map(|t| t.upstream.name.clone()).collect()
    }

    fn first_counts(b: &Balancer, targets: &[Target], n: usize) -> HashMap<String, usize> {
        let mut m = HashMap::new();
        for _ in 0..n {
            let ordered = b.order(None, targets.to_vec());
            *m.entry(ordered[0].upstream.name.clone()).or_insert(0) += 1;
        }
        m
    }

    #[test]
    fn empty_and_single_candidate_lists_are_returned_as_is() {
        let b = Balancer::new(Strategy::P2cLatency);
        assert!(b.order(None, vec![]).is_empty());
        assert_eq!(names(&b.order(None, vec![target("only", 1)])), ["only"]);
    }

    #[test]
    fn priority_never_reorders() {
        let b = Balancer::new(Strategy::Priority);
        let t = vec![target("a", 1), target("b", 1), target("c", 1)];
        // Even with wildly different load and latency, order is configuration.
        t[0].upstream.record_latency(Duration::from_secs(5));
        for _ in 0..10 {
            t[0].upstream.incr_inflight();
        }
        for _ in 0..5 {
            assert_eq!(names(&b.order(None, t.clone())), ["a", "b", "c"]);
        }
    }

    #[test]
    fn every_strategy_keeps_all_candidates_for_spillover() {
        let t = vec![target("a", 1), target("b", 2), target("c", 3)];
        for s in [
            Strategy::Priority,
            Strategy::RoundRobin,
            Strategy::LeastConn,
            Strategy::Weighted,
            Strategy::P2cLatency,
        ] {
            let b = Balancer::new(s);
            let mut got = names(&b.order(None, t.clone()));
            got.sort();
            assert_eq!(got, ["a", "b", "c"], "strategy {s} dropped a candidate");
        }
    }

    #[test]
    fn round_robin_rotates_the_head() {
        let b = Balancer::new(Strategy::RoundRobin);
        let t = vec![target("a", 1), target("b", 1), target("c", 1)];
        let heads: Vec<String> = (0..6)
            .map(|_| b.order(None, t.clone())[0].upstream.name.clone())
            .collect();
        assert_eq!(heads, ["a", "b", "c", "a", "b", "c"]);
    }

    #[test]
    fn least_conn_puts_the_idle_provider_first() {
        let b = Balancer::new(Strategy::LeastConn);
        let t = vec![target("busy", 1), target("idle", 1)];
        for _ in 0..3 {
            t[0].upstream.incr_inflight();
        }
        for _ in 0..10 {
            assert_eq!(b.order(None, t.clone())[0].upstream.name, "idle");
        }
    }

    #[test]
    fn weighted_follows_the_weights() {
        let b = Balancer::new(Strategy::Weighted);
        let t = vec![target("big", 9), target("small", 1)];
        let c = first_counts(&b, &t, 4000);
        let big = *c.get("big").unwrap_or(&0);
        assert!((3200..3900).contains(&big), "big led {big}/4000 times");
    }

    #[test]
    fn p2c_prefers_the_faster_provider() {
        let b = Balancer::new(Strategy::P2cLatency);
        let t = vec![target("slow", 1), target("fast", 1)];
        t[0].upstream.record_latency(Duration::from_millis(900));
        t[1].upstream.record_latency(Duration::from_millis(30));
        let c = first_counts(&b, &t, 200);
        assert_eq!(*c.get("fast").unwrap_or(&0), 200);
        // And the slow one is still there as the spillover path.
        assert_eq!(names(&b.order(None, t.clone())), ["fast", "slow"]);
    }

    #[test]
    fn p2c_accounts_for_queue_depth() {
        let b = Balancer::new(Strategy::P2cLatency);
        let t = vec![target("queued", 1), target("free", 1)];
        t[0].upstream.record_latency(Duration::from_millis(100));
        t[1].upstream.record_latency(Duration::from_millis(100));
        for _ in 0..3 {
            t[0].upstream.incr_inflight();
        }
        let c = first_counts(&b, &t, 200);
        assert_eq!(*c.get("free").unwrap_or(&0), 200);
    }

    #[test]
    fn a_pool_can_override_the_global_strategy() {
        let b = Balancer::new(Strategy::Priority);
        let t = vec![target("a", 1), target("b", 1), target("c", 1)];
        // Global default would not move anything.
        assert_eq!(names(&b.order(None, t.clone())), ["a", "b", "c"]);
        // The override does.
        let heads: Vec<String> = (0..3)
            .map(|_| {
                b.order(Some(Strategy::RoundRobin), t.clone())[0]
                    .upstream
                    .name
                    .clone()
            })
            .collect();
        assert_eq!(heads, ["a", "b", "c"]);
    }

    #[test]
    fn targets_carry_the_provider_side_model_name() {
        let t = target("openai", 1);
        assert_eq!(t.model, "openai-model");
        assert_eq!(t.label(), "openai:openai-model");
        assert_eq!(t.protocol(), Protocol::Openai);
    }
}
