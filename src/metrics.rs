//! Per-step measurements: latency histogram, ops, errors by code, bytes.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use aws_sdk_s3vectors::error::{ProvideErrorMetadata, SdkError};
use hdrhistogram::Histogram;
use serde::Serialize;

use crate::ext::ByteCounter;

pub struct StepMetrics {
    pub name: String,
    pub vector_op: bool,
    hist: Mutex<Histogram<u64>>, // microseconds
    ops: AtomicU64,
    errors: AtomicU64,
    error_codes: Mutex<BTreeMap<String, u64>>,
    wall: Mutex<Duration>,
    pub bytes: ByteCounter,
    logged_errors: AtomicU64,
    max_logged_errors: u64,
}

impl StepMetrics {
    pub fn new(name: &str, vector_op: bool, max_logged_errors: u64) -> Self {
        Self {
            name: name.to_string(),
            vector_op,
            hist: Mutex::new(Histogram::new_with_bounds(1, 3_600_000_000, 3).expect("histogram")),
            ops: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            error_codes: Mutex::new(BTreeMap::new()),
            wall: Mutex::new(Duration::ZERO),
            bytes: ByteCounter::new(),
            logged_errors: AtomicU64::new(0),
            max_logged_errors,
        }
    }

    pub fn record_ok(&self, latency: Duration) {
        self.ops.fetch_add(1, Ordering::Relaxed);
        let us = latency.as_micros().max(1) as u64;
        let _ = self.hist.lock().unwrap().record(us.min(3_600_000_000));
    }

    pub fn record_err(&self, code: String, detail: String) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        *self
            .error_codes
            .lock()
            .unwrap()
            .entry(code.clone())
            .or_insert(0) += 1;
        if self.logged_errors.fetch_add(1, Ordering::Relaxed) < self.max_logged_errors {
            tracing::warn!(step = %self.name, code = %code, "{detail}");
        }
    }

    pub fn add_wall(&self, d: Duration) {
        *self.wall.lock().unwrap() += d;
    }

    pub fn summary(&self) -> StepSummary {
        let h = self.hist.lock().unwrap();
        let wall = *self.wall.lock().unwrap();
        let secs = wall.as_secs_f64();
        let ops = self.ops.load(Ordering::Relaxed);
        let rate = |n: u64| if secs > 0.0 { n as f64 / secs } else { 0.0 };
        StepSummary {
            step: self.name.clone(),
            ops,
            errors: self.errors.load(Ordering::Relaxed),
            error_codes: self.error_codes.lock().unwrap().clone(),
            wall_secs: secs,
            ops_per_sec: rate(ops),
            p50_ms: us_to_ms(h.value_at_quantile(0.5)),
            p90_ms: us_to_ms(h.value_at_quantile(0.9)),
            p99_ms: us_to_ms(h.value_at_quantile(0.99)),
            max_ms: us_to_ms(h.max()),
            bytes_up: self.vector_op.then(|| self.bytes.up()),
            bytes_down: self.vector_op.then(|| self.bytes.down()),
            upload_bytes_per_sec: self.vector_op.then(|| rate(self.bytes.up())),
            download_bytes_per_sec: self.vector_op.then(|| rate(self.bytes.down())),
        }
    }
}

fn us_to_ms(us: u64) -> f64 {
    us as f64 / 1000.0
}

#[derive(Debug, Clone, Serialize)]
pub struct StepSummary {
    pub step: String,
    pub ops: u64,
    pub errors: u64,
    pub error_codes: BTreeMap<String, u64>,
    pub wall_secs: f64,
    pub ops_per_sec: f64,
    pub p50_ms: f64,
    pub p90_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_up: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_down: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_bytes_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub download_bytes_per_sec: Option<f64>,
}

/// Short classification of an SDK error: the service error code when there
/// is one, otherwise the transport-level failure kind.
pub fn error_code<E, R>(err: &SdkError<E, R>) -> String
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug + 'static,
{
    match err {
        SdkError::ServiceError(e) => err.code().map(str::to_string).unwrap_or_else(|| {
            let any: &dyn std::any::Any = e.raw();
            match any.downcast_ref::<aws_smithy_runtime_api::client::orchestrator::HttpResponse>() {
                Some(resp) => format!("HTTP{}", resp.status().as_u16()),
                None => "ServiceError".to_string(),
            }
        }),
        SdkError::TimeoutError(_) => "Timeout".to_string(),
        SdkError::DispatchFailure(_) => "DispatchFailure".to_string(),
        SdkError::ResponseError(_) => "ResponseError".to_string(),
        SdkError::ConstructionFailure(_) => "ConstructionFailure".to_string(),
        _ => "Unknown".to_string(),
    }
}

pub fn error_detail<E, R>(err: &SdkError<E, R>) -> String
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug + 'static,
{
    match err {
        SdkError::ServiceError(e) => {
            let msg = e.err().message().unwrap_or("");
            format!("{msg} ({:?}) raw: {}", e.err(), raw_response(e.raw()))
        }
        SdkError::ResponseError(e) => format!("{e:?} raw: {}", raw_response(e.raw())),
        other => error_chain(other),
    }
}

/// The error message followed by every underlying cause, so that a transport
/// failure reads "dispatch failure: ... : Connection refused" rather than
/// just "dispatch failure".
fn error_chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut cur = err.source();
    while let Some(e) = cur {
        let msg = e.to_string();
        if !msg.is_empty() && !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        cur = e.source();
    }
    out
}

/// HTTP status and a short body excerpt of a raw response, when it is an
/// SDK `HttpResponse`. Bodies of RGW error responses are XML, which the SDK
/// cannot deserialize, so this is the only place the real error shows up.
fn raw_response<R: std::fmt::Debug + 'static>(raw: &R) -> String {
    let any: &dyn std::any::Any = raw;
    if let Some(resp) =
        any.downcast_ref::<aws_smithy_runtime_api::client::orchestrator::HttpResponse>()
    {
        let body = resp
            .body()
            .bytes()
            .map(|b| {
                String::from_utf8_lossy(b)
                    .chars()
                    .take(300)
                    .collect::<String>()
            })
            .unwrap_or_default();
        format!("status {} body {body:?}", resp.status().as_u16())
    } else {
        format!("{raw:?}")
    }
}
