//! Query correctness: recall against an exact nearest-neighbour search
//! computed by the tool over the vectors the index actually holds.
//!
//! The index is listed with its data, so the ground truth is right after any
//! sequence of inserts, updates and deletes, and for any vector source. The
//! queries are run against the server, and every returned key is checked
//! against the exact top-k. Returned distances are compared with recomputed
//! ones as well, which catches a metric definition mismatch.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use futures::stream::{self, StreamExt};
use rand::{Rng, SeedableRng};
use rayon::prelude::*;
use serde::Serialize;

use crate::client;
use crate::config::{Config, Metric, QuerySource};
use crate::flow::{make_plan, IndexPlan};
use crate::metrics::error_detail;
use crate::sample::{derive_rng, SeededRng};
use crate::vectors::{self, VectorSource};

#[derive(Debug, Clone)]
pub struct RecallOpts {
    /// Queries per index.
    pub queries: u64,
    pub top_k: u32,
    pub source: QuerySource,
    /// A returned vector counts as correct when its true distance is within
    /// this relative tolerance of the k-th true distance (ties).
    pub epsilon: f64,
    /// Concurrent requests.
    pub threads: usize,
    /// ListVectors page size used to read the index back.
    pub page_size: i32,
    /// Do not list the index: take the ground truth from the dataset file.
    /// The caller guarantees that the index holds the complete dataset and
    /// nothing else.
    pub no_list: bool,
}

/// A query (its test-vector id and vector) and the server's answer: (key,
/// distance) pairs, or None when the request failed.
type QueryResult = (u64, Vec<f32>, Option<Vec<(String, Option<f32>)>>);

/// The vectors an index holds, as listed from the server.
struct Mirror {
    dim: usize,
    keys: Vec<String>,
    data: Vec<f32>, // row-major
    by_key: HashMap<String, usize>,
}

