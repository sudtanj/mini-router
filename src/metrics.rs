//! Counters and Prometheus exposition.
//!
//! Hand-rolled rather than using a metrics crate: a handful of atomics and a
//! string builder cost nothing, and the dependency would be one of the larger
//! things in the tree.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::upstream::{Health, UpstreamStatus};
use crate::util::fmt_metric;

#[derive(Debug)]
pub struct Metrics {
    started: Instant,
    pub requests_total: AtomicU64,
    pub responses_2xx: AtomicU64,
    pub responses_4xx: AtomicU64,
    pub responses_5xx: AtomicU64,
    pub retries_total: AtomicU64,
    pub queue_timeouts_total: AtomicU64,
    pub no_upstream_total: AtomicU64,
    pub unauthorized_total: AtomicU64,
    pub streaming_total: AtomicU64,
    pub translated_total: AtomicU64,
    pub spilled_out_total: AtomicU64,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            requests_total: AtomicU64::new(0),
            responses_2xx: AtomicU64::new(0),
            responses_4xx: AtomicU64::new(0),
            responses_5xx: AtomicU64::new(0),
            retries_total: AtomicU64::new(0),
            queue_timeouts_total: AtomicU64::new(0),
            no_upstream_total: AtomicU64::new(0),
            unauthorized_total: AtomicU64::new(0),
            streaming_total: AtomicU64::new(0),
            translated_total: AtomicU64::new(0),
            spilled_out_total: AtomicU64::new(0),
        }
    }

    pub fn uptime_secs(&self) -> f64 {
        self.started.elapsed().as_secs_f64()
    }

    pub fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_status(&self, status: u16) {
        match status {
            200..=299 => Self::incr(&self.responses_2xx),
            400..=499 => Self::incr(&self.responses_4xx),
            500..=599 => Self::incr(&self.responses_5xx),
            _ => {}
        }
    }

    fn get(&self, c: &AtomicU64) -> u64 {
        c.load(Ordering::Relaxed)
    }

    /// Render the Prometheus text exposition format (version 0.0.4).
    pub fn render(&self, upstreams: &[UpstreamStatus]) -> String {
        let mut out = String::with_capacity(1024 + upstreams.len() * 320);

        counter(
            &mut out,
            "mini_router_requests_total",
            "Client requests accepted for routing.",
            self.get(&self.requests_total),
        );
        counter(
            &mut out,
            "mini_router_streaming_requests_total",
            "Client requests that asked for a streamed response.",
            self.get(&self.streaming_total),
        );
        counter(
            &mut out,
            "mini_router_translated_total",
            "Responses translated between the OpenAI and Anthropic dialects.",
            self.get(&self.translated_total),
        );
        counter(
            &mut out,
            "mini_router_spilled_out_total",
            "Requests that exhausted every candidate provider.",
            self.get(&self.spilled_out_total),
        );
        counter(
            &mut out,
            "mini_router_retries_total",
            "Attempts that spilled over to another provider.",
            self.get(&self.retries_total),
        );
        counter(
            &mut out,
            "mini_router_queue_timeouts_total",
            "Requests dropped after waiting too long for a free upstream slot.",
            self.get(&self.queue_timeouts_total),
        );
        counter(
            &mut out,
            "mini_router_no_upstream_total",
            "Requests with no upstream able to serve the requested model.",
            self.get(&self.no_upstream_total),
        );
        counter(
            &mut out,
            "mini_router_unauthorized_total",
            "Requests rejected because of a missing or wrong API key.",
            self.get(&self.unauthorized_total),
        );

        out.push_str(
            "# HELP mini_router_responses_total Responses returned to clients by status class.\n",
        );
        out.push_str("# TYPE mini_router_responses_total counter\n");
        for (class, value) in [
            ("2xx", self.get(&self.responses_2xx)),
            ("4xx", self.get(&self.responses_4xx)),
            ("5xx", self.get(&self.responses_5xx)),
        ] {
            let _ = writeln!(
                out,
                "mini_router_responses_total{{class=\"{class}\"}} {value}"
            );
        }

        gauge(
            &mut out,
            "mini_router_uptime_seconds",
            "Seconds since the router started.",
            self.uptime_secs(),
        );
        gauge(
            &mut out,
            "mini_router_upstreams",
            "Configured upstreams.",
            upstreams.len() as f64,
        );

        labelled_gauge(
            &mut out,
            "mini_router_upstream_up",
            "1 when the upstream is in rotation, 0 when it is not.",
            upstreams,
            |u| if u.health == Health::Down { 0.0 } else { 1.0 },
        );
        out.push_str(
            "# HELP mini_router_upstream_info Static provider facts, carried as labels.\n",
        );
        out.push_str("# TYPE mini_router_upstream_info gauge\n");
        for u in upstreams {
            let _ = writeln!(
                out,
                "mini_router_upstream_info{{upstream=\"{}\",protocol=\"{}\"}} 1",
                escape_label(&u.name),
                u.protocol
            );
        }

        labelled_gauge(
            &mut out,
            "mini_router_upstream_inflight",
            "Requests currently being served by the upstream.",
            upstreams,
            |u| u.inflight as f64,
        );
        labelled_gauge(
            &mut out,
            "mini_router_upstream_capacity",
            "Configured max_concurrency of the upstream.",
            upstreams,
            |u| u.max_concurrency as f64,
        );
        labelled_gauge(
            &mut out,
            "mini_router_upstream_ttfb_ewma_ms",
            "Smoothed time-to-first-byte of the upstream, in milliseconds.",
            upstreams,
            |u| u.ttfb_ewma_ms,
        );
        labelled_counter(
            &mut out,
            "mini_router_upstream_requests_total",
            "Requests dispatched to the upstream.",
            upstreams,
            |u| u.total_requests as f64,
        );
        labelled_counter(
            &mut out,
            "mini_router_upstream_failures_total",
            "Failed attempts against the upstream.",
            upstreams,
            |u| u.total_failures as f64,
        );

        out
    }
}

