//! SDK client construction.
//!
//! One pair of SDK clients is built per configured endpoint, and requests are
//! spread over the endpoints in round-robin order: every call to
//! [`Clients::vectors`] or [`Clients::s3`] returns the client of the next
//! endpoint. All endpoints share the credentials and the other settings.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use aws_credential_types::Credentials;
use aws_smithy_types::retry::RetryConfig;
use aws_smithy_types::timeout::TimeoutConfig;

use crate::config::Connection;

struct Endpoint {
    #[cfg(test)]
    url: String,
    vectors: aws_sdk_s3vectors::Client,
    s3: aws_sdk_s3::Client,
}

/// Cheap to clone; clones share the round-robin counter.
#[derive(Clone)]
pub struct Clients {
    endpoints: Arc<Vec<Endpoint>>,
    next: Arc<AtomicUsize>,
}

impl Clients {
    fn pick(&self) -> &Endpoint {
        let n = self.endpoints.len();
        if n == 1 {
            return &self.endpoints[0];
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed);
        &self.endpoints[i % n]
    }

    /// The S3 Vectors client of the next endpoint in round-robin order.
    pub fn vectors(&self) -> &aws_sdk_s3vectors::Client {
        &self.pick().vectors
    }

    /// The S3 client of the next endpoint in round-robin order.
    pub fn s3(&self) -> &aws_sdk_s3::Client {
        &self.pick().s3
    }
}

pub fn build(conn: &Connection) -> Clients {
    assert!(
        !conn.endpoint.is_empty(),
        "connection.endpoint is validated to be non-empty"
    );
    let creds = Credentials::new(&conn.access_key, &conn.secret_key, None, None, "s3vtest");
    let retry = if conn.retries == 0 {
        RetryConfig::disabled()
    } else {
        RetryConfig::standard().with_max_attempts(conn.retries + 1)
    };
    let mut timeout = TimeoutConfig::builder();
    if let Some(secs) = conn.timeout_secs {
        timeout = timeout.operation_attempt_timeout(Duration::from_secs(secs));
    }
    let timeout = timeout.build();

    let endpoints = conn
        .endpoint
        .iter()
        .map(|url| {
            let vectors = aws_sdk_s3vectors::Config::builder()
                .behavior_version(aws_sdk_s3vectors::config::BehaviorVersion::latest())
                .endpoint_url(url)
                .region(aws_sdk_s3vectors::config::Region::new(conn.region.clone()))
                .credentials_provider(creds.clone())
                .retry_config(retry.clone())
                .timeout_config(timeout.clone())
                .build();

            let s3 = aws_sdk_s3::Config::builder()
                .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
                .endpoint_url(url)
                .region(aws_sdk_s3::config::Region::new(conn.region.clone()))
                .credentials_provider(creds.clone())
                .force_path_style(conn.force_path_style)
                .retry_config(retry.clone())
                .timeout_config(timeout.clone())
                .build();

            Endpoint {
                #[cfg(test)]
                url: url.to_string(),
                vectors: aws_sdk_s3vectors::Client::from_conf(vectors),
                s3: aws_sdk_s3::Client::from_conf(s3),
            }
        })
        .collect();

    Clients {
        endpoints: Arc::new(endpoints),
        next: Arc::new(AtomicUsize::new(0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Endpoints;

    #[test]
    fn round_robin() {
        let conn = Connection {
            endpoint: Endpoints::parse("http://a:1,http://b:2,http://c:3"),
            access_key: "a".into(),
            secret_key: "b".into(),
            ..Default::default()
        };
        let clients = build(&conn);
        assert_eq!(clients.endpoints.len(), 3);
        let seen: Vec<&str> = (0..7).map(|_| clients.pick().url.as_str()).collect();
        assert_eq!(
            seen,
            [
                "http://a:1",
                "http://b:2",
                "http://c:3",
                "http://a:1",
                "http://b:2",
                "http://c:3",
                "http://a:1"
            ]
        );
        // clones share the counter
        let other = clients.clone();
        assert_eq!(other.pick().url, "http://b:2");
        assert_eq!(clients.pick().url, "http://c:3");

        let one = build(&Connection {
            endpoint: Endpoints::parse("http://only:1"),
            ..conn
        });
        assert!((0..3).all(|_| one.pick().url == "http://only:1"));
    }
}
