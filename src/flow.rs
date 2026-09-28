//! Flow execution: expands each step into independent work items and runs
//! them with bounded concurrency, recording measurements per step.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use aws_sdk_s3vectors::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3vectors::types::{
    DataType, DistanceMetric, MetadataConfiguration, PutInputVector, VectorData,
};
use futures::stream::{self, StreamExt};
use rand::seq::index::sample as sample_indices;
use rand::{Rng, SeedableRng};

use crate::client::{self, Clients};
use crate::config::{Config, Metric, QuerySource, Step};
use crate::ext::{filterable_keys_extra, merge_json_body, post_filtering_extra};
use crate::metadata::{json_to_document, MetadataGen};
use crate::metrics::{error_code, error_detail, StepMetrics};
use crate::report::JobReport;
use crate::sample::{derive_rng, SeededRng};
use crate::vectors::{self, VectorSource};

/// The fixed set of entities a job works on, decided once from the seed.
struct Plan {
    buckets: Vec<String>,
    /// Every index of every bucket, flattened.
    indexes: Vec<IndexPlan>,
}

struct IndexPlan {
    bucket: String,
    name: String,
    num_vectors: u64,
}

/// Mutable per-index state, updated between steps only.
#[derive(Default, Clone)]
struct IndexState {
    keys: Vec<String>,
    next_key: u64,
}

struct Ctx {
    cfg: Config,
    /// Index dimension: configured, or taken from the dataset.
    dimension: usize,
    clients: Clients,
    source: Box<dyn VectorSource>,
    meta: MetadataGen,
    seed: u64,
    plan: Plan,
}

pub async fn run_job(name: String, cfg: Config) -> Result<JobReport> {
    let seed = cfg.execution.seed.unwrap_or_else(rand::random);
    let mut rng = SeededRng::seed_from_u64(seed);
    let threads = cfg.execution.threads;
    let repeat = cfg.execution.repeat;

    let source = vectors::build(
        &cfg.entities.vectors.source,
        cfg.entities.index.dimension.map(|d| d as usize),
        &cfg.execution.datasets_dir,
        &mut rng,
    )
    .await
    .context("building vector source")?;
    let dimension = source.dimension();
    if dimension == 0 || dimension > crate::config::limits::MAX_DIMENSION as usize {
        anyhow::bail!(
            "vector dimension {dimension} is outside the API limit of 1 to {}",
            crate::config::limits::MAX_DIMENSION
        );
    }

    let plan = make_plan(&cfg, &mut rng);
    tracing::info!(
        job = %name,
        seed,
        threads,
        repeat,
        buckets = plan.buckets.len(),
        indexes = plan.indexes.len(),
        vectors = plan.indexes.iter().map(|i| i.num_vectors).sum::<u64>(),
        "starting: {}",
        source.describe()
    );

    let ctx = Arc::new(Ctx {
        clients: client::build(&cfg.connection),
        meta: MetadataGen::new(&cfg.entities.vectors.metadata),
        source,
        seed,
        plan,
        dimension,
        cfg,
    });

    let metrics: Vec<Arc<StepMetrics>> = ctx
        .cfg
        .flow
        .iter()
        .map(|s| {
            Arc::new(StepMetrics::new(
                s.name(),
                s.is_vector_op(),
                ctx.cfg.execution.max_logged_errors,
            ))
        })
        .collect();

    let mut state: Vec<IndexState> = vec![IndexState::default(); ctx.plan.indexes.len()];
    let mut aborted: Option<String> = None;

    'run: for rep in 0..repeat {
        for (si, step) in ctx.cfg.flow.iter().enumerate() {
            let m = metrics[si].clone();
            let t = Instant::now();
            let r = run_step(&ctx, step, rep, si, &m, &mut state).await;
            m.add_wall(t.elapsed());
            match r {
                Ok(()) => {}
                Err(StepError::Conflict(what)) => {
                    let msg = format!(
                        "step {} (repeat {}): {}; aborting the job so that no later step touches them. \
                         Use a different prefix, or `s3vtest cleanup` if they are leftovers of this tool.",
                        step.name(),
                        rep + 1,
                        what
                    );
                    tracing::error!(job = %name, "{msg}");
                    aborted = Some(msg);
                    break 'run;
                }
                Err(StepError::Other(e)) => return Err(e),
            }
            let s = m.summary();
            tracing::info!(
                job = %name,
                repeat = rep + 1,
                step = step.name(),
                ops = s.ops,
                errors = s.errors,
                "done in {:.2}s",
                t.elapsed().as_secs_f64()
            );
        }
    }

    Ok(JobReport {
        job: name,
        seed,
        threads,
        repeat,
        aborted,
        steps: metrics.iter().map(|m| m.summary()).collect(),
    })
}

