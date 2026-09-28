# s3vtest

Command-line tool to test S3 Vectors functionality, performance and
reliability against Ceph RGW (or AWS). Written in Rust on top of the official
`aws-sdk-s3vectors` crate.

## Build

```sh
cargo build --release
# optional: read ann-benchmarks HDF5 datasets, linking the system libhdf5
# (hdf5-devel; on RHEL 9 it is in EPEL)
cargo build --release --features hdf5
# same, but HDF5 is built from source by cargo (needs cmake, no root)
cargo build --release --features hdf5-static
```

## Usage

```sh
s3vtest example > job.yaml        # print a commented example configuration
s3vtest datasets                  # list the downloadable datasets
s3vtest validate job.yaml         # parse and validate only
s3vtest run job.yaml              # run one job, print a table report
s3vtest run a.yaml b.yaml --report json   # several jobs concurrently, JSON report
s3vtest cleanup job.yaml          # delete leftovers under the job's bucket prefix
RUST_LOG=s3vtest=debug s3vtest run job.yaml   # more logging (stderr)
```

Exit status is 2 when any step recorded errors.

## Configuration

One YAML file per job. See `s3vtest example` for every field with comments.
Ready-made jobs are in `examples/`: `rgw-vstart.yaml` (random vectors against a
vstart RGW) and `sift-128-euclidean.yaml` (the SIFT1M dataset, needs the
`hdf5` feature).

- `connection`: endpoint, credentials, region, SDK retries (default 0 so every
  failure is counted) and per-attempt timeout.
- `entities`: how many vector buckets and indexes to create, index parameters
  (dimension, metric, RGW filterable keys, non-filterable keys), vectors per
  index, insert batch size, key prefix, vector source and metadata fields.
  Every count and batch size is either a constant or `{min, max}`, sampled
  from the job seed.
- `flow`: the ordered list of steps. Plain steps: `create_vector_bucket`,
  `create_index`, `insert_vectors`, `delete_index`, `delete_vector_bucket`.
  Parameterized steps: `get_vectors`, `list_vectors`, `delete_vectors`,
  `update_vectors`, `query_vectors`.
- `execution`: `threads` (in-flight requests), `repeat`, `seed`,
  `datasets_dir`, `max_logged_errors`.

### Vector sources

- `random: {centroids, centroid_range, mean, stddev, normalize}`: each
  centroid component is drawn once, from the job seed, uniformly in
  `centroid_range` (default [-1, 1]). Every vector is a random centroid plus
  per-component Gaussian noise `N(mean, stddev)`. The ratio of `stddev` to
  the centroid range sets how much the clusters overlap: with a range width
  of 2 and `stddev: 0.1` the clusters are well separated, with `stddev: 1.0`
  they blend together. `entities.index.dimension` is required.
- `file: path`: a local dataset in `.fvecs` (dimension per row, as in the
  TEXMEX SIFT1M and GIST1M corpora), `.npy` (2-D float32 or float64 array) or
  `.hdf5` format (ann-benchmarks layout, `train` dataset; needs the `hdf5`
  feature). The TEXMEX corpora are served over FTP only, so download them by
  hand and point `file:` at the `.fvecs` file.
- `dataset: name`: one of the ann-benchmarks datasets below, downloaded once
  from `http://ann-benchmarks.com/<name>.hdf5` into `execution.datasets_dir`
  and reused afterwards. They are HDF5 files, so the `hdf5` feature is
  required. The whole `train` dataset is loaded into memory as float32
  (about 4 GB for `deep-image-96-angular`). `s3vtest datasets` prints the
  same list.

| `dataset:` name | Dimension | Metric | Train vectors |
| --- | --- | --- | --- |
| fashion-mnist-784-euclidean | 784 | euclidean | 60,000 |
| sift-128-euclidean | 128 | euclidean | 1,000,000 |
| gist-960-euclidean | 960 | euclidean | 1,000,000 |
| glove-25-angular | 25 | cosine | 1,183,514 |
| glove-50-angular | 50 | cosine | 1,183,514 |
| glove-100-angular | 100 | cosine | 1,183,514 |
| nytimes-256-angular | 256 | cosine | 290,000 |
| deep-image-96-angular | 96 | cosine | 9,990,000 |

#### Matching the index definition to the dataset

- **Dimension.** With a `dataset:` or `file:` source, `entities.index.dimension`
  may be omitted and the index is created with the dataset's dimension. If it
  is set, it must match: a named dataset is checked at validation time, before
  any download, and a file after loading. A random source always needs an
  explicit dimension.
- **Metric.** The API accepts any metric for any data, but distances are only
  meaningful with the metric the dataset was built for: `-angular` datasets
  need `metric: cosine`, `-euclidean` ones `metric: euclidean`. For a named
  dataset a mismatch is rejected at validation time. For a `file:` source the
  tool cannot know the intended metric, so set it yourself.

### Query vectors

`query_vectors` takes a `source` option that says where the query vectors
come from:

| `source` | Dataset source | Random source |
| --- | --- | --- |
| `auto` (default) | `test` if the file has a test set, else `train` | fresh random vector |
| `test` | a held-out query vector from the file's `test` dataset | error |
| `train` | a row that was inserted into the index | fresh random vector |
| `random` | any row of the whole dataset, inserted or not | fresh random vector |

The ann-benchmarks HDF5 files carry a `test` dataset (10,000 query vectors
for SIFT1M) and a `neighbors` dataset with the true nearest train rows of
each test vector. Both are loaded; `neighbors` is kept for the correctness
mode. `.fvecs` and `.npy` files have neither, so `auto` means `train` there.

### Metadata fields

Each entry has a `name`, a `value` and an optional `probability` of being
present. Values: `{const: ...}`, `{random_string: N}`, `{range: [lo, hi]}`
(integer bounds give integers) and `{choice: [...]}`.

## Report

For every step: number of operations, errors (with a breakdown by error code),
operations per second, p50/p90/p99/max latency in milliseconds, and for the
vector steps the upload and download throughput measured from request and
response body sizes. Latency is measured around the SDK call, so it includes
signing and connection handling on the client.

## RGW notes

- With the default `rgw` backend, RGW needs a regular S3 bucket with the same
  name as the vector bucket. `create_backing_bucket: true` (the default)
  creates and deletes it around the vector bucket steps. These S3 calls are
  not measured; failures are counted under `S3CreateBucket:*` and
  `S3DeleteBucket:*`. Set it to `false` against AWS.
- The RGW extensions `filterableMetadataKeys` (CreateIndex) and
  `postFiltering` (QueryVectors) are injected into the JSON request body
  before signing, since they are not part of the SDK model.

