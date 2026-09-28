//! Extensions on top of the AWS SDK: RGW-specific request fields that are not
//! in the SDK's service model, and a byte-counting interceptor for throughput.
//!
//! Both follow the pattern used by the s3-tests-rs suite (ceph PR 69559):
//! `customize().mutate_request(..)` for request edits before signing, and an
//! `Intercept` implementation attached with `customize().interceptor(..)`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use aws_smithy_runtime_api::box_error::BoxError;
use aws_smithy_runtime_api::client::interceptors::context::{
    AfterDeserializationInterceptorContextRef, BeforeTransmitInterceptorContextRef,
};
use aws_smithy_runtime_api::client::interceptors::Intercept;
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;
use aws_smithy_types::body::SdkBody;
use aws_smithy_types::config_bag::ConfigBag;

use crate::config::FilterableKey;

/// Counts request and response body bytes of every call it is attached to.
#[derive(Clone, Default)]
pub struct ByteCounter {
    pub up: Arc<AtomicU64>,
    pub down: Arc<AtomicU64>,
}

impl ByteCounter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn up(&self) -> u64 {
        self.up.load(Ordering::Relaxed)
    }
    pub fn down(&self) -> u64 {
        self.down.load(Ordering::Relaxed)
    }
}

impl std::fmt::Debug for ByteCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ByteCounter")
            .field("up", &self.up())
            .field("down", &self.down())
            .finish()
    }
}

impl Intercept for ByteCounter {
    fn name(&self) -> &'static str {
        "s3vtest::ByteCounter"
    }

    fn read_before_transmit(
        &self,
        context: &BeforeTransmitInterceptorContextRef<'_>,
        _rc: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let req = context.request();
        let n = req
            .body()
            .bytes()
            .map(|b| b.len() as u64)
            .or_else(|| content_length(req.headers().get("content-length")))
            .unwrap_or(0);
        self.up.fetch_add(n, Ordering::Relaxed);
        Ok(())
    }

    fn read_after_deserialization(
        &self,
        context: &AfterDeserializationInterceptorContextRef<'_>,
        _rc: &RuntimeComponents,
        _cfg: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let resp = context.response();
        let n = resp
            .body()
            .bytes()
            .map(|b| b.len() as u64)
            .or_else(|| content_length(resp.headers().get("content-length")))
            .unwrap_or(0);
        self.down.fetch_add(n, Ordering::Relaxed);
        Ok(())
    }
}

fn content_length(v: Option<&str>) -> Option<u64> {
    v.and_then(|s| s.trim().parse().ok())
}

/// Merge extra top-level fields into the JSON body of a request. Used from a
/// `mutate_request` closure, which runs before signing, so the payload hash
/// and content-length stay consistent.
pub fn merge_json_body(req: &mut HttpRequest, extra: &serde_json::Map<String, serde_json::Value>) {
    let Some(bytes) = req.body().bytes() else {
        tracing::warn!("request body is not in memory; cannot inject extension fields");
        return;
    };
    let mut body: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("request body is not JSON ({e}); cannot inject extension fields");
            return;
        }
    };
    let obj = body.as_object_mut().expect("request body is a JSON object");
    for (k, v) in extra {
        deep_merge(obj, k, v);
    }
    let new = serde_json::to_vec(&body).expect("serializing JSON body");
    req.headers_mut()
        .insert("content-length", new.len().to_string());
    *req.body_mut() = SdkBody::from(new);
}

fn deep_merge(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    k: &str,
    v: &serde_json::Value,
) {
    match (obj.get_mut(k), v) {
        (Some(serde_json::Value::Object(existing)), serde_json::Value::Object(incoming)) => {
            for (ik, iv) in incoming {
                deep_merge(existing, ik, iv);
            }
        }
        _ => {
            obj.insert(k.to_string(), v.clone());
        }
    }
}

/// Extra body for CreateIndex: `metadataConfiguration.filterableMetadataKeys`.
pub fn filterable_keys_extra(keys: &[FilterableKey]) -> serde_json::Map<String, serde_json::Value> {
    let list: Vec<serde_json::Value> = keys
        .iter()
        .map(|k| {
            serde_json::json!({
                "name": k.name,
                "type": k.kind.as_str(),
                "mustExist": k.must_exist,
            })
        })
        .collect();
    let mut m = serde_json::Map::new();
    m.insert(
        "metadataConfiguration".into(),
        serde_json::json!({ "filterableMetadataKeys": list }),
    );
    m
}

/// Extra body for QueryVectors: `postFiltering`.
pub fn post_filtering_extra() -> serde_json::Map<String, serde_json::Value> {
    let mut m = serde_json::Map::new();
    m.insert("postFiltering".into(), serde_json::Value::Bool(true));
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_into_existing_object() {
        let mut obj = serde_json::json!({"a": {"x": 1}, "b": 2});
        let extra = serde_json::json!({"a": {"y": 3}, "c": 4});
        let o = obj.as_object_mut().unwrap();
        for (k, v) in extra.as_object().unwrap() {
            deep_merge(o, k, v);
        }
        assert_eq!(
            obj,
            serde_json::json!({"a": {"x": 1, "y": 3}, "b": 2, "c": 4})
        );
    }
}
