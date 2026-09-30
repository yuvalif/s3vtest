# s3vtest

Command-line tool to test S3 Vectors functionality, performance and
reliability against Ceph RGW (or AWS). Written in Rust on top of the official
`aws-sdk-s3vectors` crate.

## Run from a container

No Rust toolchain and no clone of this repository are needed. The image
contains the tool built with HDF5 support, and the example configurations
under `/usr/share/s3vtest/examples`.

```sh
podman pull ghcr.io/yuvalif/s3vtest:latest
```

### Quick start: a bundled example

The connection settings are passed in the environment, using the same
variable names as the AWS CLI, so nothing has to be mounted:

```sh
export AWS_ENDPOINT_URL=http://localhost:8000
export AWS_ACCESS_KEY_ID=...
export AWS_SECRET_ACCESS_KEY=...

podman run --rm --network host \
  -e AWS_ENDPOINT_URL -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \
  ghcr.io/yuvalif/s3vtest run /usr/share/s3vtest/examples/rgw-vstart.yaml
```

### Your own configuration, from standard input

`-` reads the configuration from standard input. Note the `-i`:

```sh
podman run --rm ghcr.io/yuvalif/s3vtest example > job.yaml    # then edit job.yaml
podman run --rm -i --network host \
  -e AWS_ENDPOINT_URL -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \
  ghcr.io/yuvalif/s3vtest run - < job.yaml
```

The job is named `stdin` in the report. Only one configuration can come from
standard input per invocation.

### Datasets: keep the download in a named volume

A downloaded dataset is lost when the container exits unless its directory
outlives it. A named volume avoids downloading it again on every run:

```sh
podman run --rm --network host \
  -e AWS_ENDPOINT_URL -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \
  -v s3vtest-datasets:/work/datasets \
  ghcr.io/yuvalif/s3vtest run /usr/share/s3vtest/examples/sift-128-euclidean.yaml
```

This works because the examples use `datasets_dir: datasets`, which is
relative to the working directory of the container, `/work`.

### Optional: mount a directory

Useful when you keep several job files, run more than one job at a time, or
want to use dataset files that are already on the host:

```sh
podman run --rm --network host -v "$PWD":/work:Z \
  -e AWS_ENDPOINT_URL -e AWS_ACCESS_KEY_ID -e AWS_SECRET_ACCESS_KEY \
  ghcr.io/yuvalif/s3vtest run a.yaml b.yaml --report json
```

`:Z` relabels the directory for SELinux.

### Notes

- `--network host` lets the tool reach an RGW that listens on the host, such
  as `http://localhost:8000`. It is not needed for a remote endpoint.
- Reports go to stdout and logs to stderr, so `> report.json 2> run.log`
  works as usual. The exit status of the tool is the exit status of
  `podman run`.
- To build the image locally: `podman build -t s3vtest .`

## Connection settings

The `connection` section of the configuration can be overridden, or left out
entirely, by command line flags or environment variables. The order of
precedence is flag, then environment, then file.

| Setting | Flag | Environment variable |
| --- | --- | --- |
| `connection.endpoint` | `--endpoint` | `AWS_ENDPOINT_URL` |
| `connection.access_key` | `--access-key` | `AWS_ACCESS_KEY_ID` |
| `connection.secret_key` | `--secret-key` | `AWS_SECRET_ACCESS_KEY` |
| `connection.region` | `--region` | `AWS_REGION`, then `AWS_DEFAULT_REGION` |

These are the variables the AWS CLI and SDKs use, so a shell that is already
set up for `aws --endpoint-url ...` against the same RGW works unchanged.
Only these are read: profiles, `~/.aws/credentials`, `AWS_SESSION_TOKEN` and
the service-specific `AWS_ENDPOINT_URL_<SERVICE>` variables are not.

