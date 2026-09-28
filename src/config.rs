//! YAML configuration schema and validation.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::sample::NumSpec;

/// Limits enforced by the S3 Vectors API (AWS and RGW).
pub mod limits {
    pub const MAX_PUT_BATCH: u64 = 500;
    pub const MAX_DELETE_BATCH: u64 = 500;
    pub const MAX_GET_BATCH: u64 = 100;
    pub const MAX_LIST_PAGE: u64 = 1000;
    pub const MAX_DIMENSION: u32 = 4096;
    pub const MAX_TOP_K: u32 = 10000;
    pub const MAX_METADATA_KEYS: usize = 50;
    pub const MAX_FILTERABLE_KEYS: usize = 10;
    pub const MAX_NON_FILTERABLE_KEYS: usize = 10;
    pub const MAX_KEY_NAME_LEN: usize = 63;
    pub const MAX_NAME_LEN: usize = 63;
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub connection: Connection,
    #[serde(with = "serde_yaml_ng::with::singleton_map_recursive")]
    pub entities: Entities,
    #[serde(with = "serde_yaml_ng::with::singleton_map_recursive")]
    pub flow: Vec<Step>,
    #[serde(default)]
    pub execution: Execution,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub endpoint: String,
    pub access_key: String,
    pub secret_key: String,
    #[serde(default = "default_region")]
    pub region: String,
    /// Number of retries the SDK may perform per call. 0 disables retries so
    /// that every failure is visible in the error counters.
    #[serde(default)]
    pub retries: u32,
    /// Per-attempt timeout in seconds. None means the SDK default.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Use path-style addressing for the backing S3 bucket operations.
    #[serde(default = "default_true")]
    pub force_path_style: bool,
}

