use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Instant;

use prometheus::{
    Encoder, Gauge, GaugeVec, HistogramOpts, HistogramVec, IntCounter, IntCounterVec, Opts,
    Registry, TextEncoder,
};

static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::default);
static STARTED_AT: LazyLock<Instant> = LazyLock::new(Instant::now);

pub static REQUEST_COUNT: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let c = IntCounterVec::new(
        Opts::new("tangle_operator_requests_total", "Total inference requests"),
        &["model", "status"],
    )
    .expect("metric");
    REGISTRY.register(Box::new(c.clone())).expect("register");
    c
});

pub static REQUEST_DURATION: LazyLock<HistogramVec> = LazyLock::new(|| {
    let h = HistogramVec::new(
        HistogramOpts::new(
            "tangle_operator_request_duration_ms",
            "End-to-end request duration in ms",
        )
        .buckets(vec![
            10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0,
        ]),
        &["model"],
    )
    .expect("metric");
    REGISTRY.register(Box::new(h.clone())).expect("register");
    h
});

pub static ACTIVE_REQUESTS: LazyLock<Gauge> = LazyLock::new(|| {
    let g = Gauge::new(
        "tangle_operator_active_requests",
        "Currently processing requests",
    )
    .expect("metric");
    REGISTRY.register(Box::new(g.clone())).expect("register");
    g
});

pub static MAX_CONCURRENT_REQUESTS: LazyLock<Gauge> = LazyLock::new(|| {
    let g = Gauge::new(
        "tangle_operator_max_concurrent",
        "Peak concurrent requests observed",
    )
    .expect("metric");
    REGISTRY.register(Box::new(g.clone())).expect("register");
    g
});

pub static TOKENS_TOTAL: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let c = IntCounterVec::new(
        Opts::new("tangle_operator_tokens_total", "LLM tokens processed"),
        &["model", "type"],
    )
    .expect("metric");
    REGISTRY.register(Box::new(c.clone())).expect("register");
    c
});

pub static TOKENS_PER_SECOND: LazyLock<GaugeVec> = LazyLock::new(|| {
    let g = GaugeVec::new(
        Opts::new(
            "tangle_operator_tokens_per_second",
            "Output token generation rate (last request)",
        ),
        &["model"],
    )
    .expect("metric");
    REGISTRY.register(Box::new(g.clone())).expect("register");
    g
});

pub static ERROR_COUNT: LazyLock<IntCounterVec> = LazyLock::new(|| {
    let c = IntCounterVec::new(
        Opts::new("tangle_operator_errors_total", "Errors by type"),
        &["model", "error_type"],
    )
    .expect("metric");
    REGISTRY.register(Box::new(c.clone())).expect("register");
    c
});

pub static HEARTBEATS_SENT: LazyLock<IntCounter> = LazyLock::new(|| {
    let c = IntCounter::new(
        "tangle_operator_heartbeats_total",
        "Heartbeats sent to chain",
    )
    .expect("metric");
    REGISTRY.register(Box::new(c.clone())).expect("register");
    c
});

pub static HEARTBEATS_FAILED: LazyLock<IntCounter> = LazyLock::new(|| {
    let c = IntCounter::new(
        "tangle_operator_heartbeats_failed_total",
        "Failed heartbeat submissions",
    )
    .expect("metric");
    REGISTRY.register(Box::new(c.clone())).expect("register");
    c
});

static TOTAL_SUCCESS: AtomicU64 = AtomicU64::new(0);
static TOTAL_ERROR: AtomicU64 = AtomicU64::new(0);
static LATENCY_SUM_MS: AtomicU64 = AtomicU64::new(0);
static LATENCY_COUNT: AtomicU64 = AtomicU64::new(0);
static LATENCY_MAX_MS: AtomicU64 = AtomicU64::new(0);
static TOKENS_GENERATED: AtomicU64 = AtomicU64::new(0);
static TOKENS_PROMPTED: AtomicU64 = AtomicU64::new(0);
static PEAK_CONCURRENT: AtomicU64 = AtomicU64::new(0);

/// RAII guard that tracks request lifecycle metrics. Create at request start,
/// set tokens/success during processing, metrics recorded on drop.
pub struct RequestGuard {
    model: String,
    start: Instant,
    prompt_tokens: u32,
    completion_tokens: u32,
    success: bool,
}

impl Default for RequestGuard {
    fn default() -> Self {
        Self::new("unknown")
    }
}

impl RequestGuard {
    pub fn new(model: &str) -> Self {
        let _ = *STARTED_AT;

        ACTIVE_REQUESTS.inc();
        let current = ACTIVE_REQUESTS.get() as u64;
        let prev = PEAK_CONCURRENT.fetch_max(current, Ordering::Relaxed);
        if current > prev {
            MAX_CONCURRENT_REQUESTS.set(current as f64);
        }

        Self {
            model: model.to_string(),
            start: Instant::now(),
            prompt_tokens: 0,
            completion_tokens: 0,
            success: false,
        }
    }

    pub fn set_tokens(&mut self, prompt: u32, completion: u32) {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
    }

    /// Record time-to-first-token in milliseconds.

    pub fn set_success(&mut self) {
        self.success = true;
    }