fn make_plan(cfg: &Config, rng: &mut SeededRng) -> Plan {
    let e = &cfg.entities;
    let nb = e.vector_buckets.count.sample(rng);
    let mut buckets = Vec::new();
    let mut indexes = Vec::new();
    for b in 0..nb {
        let bucket = format!("{}{}", e.vector_buckets.prefix, b);
        let ni = e.indexes_per_bucket.count.sample(rng);
        for i in 0..ni {
            indexes.push(IndexPlan {
                bucket: bucket.clone(),
                name: format!("{}{}", e.indexes_per_bucket.prefix, i),
                num_vectors: e.vectors_per_index.sample(rng),
            });
        }
        buckets.push(bucket);
    }
    Plan { buckets, indexes }
}

/// Run a set of futures with at most `threads` in flight; results in completion order.
async fn run_concurrent<T, Fut>(threads: usize, futs: Vec<Fut>) -> Vec<T>
where
    Fut: std::future::Future<Output = T>,
{
    stream::iter(futs).buffer_unordered(threads).collect().await
}

/// Time one SDK call and record it. On failure the error code is returned.
async fn measured<T, E, R, Fut>(m: &StepMetrics, fut: Fut) -> Result<T, String>
where
    Fut: std::future::Future<Output = Result<T, SdkError<E, R>>>,
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug + 'static,
{
    let t = Instant::now();
    match fut.await {
        Ok(v) => {
            m.record_ok(t.elapsed());
            Ok(v)
        }
        Err(e) => {
            let code = error_code(&e);
            m.record_err(code.clone(), error_detail(&e));
            Err(code)
        }
    }
}

/// A create call that hit an entity which already exists. The entity is not
/// ours, so the job must stop before any later step touches it.
fn is_conflict(code: &str) -> bool {
    code == "ConflictException" || code == "HTTP409"
}

fn percent_of(keys: &[String], percent: f64, rng: &mut SeededRng) -> Vec<String> {
    let n = ((keys.len() as f64) * percent / 100.0).round() as usize;
    let n = n.min(keys.len());
    if n == 0 {
        return Vec::new();
    }
    sample_indices(rng, keys.len(), n)
        .into_iter()
        .map(|i| keys[i].clone())
        .collect()
}

/// Split keys into batches, each batch size sampled from the spec.
fn batches(
    keys: Vec<String>,
    spec: &crate::sample::NumSpec,
    rng: &mut SeededRng,
) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut it = keys.into_iter().peekable();
    while it.peek().is_some() {
        let n = spec.sample(rng).max(1) as usize;
        out.push(it.by_ref().take(n).collect());
    }
    out
}

fn put_vectors(
    ctx: &Ctx,
    keys: &[String],
    first_index: u64,
    rng: &mut SeededRng,
) -> Vec<PutInputVector> {
    keys.iter()
        .enumerate()
        .map(|(j, k)| {
            let v = ctx.source.vector(first_index + j as u64, rng);
            let md = ctx.meta.generate(rng);
            PutInputVector::builder()
                .key(k)
                .data(VectorData::Float32(v))
                .metadata(json_to_document(&serde_json::Value::Object(md)))
                .build()
                .expect("key is set")
        })
        .collect()
}

enum StepError {
    /// A create step found entities that already exist. The job stops so that
    /// later steps (in particular the delete steps) never touch data that
    /// another job or user owns.
    Conflict(String),
    Other(anyhow::Error),
}

impl From<anyhow::Error> for StepError {
    fn from(e: anyhow::Error) -> Self {
        StepError::Other(e)
    }
}