fn default_region() -> String {
    "us-east-1".to_string()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entities {
    pub vector_buckets: BucketsSpec,
    pub indexes_per_bucket: IndexesSpec,
    pub index: IndexConfig,
    pub vectors_per_index: NumSpec,
    #[serde(default = "default_insert_batch")]
    pub insert_batch_size: NumSpec,
    #[serde(default = "default_key_prefix")]
    pub key_prefix: String,
    pub vectors: VectorsSpec,
}

fn default_insert_batch() -> NumSpec {
    NumSpec::Const(100)
}
fn default_key_prefix() -> String {
    "vec-".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BucketsSpec {
    pub count: NumSpec,
    #[serde(default = "default_bucket_prefix")]
    pub prefix: String,
    /// Create (and delete) the regular S3 bucket that backs the vector bucket.
    /// Required for RGW with the `rgw` backend; must be false against AWS.
    #[serde(default = "default_true")]
    pub create_backing_bucket: bool,
}

fn default_bucket_prefix() -> String {
    "s3vt-".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexesSpec {
    pub count: NumSpec,
    #[serde(default = "default_index_prefix")]
    pub prefix: String,
}

fn default_index_prefix() -> String {
    "idx-".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    Cosine,
    Euclidean,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
pub enum FilterableType {
    #[default]
    String,
    Number,
    Boolean,
    StringList,
    NumberList,
    BooleanList,
}

impl FilterableType {
    pub fn as_str(&self) -> &'static str {
        match self {
            FilterableType::String => "String",
            FilterableType::Number => "Number",
            FilterableType::Boolean => "Boolean",
            FilterableType::StringList => "StringList",
            FilterableType::NumberList => "NumberList",
            FilterableType::BooleanList => "BooleanList",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilterableKey {
    pub name: String,
    #[serde(default, rename = "type")]
    pub kind: FilterableType,
    #[serde(default)]
    pub must_exist: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    /// Required with a random vector source. May be omitted with a dataset
    /// or file source, in which case the dataset's dimension is used.
    #[serde(default)]
    pub dimension: Option<u32>,
    #[serde(default = "default_metric")]
    pub metric: Metric,
    /// RGW extension: typed metadata columns used for pre-filtering.
    #[serde(default)]
    pub filterable_keys: Vec<FilterableKey>,
    #[serde(default)]
    pub non_filterable_keys: Vec<String>,
}

fn default_metric() -> Metric {
    Metric::Cosine
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VectorsSpec {
    pub source: VectorSourceSpec,
    #[serde(default)]
    pub metadata: Vec<MetadataField>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorSourceSpec {
    /// Random vectors around a set of centroids.
    Random(RandomSpec),
    /// A named, downloadable dataset (see `vectors::datasets`).
    Dataset(String),
    /// A local file in `.fvecs`, `.npy` or `.hdf5` format.
    File(PathBuf),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RandomSpec {
    #[serde(default = "default_centroids")]
    pub centroids: u32,
    /// Each centroid component is drawn uniformly from this range. Together
    /// with `stddev` it sets how much the clusters overlap.
    #[serde(default = "default_centroid_range")]
    pub centroid_range: [f64; 2],
    #[serde(default)]
    pub mean: f64,
    #[serde(default = "default_stddev")]
    pub stddev: f64,
    /// Normalize each vector to unit length (useful for cosine indexes).
    #[serde(default)]
    pub normalize: bool,
}

fn default_centroids() -> u32 {
    1
}
fn default_centroid_range() -> [f64; 2] {
    [-1.0, 1.0]
}
fn default_stddev() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataField {
    pub name: String,
    pub value: ValueSpec,
    /// Probability (0.0 to 1.0) that the field is present on a given vector.
    #[serde(default = "default_probability")]
    pub probability: f64,
}

fn default_probability() -> f64 {
    1.0
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSpec {
    /// A constant JSON value (string, number, bool, list or object).
    Const(serde_json::Value),
    /// A random alphanumeric string of the given length.
    RandomString(usize),
    /// A number sampled uniformly from `[lo, hi]`. Integer bounds give an
    /// integer value, otherwise a float.
    Range([f64; 2]),
    /// A value sampled uniformly from the list.
    Choice(Vec<serde_json::Value>),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    CreateVectorBucket,
    CreateIndex,
    InsertVectors,
    GetVectors(GetVectorsOpts),
    ListVectors(ListVectorsOpts),
    DeleteVectors(DeleteVectorsOpts),
    UpdateVectors(UpdateVectorsOpts),
    QueryVectors(QueryVectorsOpts),
    DeleteIndex,
    DeleteVectorBucket,
}

impl Step {
    pub fn name(&self) -> &'static str {
        match self {
            Step::CreateVectorBucket => "create_vector_bucket",
            Step::CreateIndex => "create_index",
            Step::InsertVectors => "insert_vectors",
            Step::GetVectors(_) => "get_vectors",
            Step::ListVectors(_) => "list_vectors",
            Step::DeleteVectors(_) => "delete_vectors",
            Step::UpdateVectors(_) => "update_vectors",
            Step::QueryVectors(_) => "query_vectors",
            Step::DeleteIndex => "delete_index",
            Step::DeleteVectorBucket => "delete_vector_bucket",
        }
    }

    /// Steps that move vector payloads and therefore report throughput.
    pub fn is_vector_op(&self) -> bool {
        matches!(
            self,
            Step::InsertVectors
                | Step::GetVectors(_)
                | Step::ListVectors(_)
                | Step::DeleteVectors(_)
                | Step::UpdateVectors(_)
                | Step::QueryVectors(_)
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct GetVectorsOpts {
    pub percent: f64,
    pub batch_size: NumSpec,
    pub return_data: bool,
    pub return_metadata: bool,
}

impl Default for GetVectorsOpts {
    fn default() -> Self {
        Self {
            percent: 100.0,
            batch_size: NumSpec::Const(limits::MAX_GET_BATCH),
            return_data: true,
            return_metadata: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ListVectorsOpts {
    pub batch_size: NumSpec,
    pub return_data: bool,
    pub return_metadata: bool,
}

impl Default for ListVectorsOpts {
    fn default() -> Self {
        Self {
            batch_size: NumSpec::Const(500),
            return_data: false,
            return_metadata: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DeleteVectorsOpts {
    pub percent: f64,
    pub batch_size: NumSpec,
}

impl Default for DeleteVectorsOpts {
    fn default() -> Self {
        Self {
            percent: 100.0,
            batch_size: NumSpec::Const(limits::MAX_DELETE_BATCH),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct UpdateVectorsOpts {
    pub percent: f64,
    /// Defaults to `entities.insert_batch_size`.
    pub batch_size: Option<NumSpec>,
}

impl Default for UpdateVectorsOpts {
    fn default() -> Self {
        Self {
            percent: 100.0,
            batch_size: None,
        }
    }
}

/// Where `query_vectors` takes its query vectors from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum QuerySource {
    /// `test` when the dataset has a held-out query set, otherwise `train`.
    #[default]
    Auto,
    /// The dataset's held-out query vectors (the `test` dataset of an
    /// ann-benchmarks HDF5 file). Fails for a source without one.
    Test,
    /// A row that was inserted into the index (rows 0 to `vectors_per_index`
    /// of the dataset). With a random source, a fresh random vector.
    Train,
    /// Any row of the whole dataset, inserted or not. With a random source,
    /// a fresh random vector.
    Random,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct QueryVectorsOpts {
    /// Number of queries to run per index.
    pub count: NumSpec,
    /// Where the query vectors come from. See `QuerySource`.
    pub source: QuerySource,
    pub top_k: u32,
    pub return_distance: bool,
    pub return_metadata: bool,
    /// Optional metadata filter, in the S3 Vectors filter syntax.
    pub filter: Option<serde_json::Value>,
    /// RGW extension: force post-filtering over the JSON metadata.
    pub post_filtering: bool,
}

impl Default for QueryVectorsOpts {
    fn default() -> Self {
        Self {
            count: NumSpec::Const(100),
            source: QuerySource::Auto,
            top_k: 10,
            return_distance: true,
            return_metadata: false,
            filter: None,
            post_filtering: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Execution {
    pub threads: usize,
    pub repeat: u32,
    /// Seed for all random choices. A random seed is drawn (and printed) when absent.
    pub seed: Option<u64>,
    /// Directory where downloaded datasets are cached.
    pub datasets_dir: PathBuf,
    /// Log at most this many errors per step (all are counted).
    pub max_logged_errors: u64,
}

impl Default for Execution {
    fn default() -> Self {
        Self {
            threads: 1,
            repeat: 1,
            seed: None,
            datasets_dir: PathBuf::from("datasets"),
            max_logged_errors: 10,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        let cfg: Config = serde_yaml_ng::from_str(&text)
            .with_context(|| format!("parsing config file {}", path.display()))?;
        cfg.validate()
            .with_context(|| format!("invalid config file {}", path.display()))?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();
        let e = &self.entities;

        check_num(
            &mut errors,
            "entities.vector_buckets.count",
            &e.vector_buckets.count,
            1,
            u64::MAX,
        );
        check_num(
            &mut errors,
            "entities.indexes_per_bucket.count",
            &e.indexes_per_bucket.count,
            1,
            u64::MAX,
        );
        check_num(
            &mut errors,
            "entities.vectors_per_index",
            &e.vectors_per_index,
            0,
            u64::MAX,
        );
        check_num(
            &mut errors,
            "entities.insert_batch_size",
            &e.insert_batch_size,
            1,
            limits::MAX_PUT_BATCH,
        );

        match e.index.dimension {
            Some(d) if d == 0 || d > limits::MAX_DIMENSION => errors.push(format!(
                "entities.index.dimension must be between 1 and {}",
                limits::MAX_DIMENSION
            )),
            None if matches!(e.vectors.source, VectorSourceSpec::Random(_)) => errors
                .push("entities.index.dimension is required with a random vector source".into()),
            _ => {}
        }
        if let VectorSourceSpec::Dataset(name) = &e.vectors.source {
            match crate::vectors::find_dataset(name) {
                None => errors.push(format!(
                    "entities.vectors.source.dataset: unknown dataset '{name}'; known: {}",
                    crate::vectors::dataset_names()
                )),
                Some(d) => {
                    if let Some(dim) = e.index.dimension {
                        if dim as usize != d.dimension {
                            errors.push(format!(
                                "entities.index.dimension is {dim} but dataset '{name}' has dimension {}",
                                d.dimension
                            ));
                        }
                    }
                    if d.metric != e.index.metric {
                        errors.push(format!(
                            "entities.index.metric is {:?} but dataset '{name}' is built for {:?}",
                            e.index.metric, d.metric
                        ));
                    }
                }
            }
        }
        if e.index.filterable_keys.len() > limits::MAX_FILTERABLE_KEYS {
            errors.push(format!(
                "entities.index.filterable_keys: at most {} keys",
                limits::MAX_FILTERABLE_KEYS
            ));
        }
        if e.index.non_filterable_keys.len() > limits::MAX_NON_FILTERABLE_KEYS {
            errors.push(format!(
                "entities.index.non_filterable_keys: at most {} keys",
                limits::MAX_NON_FILTERABLE_KEYS
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for k in &e.index.filterable_keys {
            check_key_name(&mut errors, "entities.index.filterable_keys", &k.name);
            if !seen.insert(k.name.as_str()) {
                errors.push(format!(
                    "entities.index: metadata key '{}' declared more than once",
                    k.name
                ));
            }
        }
        for k in &e.index.non_filterable_keys {
            check_key_name(&mut errors, "entities.index.non_filterable_keys", k);
            if !seen.insert(k.as_str()) {
                errors.push(format!(
                    "entities.index: metadata key '{}' declared more than once",
                    k
                ));
            }
        }
        if e.vectors.metadata.len() > limits::MAX_METADATA_KEYS {
            errors.push(format!(
                "entities.vectors.metadata: at most {} fields",
                limits::MAX_METADATA_KEYS
            ));
        }
        let mut md_names = std::collections::HashSet::new();
        for f in &e.vectors.metadata {
            check_key_name(&mut errors, "entities.vectors.metadata", &f.name);
            if !md_names.insert(f.name.as_str()) {
                errors.push(format!(
                    "entities.vectors.metadata: field '{}' defined more than once",
                    f.name
                ));
            }
            if !(0.0..=1.0).contains(&f.probability) {
                errors.push(format!(
                    "entities.vectors.metadata.{}: probability must be within [0, 1]",
                    f.name
                ));
            }
            match &f.value {
                ValueSpec::Range([lo, hi]) if lo > hi => errors.push(format!(
                    "entities.vectors.metadata.{}: range lo > hi",
                    f.name
                )),
                ValueSpec::Choice(v) if v.is_empty() => errors.push(format!(
                    "entities.vectors.metadata.{}: choice list is empty",
                    f.name
                )),
                ValueSpec::Const(serde_json::Value::Null) => errors.push(format!(
                    "entities.vectors.metadata.{}: null values are rejected by the API",
                    f.name
                )),
                _ => {}
            }
        }
        for k in &e.index.filterable_keys {
            if k.must_exist {
                match e.vectors.metadata.iter().find(|f| f.name == k.name) {
                    None => errors.push(format!(
                        "entities.index.filterable_keys.{}: must_exist is set but no metadata field generates it",
                        k.name
                    )),
                    Some(f) if f.probability < 1.0 => errors.push(format!(
                        "entities.index.filterable_keys.{}: must_exist is set but the metadata field has probability < 1",
                        k.name
                    )),
                    _ => {}
                }
            }
        }
        if let VectorSourceSpec::Random(r) = &e.vectors.source {
            if r.centroids == 0 {
                errors.push("entities.vectors.source.random.centroids must be >= 1".into());
            }
            if r.stddev < 0.0 {
                errors.push("entities.vectors.source.random.stddev must be >= 0".into());
            }
            if r.centroid_range[0] > r.centroid_range[1] {
                errors.push("entities.vectors.source.random.centroid_range: lo > hi".into());
            }
        }
        check_name_prefix(
            &mut errors,
            "entities.vector_buckets.prefix",
            &e.vector_buckets.prefix,
        );
        check_name_prefix(
            &mut errors,
            "entities.indexes_per_bucket.prefix",
            &e.indexes_per_bucket.prefix,
        );

        if self.flow.is_empty() {
            errors.push("flow must contain at least one step".into());
        }
        for (i, step) in self.flow.iter().enumerate() {
            let p = format!("flow[{}] ({})", i, step.name());
            match step {
                Step::GetVectors(o) => {
                    check_percent(&mut errors, &p, o.percent);
                    check_num(
                        &mut errors,
                        &format!("{p}.batch_size"),
                        &o.batch_size,
                        1,
                        limits::MAX_GET_BATCH,
                    );
                }
                Step::ListVectors(o) => {
                    check_num(
                        &mut errors,
                        &format!("{p}.batch_size"),
                        &o.batch_size,
                        1,
                        limits::MAX_LIST_PAGE,
                    );
                }
                Step::DeleteVectors(o) => {
                    check_percent(&mut errors, &p, o.percent);
                    check_num(
                        &mut errors,
                        &format!("{p}.batch_size"),
                        &o.batch_size,
                        1,
                        limits::MAX_DELETE_BATCH,
                    );
                }
                Step::UpdateVectors(o) => {
                    check_percent(&mut errors, &p, o.percent);
                    if let Some(b) = &o.batch_size {
                        check_num(
                            &mut errors,
                            &format!("{p}.batch_size"),
                            b,
                            1,
                            limits::MAX_PUT_BATCH,
                        );
                    }
                }
                Step::QueryVectors(o) => {
                    check_num(&mut errors, &format!("{p}.count"), &o.count, 0, u64::MAX);
                    if o.source == QuerySource::Test
                        && matches!(self.entities.vectors.source, VectorSourceSpec::Random(_))
                    {
                        errors.push(format!("{p}.source: 'test' needs a dataset with a test set, not a random source"));
                    }
                    if o.top_k == 0 || o.top_k > limits::MAX_TOP_K {
                        errors.push(format!(
                            "{p}.top_k must be between 1 and {}",
                            limits::MAX_TOP_K
                        ));
                    }
                    if let Some(f) = &o.filter {
                        if !f.is_object() {
                            errors.push(format!("{p}.filter must be a JSON object"));
                        }
                    }
                }
                _ => {}
            }
        }
        if self.execution.threads == 0 {
            errors.push("execution.threads must be >= 1".into());
        }
        if self.execution.repeat == 0 {
            errors.push("execution.repeat must be >= 1".into());
        }

        if errors.is_empty() {
            Ok(())
        } else {
            bail!("{}", errors.join("\n"));
        }
    }
}

fn check_num(errors: &mut Vec<String>, path: &str, n: &NumSpec, lo: u64, hi: u64) {
    match n {
        NumSpec::Const(v) => {
            if *v < lo || *v > hi {
                errors.push(format!("{path}: {v} is outside [{lo}, {hi}]"));
            }
        }
        NumSpec::Range { min, max } => {
            if min > max {
                errors.push(format!("{path}: min {min} > max {max}"));
            }
            if *min < lo || *max > hi {
                errors.push(format!(
                    "{path}: range [{min}, {max}] is outside [{lo}, {hi}]"
                ));
            }
        }
    }
}

fn check_percent(errors: &mut Vec<String>, path: &str, p: f64) {
    if !(0.0..=100.0).contains(&p) {
        errors.push(format!("{path}.percent must be within [0, 100]"));
    }
}

fn check_key_name(errors: &mut Vec<String>, path: &str, name: &str) {
    if name.is_empty() || name.len() > limits::MAX_KEY_NAME_LEN {
        errors.push(format!(
            "{path}: key name '{name}' must be 1 to {} characters",
            limits::MAX_KEY_NAME_LEN
        ));
    }
    if name.starts_with('_') || name.contains('.') || name.contains('`') {
        errors.push(format!(
            "{path}: key name '{name}' must not start with '_' or contain '.' or '`'"
        ));
    }
}

fn check_name_prefix(errors: &mut Vec<String>, path: &str, prefix: &str) {
    // The generated name is prefix + decimal number (at least one digit); the
    // full name must stay within the API's 3..=63 range and S3 bucket rules.
    if prefix.len() + 1 > limits::MAX_NAME_LEN {
        errors.push(format!(
            "{path}: prefix too long ({} characters)",
            prefix.len()
        ));
    }
    if !prefix
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        errors.push(format!(
            "{path}: prefix must be lowercase letters, digits and '-'"
        ));
    }
}

/// An example configuration, printed by `s3vtest example`.
pub const EXAMPLE_YAML: &str = r#"# s3vtest configuration
connection:
  endpoint: http://localhost:8000
  access_key: 0555b35654ad1656d804
  secret_key: h7GhxuBLTrlhVUyxSPUKUV8r/2EI4ngqJxD7iBdBYLhwluN30JaT3Q==
  region: default
  retries: 0            # SDK retries per call; 0 makes every failure visible
  # timeout_secs: 30    # per-attempt timeout

entities:
  vector_buckets:
    count: 1                     # constant, or {min: 1, max: 4}
    prefix: s3vt-
    create_backing_bucket: true  # RGW needs a regular S3 bucket with the same name
  indexes_per_bucket:
    count: {min: 1, max: 2}
    prefix: idx-
  index:
    dimension: 128               # required for a random source; taken from the dataset otherwise
    metric: cosine               # cosine | euclidean; must match the dataset (-angular => cosine)
    filterable_keys:             # RGW extension (typed, pre-filtered columns)
      - {name: year, type: Number, must_exist: true}
      - {name: genre}            # type defaults to String
    non_filterable_keys: [blob]
  vectors_per_index: 2000
  insert_batch_size: {min: 100, max: 300}   # RGW rejects bodies over rgw_max_put_param_size (1 MiB) with 405
  key_prefix: vec-
  vectors:
    source:
      random:
        centroids: 8                 # cluster centers, drawn once from the seed
        centroid_range: [-1.0, 1.0]  # uniform range of each centroid component
        mean: 0.0                    # per-component Gaussian noise around the centroid;
        stddev: 0.1                  # stddev relative to centroid_range sets cluster overlap
        normalize: true              # unit length, for cosine indexes
      # dataset: sift-128-euclidean      # downloaded into execution.datasets_dir (needs the hdf5 feature);
      #                                  # see `s3vtest datasets` for the list
      # file: /data/sift_base.fvecs      # .fvecs, .npy or .hdf5
    metadata:
      - {name: year,  value: {range: [1950, 2025]}}
      - {name: genre, value: {choice: [rock, pop, jazz]}, probability: 0.9}
      - {name: tag,   value: {random_string: 16}}
      - {name: blob,  value: {random_string: 256}, probability: 0.5}
      - {name: src,   value: {const: s3vtest}}

flow:
  - create_vector_bucket
  - create_index
  - insert_vectors
  - get_vectors: {percent: 10, batch_size: 100, return_data: true, return_metadata: true}
  - list_vectors: {batch_size: 500, return_data: false, return_metadata: false}
  # source: auto (default) | test | train | random; auto = the dataset's test set when it
  # has one, else train (inserted rows); with a random vector source it is a fresh vector
  - query_vectors: {count: 50, top_k: 10, source: auto, return_distance: true}
  - update_vectors: {percent: 5}
  - delete_vectors: {percent: 20, batch_size: 500}
  - delete_index
  - delete_vector_bucket

execution:
  threads: 8
  repeat: 1
  seed: 42
  datasets_dir: datasets
  max_logged_errors: 10
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses_and_validates() {
        let cfg: Config = serde_yaml_ng::from_str(EXAMPLE_YAML).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.flow.len(), 10);
    }

    #[test]
    fn dataset_checks() {
        let mut cfg: Config = serde_yaml_ng::from_str(EXAMPLE_YAML).unwrap();
        cfg.entities.vectors.source = VectorSourceSpec::Dataset("sift-128-euclidean".into());
        cfg.entities.index.metric = Metric::Euclidean;
        cfg.validate().unwrap();
        cfg.entities.index.dimension = None;
        cfg.validate().unwrap();
        cfg.entities.index.metric = Metric::Cosine;
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("built for"));
        cfg.entities.index.metric = Metric::Euclidean;
        cfg.entities.index.dimension = Some(64);
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("dimension"));
        cfg.entities.vectors.source = VectorSourceSpec::Dataset("nope".into());
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("unknown dataset"));
        cfg.entities.vectors.source = VectorSourceSpec::Random(RandomSpec {
            centroids: 1,
            centroid_range: [-1.0, 1.0],
            mean: 0.0,
            stddev: 1.0,
            normalize: false,
        });
        cfg.entities.index.dimension = None;
        assert!(cfg.validate().unwrap_err().to_string().contains("required"));
    }

    #[test]
    fn query_source_test_needs_dataset() {
        let mut cfg: Config = serde_yaml_ng::from_str(EXAMPLE_YAML).unwrap();
        let q = QueryVectorsOpts {
            source: QuerySource::Test,
            ..Default::default()
        };
        cfg.flow = vec![Step::QueryVectors(q)];
        assert!(cfg.validate().unwrap_err().to_string().contains("test set"));
        cfg.entities.vectors.source = VectorSourceSpec::Dataset("sift-128-euclidean".into());
        cfg.entities.index.metric = Metric::Euclidean;
        cfg.validate().unwrap();
    }

    #[test]
    fn rejects_bad_batch() {
        let mut cfg: Config = serde_yaml_ng::from_str(EXAMPLE_YAML).unwrap();
        cfg.entities.insert_batch_size = NumSpec::Const(501);
        assert!(cfg.validate().is_err());
    }
}