impl Mirror {
    fn row(&self, r: usize) -> &[f32] {
        &self.data[r * self.dim..(r + 1) * self.dim]
    }
    fn len(&self) -> usize {
        self.keys.len()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct IndexRecall {
    pub bucket: String,
    pub index: String,
    /// "listed": exact search over the vectors listed from the index.
    /// "file": the dataset's own ground truth, index not listed.
    pub ground_truth: String,
    pub vectors: usize,
    pub queries: u64,
    pub top_k: u32,
    pub source: String,
    /// Queries that failed or returned no result set.
    pub query_errors: u64,
    pub recall_mean: f64,
    pub recall_min: f64,
    pub recall_p10: f64,
    /// Fraction of queries whose recall is exactly 1.
    pub perfect_fraction: f64,
    /// Returned keys that the index listing does not contain.
    pub unknown_keys: u64,
    /// Results with no distance although one was requested.
    pub missing_distances: u64,
    /// Largest relative difference between a returned distance and the one
    /// recomputed with the index metric.
    pub distance_max_rel_err: f64,
    /// For euclidean indexes: the same, against squared euclidean distance.
    /// When this is small and the plain one is not, the server returns
    /// squared distances.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance_max_rel_err_squared: Option<f64>,
    pub list_secs: f64,
    pub query_secs: f64,
    pub compute_secs: f64,
}

pub async fn recall(cfg: &Config, opts: &RecallOpts) -> Result<Vec<IndexRecall>> {
    let seed = cfg.execution.seed.unwrap_or_else(rand::random);
    let mut rng = SeededRng::seed_from_u64(seed);
    let source: Arc<dyn VectorSource> = Arc::from(
        vectors::build(
            &cfg.entities.vectors.source,
            cfg.entities.index.dimension.map(|d| d as usize),
            &cfg.execution.datasets_dir,
            &mut rng,
        )
        .await
        .context("building vector source")?,
    );
    let dim = source.dimension();
    let plan = make_plan(cfg, &mut rng);
    let clients = client::build(&cfg.connection);
    let metric = cfg.entities.index.metric;

    let has_test = source.test_len().is_some();
    let qsource = match opts.source {
        QuerySource::Auto if has_test => QuerySource::Test,
        QuerySource::Auto => QuerySource::Train,
        QuerySource::Test if !has_test => {
            bail!("query source is 'test' but the vector source has no test set")
        }
        other => other,
    };
    let key_prefix = cfg.entities.key_prefix.clone();
    if opts.no_list {
        if source.neighbors(0).is_none() {
            bail!("--no-list needs a dataset with ground truth (an ann-benchmarks HDF5 file with 'test' and 'neighbors')");
        }
        if qsource != QuerySource::Test {
            bail!("--no-list works with the dataset's test vectors only; use --source test");
        }
        let train = source.train_len().unwrap_or(0);
        for ip in &plan.indexes {
            if ip.num_vectors != train {
                bail!(
                    "--no-list assumes the whole dataset is in the index, but vectors_per_index gives {} of {} rows for {}/{}",
                    ip.num_vectors, train, ip.bucket, ip.name
                );
            }
        }
        let k_avail = source.neighbors(0).map(|n| n.len()).unwrap_or(0);
        if opts.top_k as usize > k_avail {
            bail!("--no-list: the dataset has ground truth for {k_avail} neighbours, --top-k {} is too large", opts.top_k);
        }
    }
    tracing::info!(
        seed,
        indexes = plan.indexes.len(),
        queries = opts.queries,
        top_k = opts.top_k,
        source = ?qsource,
        "recall: {}",
        source.describe()
    );

    let mut out = Vec::new();
    for (ii, ip) in plan.indexes.iter().enumerate() {
        // 1. the ground truth: read the index back, or trust the dataset file
        let t = Instant::now();
        let mirror: Option<Arc<Mirror>> = if opts.no_list {
            None
        } else {
            let m = list_index(&clients.vectors, ip, dim, opts.page_size).await?;
            tracing::info!(index = %format!("{}/{}", ip.bucket, ip.name), vectors = m.len(), "listed in {:.1}s", t.elapsed().as_secs_f64());
            if m.len() == 0 {
                bail!(
                    "index {}/{} holds no vectors; run a flow that inserts first",
                    ip.bucket,
                    ip.name
                );
            }
            Some(Arc::new(m))
        };
        let list_secs = t.elapsed().as_secs_f64();

        // 2. run the queries
        let t = Instant::now();
        let futs: Vec<_> = (0..opts.queries)
            .map(|q| {
                let mut rng = derive_rng(seed, &[u64::MAX - 1, ii as u64, q]);
                let i: u64 = rng.random();
                let qi = i; // identifies the test vector for the file's ground truth
                let qv = match qsource {
                    QuerySource::Test => source.test_vector(i).expect("test set present"),
                    QuerySource::Train => source.vector(i % ip.num_vectors.max(1), &mut rng),
                    _ => source.vector(i, &mut rng),
                };
                let clients = clients.clone();
                let (bucket, index) = (ip.bucket.clone(), ip.name.clone());
                let top_k = opts.top_k as i32;
                async move {
                    let r = clients
                        .vectors
                        .query_vectors()
                        .vector_bucket_name(&bucket)
                        .index_name(&index)
                        .top_k(top_k)
                        .query_vector(aws_sdk_s3vectors::types::VectorData::Float32(qv.clone()))
                        .return_distance(true)
                        .send()
                        .await;
                    match r {
                        Ok(o) => {
                            let hits: Vec<(String, Option<f32>)> = o
                                .vectors()
                                .iter()
                                .map(|v| (v.key().to_string(), v.distance()))
                                .collect();
                            (qi, qv, Some(hits))
                        }
                        Err(e) => {
                            tracing::warn!("query failed: {}", error_detail(&e));
                            (qi, qv, None)
                        }
                    }
                }
            })
            .collect();
        let results: Vec<QueryResult> = stream::iter(futs)
            .buffer_unordered(opts.threads.max(1))
            .collect()
            .await;
        let query_secs = t.elapsed().as_secs_f64();

        // 3. exact search (or file lookup) and comparison, in parallel over the queries
        let t = Instant::now();
        let (per_query, vectors): (Vec<QueryEval>, usize) = match &mirror {
            Some(mirror) => {
                let k = (opts.top_k as usize).min(mirror.len());
                let evals = results
                    .par_iter()
                    .map(|(_, qv, hits)| {
                        evaluate(mirror, metric, qv, hits.as_deref(), k, opts.epsilon)
                    })
                    .collect();
                (evals, mirror.len())
            }
            None => {
                let k = opts.top_k as usize;
                let src = &*source;
                let evals = results
                    .par_iter()
                    .map(|(qi, qv, hits)| {
                        evaluate_file(
                            src,
                            &key_prefix,
                            metric,
                            *qi,
                            qv,
                            hits.as_deref(),
                            k,
                            opts.epsilon,
                        )
                    })
                    .collect();
                (evals, source.train_len().unwrap_or(0) as usize)
            }
        };
        let compute_secs = t.elapsed().as_secs_f64();

        out.push(summarize(
            ip,
            vectors,
            mirror.is_none(),
            metric,
            opts,
            qsource,
            &per_query,
            list_secs,
            query_secs,
            compute_secs,
        ));
        let r = out.last().unwrap();
        tracing::info!(
            index = %format!("{}/{}", ip.bucket, ip.name),
            recall_mean = format!("{:.4}", r.recall_mean),
            recall_min = format!("{:.4}", r.recall_min),
            query_errors = r.query_errors,
            unknown_keys = r.unknown_keys,
            "recall computed in {compute_secs:.1}s"
        );
    }
    Ok(out)
}

async fn list_index(
    client: &aws_sdk_s3vectors::Client,
    ip: &IndexPlan,
    dim: usize,
    page_size: i32,
) -> Result<Mirror> {
    let mut keys = Vec::new();
    let mut data = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let page = client
            .list_vectors()
            .vector_bucket_name(&ip.bucket)
            .index_name(&ip.name)
            .max_results(page_size)
            .set_next_token(token.take())
            .return_data(true)
            .return_metadata(false)
            .send()
            .await
            .map_err(|e| {
                anyhow::anyhow!("listing {}/{}: {}", ip.bucket, ip.name, error_detail(&e))
            })?;
        for v in page.vectors() {
            let Some(aws_sdk_s3vectors::types::VectorData::Float32(f)) = v.data() else {
                bail!("vector {} was listed without float32 data", v.key());
            };
            if f.len() != dim {
                bail!(
                    "vector {} has dimension {}, expected {dim}",
                    v.key(),
                    f.len()
                );
            }
            keys.push(v.key().to_string());
            data.extend_from_slice(f);
        }
        match page.next_token() {
            Some(t) => token = Some(t.to_string()),
            None => break,
        }
    }
    let by_key = keys
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, k)| (k, i))
        .collect();
    Ok(Mirror {
        dim,
        keys,
        data,
        by_key,
    })
}