They apply to `run`, `validate` and `cleanup`, and to every job of the
invocation.

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
s3vtest run - < job.yaml          # the same, reading the configuration from stdin
s3vtest run a.yaml b.yaml --report json   # several jobs concurrently, JSON report
s3vtest recall job.yaml           # measure query correctness against an exact local search
s3vtest cleanup job.yaml          # delete leftovers under the job's bucket prefix
RUST_LOG=s3vtest=debug s3vtest run job.yaml   # more logging (stderr)
```

Exit status is 2 when any step recorded errors.

## Configuration

One YAML file per job. See `s3vtest example` for every field with comments.
Ready-made jobs are in `examples/`:

- `rgw-vstart.yaml`: random vectors against a vstart RGW, every step.
- `sift-128-euclidean.yaml`: 100,000 SIFT1M vectors with metadata, filtered
  and unfiltered queries, then cleanup (needs the `hdf5` feature).
- `fashion-mnist-784-euclidean-full.yaml`: the whole Fashion-MNIST dataset,
  60,000 vectors, loaded and left in place for the `recall` command; the
  quickest full dataset to load and check (needs the `hdf5` feature).

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

## Query correctness: the `recall` command

`s3vtest recall job.yaml` measures how correct the server's query results
are. For every index of the job it:

1. lists the vectors the index holds, with their data;
2. runs `--queries` queries (default 1000) with `--top-k` results each;
3. computes the exact `k` nearest neighbours of every query locally, in
   parallel, over the listed vectors;
4. reports the recall: the fraction of returned keys that are true
   neighbours, averaged over the queries.

Because the ground truth comes from what the index actually holds, it is
right for any vector source, for a subset of a dataset, and after updates and
deletes. Run it after a flow that inserted vectors and before one that deletes
them, for example:

```sh
s3vtest run job.yaml            # flow: create_vector_bucket, create_index, insert_vectors
s3vtest recall job.yaml --queries 1000 --top-k 10 --min-recall 0.95
s3vtest cleanup job.yaml
```

`examples/fashion-mnist-784-euclidean-full.yaml` is such a job: it loads the
complete 60,000-vector Fashion-MNIST dataset into one index and has no
update or delete steps.

The same job file and seed are used, so the command finds the same buckets
and indexes the flow created.

| Column | Meaning |
| --- | --- |
| recall mean, min, p10 | over the queries; p10 is the 10th percentile |
| perfect % | queries whose recall is exactly 1 |
| truth | `listed`: exact search over the listed vectors; `file`: the dataset's ground truth (`--no-list`) |
| unknown keys | returned keys the listing (or, with `--no-list`, the dataset) does not contain, always a bug |
| query errors | failed queries, excluded from the recall figures |
| dist max rel err | largest relative difference between a returned distance and the recomputed one |

Options: `--source` picks the query vectors as for the `query_vectors` step
(`auto`, `test`, `train`, `random`); `--epsilon` (default 0.001) counts a
result as correct when its true distance is within that relative tolerance of
the k-th true distance, so ties do not count as misses; `--min-recall` makes
the command exit with status 2 when the mean recall of any index is below it;
`--report json` prints the figures as JSON. Unknown keys and query errors
always give exit status 2.

### `--no-list`: use the ground truth shipped with the dataset

The ann-benchmarks files carry the true 100 nearest train rows of every test
vector. With `--no-list` the command skips the listing and scores the
returned keys against that, mapping each key back to its dataset row through
`key_prefix`. This saves the listing and the exact search, which are most of
the run time on a large index, but it is only right when **each index holds
the complete dataset and nothing else**: no partial load, no extra vectors,
no deletes, no updates. The command checks what it can (a dataset with
ground truth, `--source test`, `vectors_per_index` equal to the dataset size,
`--top-k` at most 100) and trusts you for the rest. Returned distances are
still checked against the train vectors held in memory.

```sh
s3vtest run    examples/fashion-mnist-784-euclidean-full.yaml
s3vtest recall examples/fashion-mnist-784-euclidean-full.yaml --no-list --queries 5000 --top-k 100
```

The `truth` column of the report says `file` or `listed`.

How to read the numbers:

- A recall below 1 is not automatically a bug. An approximate index trades
  some recall for speed, and the RGW may build such an index in the
  background, so the figure can change over time for the same data. Use
  `--min-recall` as a quality threshold.
- For a euclidean index the command also compares the returned distances
  with squared euclidean distances and says so when those match instead.
- Listing a large index takes a while (one round trip per page of
  `--page-size` vectors, 500 by default), and the exact search costs about a
  minute per 1000 queries over a million 128-dimensional vectors.

## RGW notes

- With the default `rgw` backend, RGW needs a regular S3 bucket with the same
  name as the vector bucket. `create_backing_bucket: true` (the default)
  creates and deletes it around the vector bucket steps. These S3 calls are
  not measured; failures are counted under `S3CreateBucket:*` and
  `S3DeleteBucket:*`. Set it to `false` against AWS.
- The RGW extensions `filterableMetadataKeys` (CreateIndex) and
  `postFiltering` (QueryVectors) are injected into the JSON request body
  before signing, since they are not part of the SDK model.