async fn run_step(
    ctx: &Arc<Ctx>,
    step: &Step,
    rep: u32,
    si: usize,
    m: &Arc<StepMetrics>,
    state: &mut [IndexState],
) -> Result<(), StepError> {
    let threads = ctx.cfg.execution.threads;
    let seed = ctx.seed;
    let disc = |extra: &[u64]| {
        let mut parts = vec![rep as u64, si as u64];
        parts.extend_from_slice(extra);
        derive_rng(seed, &parts)
    };

    match step {
        Step::CreateVectorBucket => {
            let futs: Vec<_> = ctx
                .plan
                .buckets
                .iter()
                .cloned()
                .map(|bucket| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    async move {
                        if ctx.cfg.entities.vector_buckets.create_backing_bucket {
                            // Not measured: a prerequisite of the RGW backend, not part of the API.
                            if let Err(e) =
                                ctx.clients.s3.create_bucket().bucket(&bucket).send().await
                            {
                                let code = error_code(&e);
                                if code != "BucketAlreadyOwnedByYou" {
                                    m.record_err(
                                        format!("S3CreateBucket:{code}"),
                                        error_detail(&e),
                                    );
                                }
                            }
                        }
                        let r = measured(
                            &m,
                            ctx.clients
                                .vectors
                                .create_vector_bucket()
                                .vector_bucket_name(&bucket)
                                .customize()
                                .interceptor(m.bytes.clone())
                                .send(),
                        )
                        .await;
                        r.err().filter(|c| is_conflict(c)).map(|_| bucket)
                    }
                })
                .collect();
            let conflicts: Vec<String> = run_concurrent(threads, futs)
                .await
                .into_iter()
                .flatten()
                .collect();
            if !conflicts.is_empty() {
                return Err(StepError::Conflict(format!(
                    "vector bucket(s) already exist: {}",
                    conflicts.join(", ")
                )));
            }
        }

        Step::CreateIndex => {
            let ic = &ctx.cfg.entities.index;
            let md_conf = if ic.non_filterable_keys.is_empty() {
                None
            } else {
                Some(
                    MetadataConfiguration::builder()
                        .set_non_filterable_metadata_keys(Some(ic.non_filterable_keys.clone()))
                        .build()
                        .context("building metadata configuration")?,
                )
            };
            let extra = (!ic.filterable_keys.is_empty())
                .then(|| Arc::new(filterable_keys_extra(&ic.filterable_keys)));
            let futs: Vec<_> = ctx
                .plan
                .indexes
                .iter()
                .map(|ip| (ip.bucket.clone(), ip.name.clone()))
                .map(|(bucket, index)| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    let md_conf = md_conf.clone();
                    let extra = extra.clone();
                    async move {
                        let ic = &ctx.cfg.entities.index;
                        let req = ctx
                            .clients
                            .vectors
                            .create_index()
                            .vector_bucket_name(&bucket)
                            .index_name(&index)
                            .data_type(DataType::Float32)
                            .dimension(ctx.dimension as i32)
                            .distance_metric(match ic.metric {
                                Metric::Cosine => DistanceMetric::Cosine,
                                Metric::Euclidean => DistanceMetric::Euclidean,
                            })
                            .set_metadata_configuration(md_conf);
                        let mut c = req.customize().interceptor(m.bytes.clone());
                        if let Some(extra) = extra {
                            c = c.mutate_request(move |r| merge_json_body(r, &extra));
                        }
                        let r = measured(&m, c.send()).await;
                        r.err()
                            .filter(|c| is_conflict(c))
                            .map(|_| format!("{bucket}/{index}"))
                    }
                })
                .collect();
            let conflicts: Vec<String> = run_concurrent(threads, futs)
                .await
                .into_iter()
                .flatten()
                .collect();
            if !conflicts.is_empty() {
                return Err(StepError::Conflict(format!(
                    "index(es) already exist: {}",
                    conflicts.join(", ")
                )));
            }
        }

        Step::InsertVectors => {
            // Plan batches first so key numbering is deterministic.
            let mut items = Vec::new(); // (index_idx, batch_idx, first_key_n, keys)
            for (ii, ip) in ctx.plan.indexes.iter().enumerate() {
                let mut rng = disc(&[ii as u64, u64::MAX]);
                let mut left = ip.num_vectors;
                let mut bi = 0u64;
                while left > 0 {
                    let n = ctx
                        .cfg
                        .entities
                        .insert_batch_size
                        .sample(&mut rng)
                        .clamp(1, left);
                    let first = state[ii].next_key;
                    let keys: Vec<String> = (first..first + n)
                        .map(|k| format!("{}{}", ctx.cfg.entities.key_prefix, k))
                        .collect();
                    state[ii].next_key += n;
                    items.push((ii, bi, first, keys));
                    left -= n;
                    bi += 1;
                }
            }
            let futs: Vec<_> = items
                .into_iter()
                .map(|(ii, bi, first, keys)| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    let mut rng = disc(&[ii as u64, bi]);
                    async move {
                        let ip = &ctx.plan.indexes[ii];
                        let vectors = put_vectors(&ctx, &keys, first, &mut rng);
                        let ok = measured(
                            &m,
                            ctx.clients
                                .vectors
                                .put_vectors()
                                .vector_bucket_name(&ip.bucket)
                                .index_name(&ip.name)
                                .set_vectors(Some(vectors))
                                .customize()
                                .interceptor(m.bytes.clone())
                                .send(),
                        )
                        .await
                        .is_ok();
                        (ii, keys, ok)
                    }
                })
                .collect();
            for (ii, keys, ok) in run_concurrent(threads, futs).await {
                if ok {
                    state[ii].keys.extend(keys);
                }
            }
        }

        Step::GetVectors(o) => {
            let mut items = Vec::new();
            for (ii, st) in state.iter().enumerate() {
                let mut rng = disc(&[ii as u64]);
                let chosen = percent_of(&st.keys, o.percent, &mut rng);
                for b in batches(chosen, &o.batch_size, &mut rng) {
                    items.push((ii, b));
                }
            }
            let futs: Vec<_> = items
                .into_iter()
                .map(|(ii, keys)| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    let o = o.clone();
                    async move {
                        let ip = &ctx.plan.indexes[ii];
                        let _ = measured(
                            &m,
                            ctx.clients
                                .vectors
                                .get_vectors()
                                .vector_bucket_name(&ip.bucket)
                                .index_name(&ip.name)
                                .set_keys(Some(keys))
                                .return_data(o.return_data)
                                .return_metadata(o.return_metadata)
                                .customize()
                                .interceptor(m.bytes.clone())
                                .send(),
                        )
                        .await;
                    }
                })
                .collect();
            run_concurrent(threads, futs).await;
        }

        Step::ListVectors(o) => {
            let futs: Vec<_> = (0..ctx.plan.indexes.len())
                .map(|ii| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    let o = o.clone();
                    let mut rng = disc(&[ii as u64]);
                    async move {
                        let ip = &ctx.plan.indexes[ii];
                        let mut token: Option<String> = None;
                        loop {
                            let page = o.batch_size.sample(&mut rng) as i32;
                            let out = measured(
                                &m,
                                ctx.clients
                                    .vectors
                                    .list_vectors()
                                    .vector_bucket_name(&ip.bucket)
                                    .index_name(&ip.name)
                                    .max_results(page)
                                    .set_next_token(token.take())
                                    .return_data(o.return_data)
                                    .return_metadata(o.return_metadata)
                                    .customize()
                                    .interceptor(m.bytes.clone())
                                    .send(),
                            )
                            .await;
                            match out.ok().and_then(|r| r.next_token().map(str::to_string)) {
                                Some(t) => token = Some(t),
                                None => break,
                            }
                        }
                    }
                })
                .collect();
            run_concurrent(threads, futs).await;
        }

        Step::DeleteVectors(o) => {
            let mut items = Vec::new();
            for (ii, st) in state.iter().enumerate() {
                let mut rng = disc(&[ii as u64]);
                let chosen = percent_of(&st.keys, o.percent, &mut rng);
                for b in batches(chosen, &o.batch_size, &mut rng) {
                    items.push((ii, b));
                }
            }
            let futs: Vec<_> = items
                .into_iter()
                .map(|(ii, keys)| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    async move {
                        let ip = &ctx.plan.indexes[ii];
                        let ok = measured(
                            &m,
                            ctx.clients
                                .vectors
                                .delete_vectors()
                                .vector_bucket_name(&ip.bucket)
                                .index_name(&ip.name)
                                .set_keys(Some(keys.clone()))
                                .customize()
                                .interceptor(m.bytes.clone())
                                .send(),
                        )
                        .await
                        .is_ok();
                        (ii, keys, ok)
                    }
                })
                .collect();
            let mut deleted: Vec<HashSet<String>> = vec![HashSet::new(); state.len()];
            for (ii, keys, ok) in run_concurrent(threads, futs).await {
                if ok {
                    deleted[ii].extend(keys);
                }
            }
            for (ii, st) in state.iter_mut().enumerate() {
                if !deleted[ii].is_empty() {
                    st.keys.retain(|k| !deleted[ii].contains(k));
                }
            }
        }

        Step::UpdateVectors(o) => {
            let spec = o.batch_size.unwrap_or(ctx.cfg.entities.insert_batch_size);
            let mut items = Vec::new();
            for (ii, st) in state.iter().enumerate() {
                let mut rng = disc(&[ii as u64]);
                let chosen = percent_of(&st.keys, o.percent, &mut rng);
                for (bi, b) in batches(chosen, &spec, &mut rng).into_iter().enumerate() {
                    items.push((ii, bi as u64, b));
                }
            }
            let futs: Vec<_> = items
                .into_iter()
                .map(|(ii, bi, keys)| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    let mut rng = disc(&[ii as u64, bi]);
                    async move {
                        let ip = &ctx.plan.indexes[ii];
                        let first: u64 = rng.random();
                        let vectors = put_vectors(&ctx, &keys, first, &mut rng);
                        let _ = measured(
                            &m,
                            ctx.clients
                                .vectors
                                .put_vectors()
                                .vector_bucket_name(&ip.bucket)
                                .index_name(&ip.name)
                                .set_vectors(Some(vectors))
                                .customize()
                                .interceptor(m.bytes.clone())
                                .send(),
                        )
                        .await;
                    }
                })
                .collect();
            run_concurrent(threads, futs).await;
        }

        Step::QueryVectors(o) => {
            let filter = o.filter.as_ref().map(json_to_document);
            let has_test = ctx.source.test_len().is_some();
            let qsource = match o.source {
                QuerySource::Auto if has_test => QuerySource::Test,
                QuerySource::Auto => QuerySource::Train,
                QuerySource::Test if !has_test => {
                    return Err(StepError::Other(anyhow::anyhow!(
                        "query_vectors source is 'test' but the vector source has no test set"
                    )))
                }
                other => other,
            };
            tracing::info!(step = step.name(), source = ?qsource, "query vectors");
            let mut items = Vec::new();
            for ii in 0..ctx.plan.indexes.len() {
                let mut rng = disc(&[ii as u64, u64::MAX]);
                let n = o.count.sample(&mut rng);
                for q in 0..n {
                    items.push((ii, q));
                }
            }
            let futs: Vec<_> = items
                .into_iter()
                .map(|(ii, q)| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    let o = o.clone();
                    let filter = filter.clone();
                    let mut rng = disc(&[ii as u64, q]);
                    async move {
                        let ip = &ctx.plan.indexes[ii];
                        let i: u64 = rng.random();
                        let v = match qsource {
                            QuerySource::Test => {
                                ctx.source.test_vector(i).expect("test set present")
                            }
                            // rows 0..num_vectors are the ones inserted into this index
                            QuerySource::Train => {
                                ctx.source.vector(i % ip.num_vectors.max(1), &mut rng)
                            }
                            _ => ctx.source.vector(i, &mut rng),
                        };
                        let req = ctx
                            .clients
                            .vectors
                            .query_vectors()
                            .vector_bucket_name(&ip.bucket)
                            .index_name(&ip.name)
                            .top_k(o.top_k as i32)
                            .query_vector(VectorData::Float32(v))
                            .set_filter(filter)
                            .return_distance(o.return_distance)
                            .return_metadata(o.return_metadata);
                        let mut c = req.customize().interceptor(m.bytes.clone());
                        if o.post_filtering {
                            let extra = post_filtering_extra();
                            c = c.mutate_request(move |r| merge_json_body(r, &extra));
                        }
                        let _ = measured(&m, c.send()).await;
                    }
                })
                .collect();
            run_concurrent(threads, futs).await;
        }

        Step::DeleteIndex => {
            let futs: Vec<_> = (0..ctx.plan.indexes.len())
                .map(|ii| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    async move {
                        let ip = &ctx.plan.indexes[ii];
                        let ok = measured(
                            &m,
                            ctx.clients
                                .vectors
                                .delete_index()
                                .vector_bucket_name(&ip.bucket)
                                .index_name(&ip.name)
                                .customize()
                                .interceptor(m.bytes.clone())
                                .send(),
                        )
                        .await
                        .is_ok();
                        (ii, ok)
                    }
                })
                .collect();
            for (ii, ok) in run_concurrent(threads, futs).await {
                if ok {
                    state[ii].keys.clear();
                }
            }
        }

        Step::DeleteVectorBucket => {
            let futs: Vec<_> = ctx
                .plan
                .buckets
                .iter()
                .cloned()
                .map(|bucket| {
                    let ctx = ctx.clone();
                    let m = m.clone();
                    async move {
                        let _ = measured(
                            &m,
                            ctx.clients
                                .vectors
                                .delete_vector_bucket()
                                .vector_bucket_name(&bucket)
                                .customize()
                                .interceptor(m.bytes.clone())
                                .send(),
                        )
                        .await;
                        if ctx.cfg.entities.vector_buckets.create_backing_bucket {
                            // Not measured; see CreateVectorBucket.
                            if let Err(e) =
                                ctx.clients.s3.delete_bucket().bucket(&bucket).send().await
                            {
                                m.record_err(
                                    format!("S3DeleteBucket:{}", error_code(&e)),
                                    error_detail(&e),
                                );
                            }
                        }
                    }
                })
                .collect();
            run_concurrent(threads, futs).await;
        }
    }
    Ok(())
}