/// Distance as the API defines it for the index metric.
pub fn distance(metric: Metric, a: &[f32], b: &[f32]) -> f64 {
    match metric {
        Metric::Euclidean => squared_l2(a, b).sqrt(),
        Metric::Cosine => {
            let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
            for (x, y) in a.iter().zip(b) {
                let (x, y) = (*x as f64, *y as f64);
                dot += x * y;
                na += x * x;
                nb += y * y;
            }
            if na == 0.0 || nb == 0.0 {
                1.0
            } else {
                1.0 - dot / (na.sqrt() * nb.sqrt())
            }
        }
    }
}

fn squared_l2(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = *x as f64 - *y as f64;
            d * d
        })
        .sum()
}

struct QueryEval {
    failed: bool,
    recall: f64,
    unknown: u64,
    missing_distance: u64,
    max_rel_err: f64,
    max_rel_err_squared: f64,
}

fn rel_err(returned: f64, computed: f64) -> f64 {
    (returned - computed).abs() / computed.abs().max(1e-6)
}

fn evaluate(
    mirror: &Mirror,
    metric: Metric,
    qv: &[f32],
    hits: Option<&[(String, Option<f32>)]>,
    k: usize,
    epsilon: f64,
) -> QueryEval {
    let mut ev = QueryEval {
        failed: false,
        recall: 0.0,
        unknown: 0,
        missing_distance: 0,
        max_rel_err: 0.0,
        max_rel_err_squared: 0.0,
    };
    let Some(hits) = hits else {
        ev.failed = true;
        return ev;
    };
    if k == 0 {
        ev.recall = 1.0;
        return ev;
    }
    // exact distances to every vector, then the k-th smallest
    let mut dists: Vec<f64> = (0..mirror.len())
        .map(|r| distance(metric, qv, mirror.row(r)))
        .collect();
    let kth = {
        let (_, kth, _) = dists.select_nth_unstable_by(k - 1, |a, b| a.total_cmp(b));
        *kth
    };
    let threshold = kth * (1.0 + epsilon) + 1e-9;

    let mut correct = 0usize;
    for (key, d) in hits {
        let Some(&r) = mirror.by_key.get(key) else {
            ev.unknown += 1;
            continue;
        };
        let true_d = distance(metric, qv, mirror.row(r));
        if true_d <= threshold {
            correct += 1;
        }
        match d {
            None => ev.missing_distance += 1,
            Some(d) => {
                let d = *d as f64;
                ev.max_rel_err = ev.max_rel_err.max(rel_err(d, true_d));
                if metric == Metric::Euclidean {
                    ev.max_rel_err_squared =
                        ev.max_rel_err_squared.max(rel_err(d, true_d * true_d));
                }
            }
        }
    }
    ev.recall = (correct.min(k) as f64) / k as f64;
    ev
}

