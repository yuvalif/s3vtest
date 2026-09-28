//! SDK client construction.

use std::time::Duration;

use aws_credential_types::Credentials;
use aws_smithy_types::retry::RetryConfig;
use aws_smithy_types::timeout::TimeoutConfig;

use crate::config::Connection;

#[derive(Clone)]
pub struct Clients {
    pub vectors: aws_sdk_s3vectors::Client,
    pub s3: aws_sdk_s3::Client,
}

pub fn build(conn: &Connection) -> Clients {
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

    let vectors = aws_sdk_s3vectors::Config::builder()
        .behavior_version(aws_sdk_s3vectors::config::BehaviorVersion::latest())
        .endpoint_url(&conn.endpoint)
        .region(aws_sdk_s3vectors::config::Region::new(conn.region.clone()))
        .credentials_provider(creds.clone())
        .retry_config(retry.clone())
        .timeout_config(timeout.clone())
        .build();

    let s3 = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .endpoint_url(&conn.endpoint)
        .region(aws_sdk_s3::config::Region::new(conn.region.clone()))
        .credentials_provider(creds)
        .force_path_style(conn.force_path_style)
        .retry_config(retry)
        .timeout_config(timeout)
        .build();

    Clients {
        vectors: aws_sdk_s3vectors::Client::from_conf(vectors),
        s3: aws_sdk_s3::Client::from_conf(s3),
    }
}