    /// Record a typed error for fine-grained error tracking.
    pub fn record_error(&self, error_type: &str) {
        ERROR_COUNT
            .with_label_values(&[&self.model, error_type])
            .inc();
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        ACTIVE_REQUESTS.dec();

        let elapsed_ms = self.start.elapsed().as_millis() as u64;

        let status = if self.success { "success" } else { "error" };
        REQUEST_COUNT
            .with_label_values(&[&self.model, status])
            .inc();
        REQUEST_DURATION
            .with_label_values(&[&self.model])
            .observe(elapsed_ms as f64);

        if self.success {
            TOTAL_SUCCESS.fetch_add(1, Ordering::Relaxed);
        } else {
            TOTAL_ERROR.fetch_add(1, Ordering::Relaxed);
        }

        LATENCY_SUM_MS.fetch_add(elapsed_ms, Ordering::Relaxed);
        LATENCY_COUNT.fetch_add(1, Ordering::Relaxed);

        LATENCY_MAX_MS.fetch_max(elapsed_ms, Ordering::Relaxed);

        if self.prompt_tokens > 0 {
            TOKENS_TOTAL
                .with_label_values(&[&self.model, "prompt"])
                .inc_by(self.prompt_tokens as u64);
            TOKENS_PROMPTED.fetch_add(self.prompt_tokens as u64, Ordering::Relaxed);
        }
        if self.completion_tokens > 0 {
            TOKENS_TOTAL
                .with_label_values(&[&self.model, "completion"])
                .inc_by(self.completion_tokens as u64);
            TOKENS_GENERATED.fetch_add(self.completion_tokens as u64, Ordering::Relaxed);

            let elapsed_secs = self.start.elapsed().as_secs_f64();
            if elapsed_secs > 0.0 {
                let tps = self.completion_tokens as f64 / elapsed_secs;
                TOKENS_PER_SECOND.with_label_values(&[&self.model]).set(tps);
            }
        }
    }
}

/// Gather all registered metrics as Prometheus text exposition format.
pub fn gather() -> String {
    let encoder = TextEncoder::new();
    let families = REGISTRY.gather();
    let mut buf = Vec::new();
    encoder.encode(&families, &mut buf).unwrap_or_default();
    String::from_utf8(buf).unwrap_or_default()
}

/// Snapshot of key metrics for on-chain QoS submission.
/// Values are designed for uint256 encoding (basis points for rates).
pub fn on_chain_metrics() -> Vec<(String, u64)> {
    let success = TOTAL_SUCCESS.load(Ordering::Relaxed);
    let error = TOTAL_ERROR.load(Ordering::Relaxed);
    let total = success + error;
    let latency_count = LATENCY_COUNT.load(Ordering::Relaxed);
    let latency_avg = if latency_count > 0 {
        LATENCY_SUM_MS
            .load(Ordering::Relaxed)
            .checked_div(latency_count)
            .unwrap_or(0)
    } else {
        0
    };
    let latency_max = LATENCY_MAX_MS.load(Ordering::Relaxed);
    let uptime_bps = if total > 0 {
        (success * 10000).checked_div(total).unwrap_or(0)
    } else {
        10000
    };
    let error_rate_bps = if total > 0 {
        (error * 10000).checked_div(total).unwrap_or(0)
    } else {
        0
    };
    let uptime_secs = STARTED_AT.elapsed().as_secs();

    vec![
        ("requests_total".into(), total),
        ("requests_success".into(), success),
        ("requests_error".into(), error),
        ("latency_avg_ms".into(), latency_avg),
        ("latency_max_ms".into(), latency_max),
        ("uptime_bps".into(), uptime_bps),
        ("error_rate_bps".into(), error_rate_bps),
        (
            "tokens_generated".into(),
            TOKENS_GENERATED.load(Ordering::Relaxed),
        ),
        (
            "tokens_prompted".into(),
            TOKENS_PROMPTED.load(Ordering::Relaxed),
        ),
        (
            "peak_concurrent".into(),
            PEAK_CONCURRENT.load(Ordering::Relaxed),
        ),
        ("uptime_seconds".into(), uptime_secs),
    ]
}

/// JSON summary of operator health for dashboards and monitoring.
pub fn health_summary() -> serde_json::Value {
    let success = TOTAL_SUCCESS.load(Ordering::Relaxed);
    let error = TOTAL_ERROR.load(Ordering::Relaxed);
    let total = success + error;
    let latency_count = LATENCY_COUNT.load(Ordering::Relaxed);

    serde_json::json!({
        "uptime_seconds": STARTED_AT.elapsed().as_secs(),
        "requests": {
            "total": total,
            "success": success,
            "error": error,
        },
        "latency": {
            "avg_ms": if latency_count > 0 {
                LATENCY_SUM_MS
                    .load(Ordering::Relaxed)
                    .checked_div(latency_count)
                    .unwrap_or(0)
            } else {
                0
            },
            "max_ms": LATENCY_MAX_MS.load(Ordering::Relaxed),

        },
        "throughput": {
            "tokens_generated": TOKENS_GENERATED.load(Ordering::Relaxed),
            "tokens_prompted": TOKENS_PROMPTED.load(Ordering::Relaxed),
        },
        "infrastructure": {
            "peak_concurrent": PEAK_CONCURRENT.load(Ordering::Relaxed),
            "active_requests": ACTIVE_REQUESTS.get() as u64,
        },
    })
}