/// Ground truth from the dataset file: the returned keys are mapped back to
/// dataset rows through the key prefix, and compared with the file's
/// neighbours of test vector `qi`. Distances come from the train rows held in
/// memory, so ties and returned distances are checked as in `evaluate`.
#[allow(clippy::too_many_arguments)]
fn evaluate_file(
    source: &dyn VectorSource,
    key_prefix: &str,
    metric: Metric,
    qi: u64,
    qv: &[f32],
    hits: Option<&[(String, Option<f32>)]>,
    k: usize,
    epsilon: f64,
) -> QueryEval {
    let mut ev = QueryEval {
        failed: false,
        recall: 0.0,
        unknown: 0,
        missing_distance: 0,
        max_rel_err: 0.0,
        max_rel_err_squared: 0.0,
    };
    let Some(hits) = hits else {
        ev.failed = true;
        return ev;
    };
    if k == 0 {
        ev.recall = 1.0;
        return ev;
    }
    let rows = source.train_len().unwrap_or(0);
    let neighbours = source.neighbors(qi).expect("checked before");
    let mut rng = SeededRng::seed_from_u64(0); // unused by dataset sources
    let mut row_vec = |r: u64| source.vector(r, &mut rng);
    // the k-th true distance, for the tie tolerance
    let kth_row = neighbours[k - 1].max(0) as u64;
    let kth = distance(metric, qv, &row_vec(kth_row));
    let threshold = kth * (1.0 + epsilon) + 1e-9;

    let mut correct = 0usize;
    for (key, d) in hits {
        let row = key
            .strip_prefix(key_prefix)
            .and_then(|n| n.parse::<u64>().ok())
            .filter(|r| *r < rows);
        let Some(row) = row else {
            ev.unknown += 1;
            continue;
        };
        let true_d = distance(metric, qv, &row_vec(row));
        if true_d <= threshold {
            correct += 1;
        }
        match d {
            None => ev.missing_distance += 1,
            Some(d) => {
                let d = *d as f64;
                ev.max_rel_err = ev.max_rel_err.max(rel_err(d, true_d));
                if metric == Metric::Euclidean {
                    ev.max_rel_err_squared =
                        ev.max_rel_err_squared.max(rel_err(d, true_d * true_d));
                }
            }
        }
    }
    ev.recall = (correct.min(k) as f64) / k as f64;
    ev
}