fn counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {value}");
}

fn gauge(out: &mut String, name: &str, help: &str, value: f64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {}", fmt_metric(value));
}

fn labelled_gauge(
    out: &mut String,
    name: &str,
    help: &str,
    upstreams: &[UpstreamStatus],
    f: impl Fn(&UpstreamStatus) -> f64,
) {
    labelled(out, name, help, "gauge", upstreams, f);
}

fn labelled_counter(
    out: &mut String,
    name: &str,
    help: &str,
    upstreams: &[UpstreamStatus],
    f: impl Fn(&UpstreamStatus) -> f64,
) {
    labelled(out, name, help, "counter", upstreams, f);
}

fn labelled(
    out: &mut String,
    name: &str,
    help: &str,
    kind: &str,
    upstreams: &[UpstreamStatus],
    f: impl Fn(&UpstreamStatus) -> f64,
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {kind}");
    for u in upstreams {
        let _ = writeln!(
            out,
            "{name}{{upstream=\"{}\"}} {}",
            escape_label(&u.name),
            fmt_metric(f(u))
        );
    }
}

/// Escape a Prometheus label value: backslash, double quote and newline.
fn escape_label(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            _ => s.push(c),
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(name: &str) -> UpstreamStatus {
        UpstreamStatus {
            name: name.into(),
            url: "https://api.openai.com/v1".into(),
            protocol: crate::protocol::Protocol::Openai,
            health: Health::Up,
            inflight: 2,
            max_concurrency: 4,
            weight: 1,
            fallback_only: false,
            ttfb_ewma_ms: 12.5,
            total_requests: 7,
            total_failures: 1,
            models: vec!["gpt-4o-mini".into()],
            last_error: None,
        }
    }

    #[test]
    fn render_contains_every_family_once() {
        let m = Metrics::new();
        m.requests_total.store(3, Ordering::Relaxed);
        let text = m.render(&[status("opi-a")]);
        for family in [
            "mini_router_requests_total",
            "mini_router_responses_total",
            "mini_router_upstream_up",
            "mini_router_upstream_inflight",
            "mini_router_upstream_ttfb_ewma_ms",
        ] {
            assert_eq!(
                text.matches(&format!("# TYPE {family} ")).count(),
                1,
                "{family} should be declared exactly once"
            );
        }
        assert!(text.contains("mini_router_requests_total 3"));
        assert!(text.contains("mini_router_upstream_inflight{upstream=\"opi-a\"} 2"));
        assert!(text.contains("mini_router_upstream_ttfb_ewma_ms{upstream=\"opi-a\"} 12.500"));
        assert!(
            text.contains("mini_router_upstream_info{upstream=\"opi-a\",protocol=\"openai\"} 1")
        );
    }

    #[test]
    fn down_upstream_reports_zero() {
        let m = Metrics::new();
        let mut s = status("dead");
        s.health = Health::Down;
        let text = m.render(&[s]);
        assert!(text.contains("mini_router_upstream_up{upstream=\"dead\"} 0"));
    }

    #[test]
    fn status_classes_are_counted() {
        let m = Metrics::new();
        m.record_status(200);
        m.record_status(204);
        m.record_status(401);
        m.record_status(503);
        let text = m.render(&[]);
        assert!(text.contains("mini_router_responses_total{class=\"2xx\"} 2"));
        assert!(text.contains("mini_router_responses_total{class=\"4xx\"} 1"));
        assert!(text.contains("mini_router_responses_total{class=\"5xx\"} 1"));
    }

    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape_label("a\"b\\c"), "a\\\"b\\\\c");
        let m = Metrics::new();
        let text = m.render(&[status("we\"ird")]);
        assert!(text.contains("upstream=\"we\\\"ird\""));
    }
}