/// Best-effort removal of everything under the configured bucket prefix.
pub async fn cleanup(cfg: &Config) -> Result<()> {
    let clients = client::build(&cfg.connection);
    let prefix = &cfg.entities.vector_buckets.prefix;
    let mut buckets = Vec::new();
    let mut pages = clients
        .vectors
        .list_vector_buckets()
        .prefix(prefix)
        .into_paginator()
        .send();
    let mut listed = true;
    while let Some(page) = pages.next().await {
        match page {
            Ok(page) => buckets.extend(
                page.vector_buckets()
                    .iter()
                    .map(|b| b.vector_bucket_name().to_string()),
            ),
            Err(e) => {
                // RGW returns creationTime as an ISO string, which the SDK
                // rejects. Fall back to the backing buckets, which share
                // the vector bucket names.
                tracing::warn!("listing vector buckets failed: {}", error_detail(&e));
                listed = false;
                break;
            }
        }
    }
    if !listed {
        if !cfg.entities.vector_buckets.create_backing_bucket {
            anyhow::bail!("cannot list vector buckets, and no backing buckets to fall back to");
        }
        buckets.clear();
        let out = clients
            .s3
            .list_buckets()
            .send()
            .await
            .context("listing S3 buckets")?;
        buckets.extend(
            out.buckets()
                .iter()
                .filter_map(|b| b.name())
                .filter(|n| n.starts_with(prefix.as_str()))
                .map(str::to_string),
        );
        tracing::info!(
            "using {} backing bucket(s) with prefix '{prefix}'",
            buckets.len()
        );
    }
    for bucket in &buckets {
        let mut indexes = Vec::new();
        let mut pages = clients
            .vectors
            .list_indexes()
            .vector_bucket_name(bucket)
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            let page = page.with_context(|| format!("listing indexes of {bucket}"))?;
            indexes.extend(page.indexes().iter().map(|i| i.index_name().to_string()));
        }
        for index in &indexes {
            tracing::info!("deleting index {bucket}/{index}");
            if let Err(e) = clients
                .vectors
                .delete_index()
                .vector_bucket_name(bucket)
                .index_name(index)
                .send()
                .await
            {
                tracing::warn!("delete index {bucket}/{index}: {}", error_detail(&e));
            }
        }
        tracing::info!("deleting vector bucket {bucket}");
        if let Err(e) = clients
            .vectors
            .delete_vector_bucket()
            .vector_bucket_name(bucket)
            .send()
            .await
        {
            tracing::warn!("delete vector bucket {bucket}: {}", error_detail(&e));
        }
    }
    if cfg.entities.vector_buckets.create_backing_bucket {
        let out = clients
            .s3
            .list_buckets()
            .send()
            .await
            .context("listing S3 buckets")?;
        for b in out.buckets() {
            let Some(name) = b.name() else { continue };
            if !name.starts_with(prefix.as_str()) {
                continue;
            }
            tracing::info!("deleting backing bucket {name}");
            if let Err(e) = clients.s3.delete_bucket().bucket(name).send().await {
                tracing::warn!("delete bucket {name}: {}", error_detail(&e));
            }
        }
    }
    println!(
        "cleanup: {} vector bucket(s) with prefix '{prefix}'",
        buckets.len()
    );
    Ok(())
}