#[allow(clippy::too_many_arguments)]
fn summarize(
    ip: &IndexPlan,
    vectors: usize,
    from_file: bool,
    metric: Metric,
    opts: &RecallOpts,
    qsource: QuerySource,
    evals: &[QueryEval],
    list_secs: f64,
    query_secs: f64,
    compute_secs: f64,
) -> IndexRecall {
    let ok: Vec<&QueryEval> = evals.iter().filter(|e| !e.failed).collect();
    let mut recalls: Vec<f64> = ok.iter().map(|e| e.recall).collect();
    recalls.sort_by(|a, b| a.total_cmp(b));
    let n = recalls.len();
    let mean = if n > 0 {
        recalls.iter().sum::<f64>() / n as f64
    } else {
        0.0
    };
    let p10 = if n > 0 {
        recalls[(n as f64 * 0.10) as usize]
    } else {
        0.0
    };
    let perfect = if n > 0 {
        recalls.iter().filter(|r| **r >= 1.0).count() as f64 / n as f64
    } else {
        0.0
    };
    let max_err = ok.iter().map(|e| e.max_rel_err).fold(0.0, f64::max);
    let max_err_sq = ok.iter().map(|e| e.max_rel_err_squared).fold(0.0, f64::max);
    IndexRecall {
        bucket: ip.bucket.clone(),
        index: ip.name.clone(),
        ground_truth: if from_file { "file" } else { "listed" }.to_string(),
        vectors,
        queries: opts.queries,
        top_k: opts.top_k,
        source: format!("{qsource:?}").to_lowercase(),
        query_errors: evals.iter().filter(|e| e.failed).count() as u64,
        recall_mean: mean,
        recall_min: recalls.first().copied().unwrap_or(0.0),
        recall_p10: p10,
        perfect_fraction: perfect,
        unknown_keys: ok.iter().map(|e| e.unknown).sum(),
        missing_distances: ok.iter().map(|e| e.missing_distance).sum(),
        distance_max_rel_err: max_err,
        distance_max_rel_err_squared: (metric == Metric::Euclidean).then_some(max_err_sq),
        list_secs,
        query_secs,
        compute_secs,
    }
}

pub fn table(results: &[IndexRecall]) -> String {
    use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, CellAlignment, Table};
    let mut t = Table::new();
    t.load_style(UTF8_FULL_CONDENSED);
    t.set_header(vec![
        "index",
        "truth",
        "vectors",
        "queries",
        "k",
        "source",
        "recall mean",
        "recall min",
        "recall p10",
        "perfect %",
        "unknown keys",
        "query errors",
        "dist max rel err",
    ]);
    for r in results {
        let row = vec![
            Cell::new(format!("{}/{}", r.bucket, r.index)),
            Cell::new(&r.ground_truth),
            Cell::new(r.vectors),
            Cell::new(r.queries),
            Cell::new(r.top_k),
            Cell::new(&r.source),
            Cell::new(format!("{:.4}", r.recall_mean)),
            Cell::new(format!("{:.4}", r.recall_min)),
            Cell::new(format!("{:.4}", r.recall_p10)),
            Cell::new(format!("{:.1}", r.perfect_fraction * 100.0)),
            Cell::new(r.unknown_keys),
            Cell::new(r.query_errors),
            Cell::new(format!("{:.2e}", r.distance_max_rel_err)),
        ];
        t.add_row(row.into_iter().enumerate().map(|(i, c)| {
            if i <= 1 || i == 5 {
                c
            } else {
                c.set_alignment(CellAlignment::Right)
            }
        }));
    }
    let mut out = format!("{t}\n");
    for r in results {
        if let Some(sq) = r.distance_max_rel_err_squared {
            if r.distance_max_rel_err > 1e-3 && sq <= 1e-3 {
                out.push_str(&format!(
                    "  {}/{}: returned distances match SQUARED euclidean distance (rel err {:.2e}), not euclidean\n",
                    r.bucket, r.index, sq
                ));
            }
        }
        if r.missing_distances > 0 {
            out.push_str(&format!(
                "  {}/{}: {} results came without a distance\n",
                r.bucket, r.index, r.missing_distances
            ));
        }
        out.push_str(&format!(
            "  {}/{}: list {:.1}s, queries {:.1}s, {} {:.1}s\n",
            r.bucket,
            r.index,
            r.list_secs,
            r.query_secs,
            if r.ground_truth == "file" {
                "file ground truth"
            } else {
                "exact search"
            },
            r.compute_secs
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dataset with 4 train rows on a line, one test vector at the origin,
    /// and its neighbours in order.
    struct Fake;
    impl VectorSource for Fake {
        fn dimension(&self) -> usize {
            2
        }
        fn train_len(&self) -> Option<u64> {
            Some(4)
        }
        fn test_len(&self) -> Option<u64> {
            Some(1)
        }
        fn test_vector(&self, _i: u64) -> Option<Vec<f32>> {
            Some(vec![0.0, 0.0])
        }
        fn neighbors(&self, _i: u64) -> Option<&[i32]> {
            Some(&[2, 0, 1, 3])
        }
        fn vector(&self, i: u64, _rng: &mut SeededRng) -> Vec<f32> {
            [[1.0, 0.0], [2.0, 0.0], [0.5, 0.0], [9.0, 0.0]][(i % 4) as usize].to_vec()
        }
        fn describe(&self) -> String {
            "fake".into()
        }
    }

    #[test]
    fn file_ground_truth_maps_keys_to_rows() {
        let hits = vec![
            ("v-2".to_string(), Some(0.5f32)),
            ("v-0".to_string(), Some(1.0)),
            ("v-3".to_string(), Some(9.0)), // not among the true top-3
            ("other".to_string(), None),    // wrong prefix: unknown
            ("v-7".to_string(), None),      // beyond the dataset: unknown
        ];
        let ev = evaluate_file(
            &Fake,
            "v-",
            Metric::Euclidean,
            0,
            &[0.0, 0.0],
            Some(&hits),
            3,
            0.0,
        );
        assert!((ev.recall - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(ev.unknown, 2);
        assert_eq!(ev.missing_distance, 0);
        assert!(ev.max_rel_err < 1e-6);
        let all: Vec<_> = [2, 0, 1, 3]
            .iter()
            .map(|r| (format!("v-{r}"), None))
            .collect();
        let ev = evaluate_file(
            &Fake,
            "v-",
            Metric::Euclidean,
            0,
            &[0.0, 0.0],
            Some(&all),
            4,
            0.0,
        );
        assert!((ev.recall - 1.0).abs() < 1e-9);
        assert_eq!(ev.missing_distance, 4);
    }

    fn mirror(rows: &[&[f32]]) -> Mirror {
        let dim = rows[0].len();
        let keys: Vec<String> = (0..rows.len()).map(|i| format!("k{i}")).collect();
        let data = rows.iter().flat_map(|r| r.iter().copied()).collect();
        let by_key = keys
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, k)| (k, i))
            .collect();
        Mirror {
            dim,
            keys,
            data,
            by_key,
        }
    }

    #[test]
    fn distances() {
        assert!((distance(Metric::Euclidean, &[0.0, 0.0], &[3.0, 4.0]) - 5.0).abs() < 1e-9);
        assert!(distance(Metric::Cosine, &[1.0, 0.0], &[2.0, 0.0]).abs() < 1e-9);
        assert!((distance(Metric::Cosine, &[1.0, 0.0], &[0.0, 1.0]) - 1.0).abs() < 1e-9);
        assert!((distance(Metric::Cosine, &[1.0, 0.0], &[0.0, 0.0]) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn recall_counts_true_neighbours_and_ties() {
        let m = mirror(&[
            &[0.0, 0.0],
            &[1.0, 0.0],
            &[0.0, 1.0],
            &[5.0, 5.0],
            &[9.0, 9.0],
        ]);
        let q = [0.1f32, 0.0];
        // true top-3: k0 (0.1), k1 (0.9), k2 (~1.005)
        let hits = vec![
            ("k0".to_string(), Some(0.1f32)),
            ("k1".to_string(), Some(0.9)),
            ("k3".to_string(), Some(7.0007)),
        ];
        let ev = evaluate(&m, Metric::Euclidean, &q, Some(&hits), 3, 1e-3);
        assert!((ev.recall - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(ev.unknown, 0);
        assert!(ev.max_rel_err < 2e-3, "{}", ev.max_rel_err);

        // a tie at the k-th distance counts, an unknown key does not
        let m = mirror(&[&[1.0, 0.0], &[-1.0, 0.0], &[0.0, 3.0]]);
        let hits = vec![("k1".to_string(), None), ("nope".to_string(), None)];
        let ev = evaluate(&m, Metric::Euclidean, &[0.0, 0.0], Some(&hits), 2, 0.0);
        assert!((ev.recall - 0.5).abs() < 1e-9);
        assert_eq!(ev.unknown, 1);
        assert_eq!(ev.missing_distance, 1);

        // squared distances are detected
        let m = mirror(&[&[0.0, 0.0], &[3.0, 4.0]]);
        let hits = vec![("k1".to_string(), Some(25.0f32))];
        let ev = evaluate(&m, Metric::Euclidean, &[0.0, 0.0], Some(&hits), 1, 0.0);
        assert!(ev.max_rel_err > 1.0 && ev.max_rel_err_squared < 1e-6);

        let ev = evaluate(&m, Metric::Euclidean, &[0.0, 0.0], None, 1, 0.0);
        assert!(ev.failed);
    }
}
