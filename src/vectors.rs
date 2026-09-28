//! Vector sources: random vectors around centroids, or vectors loaded from a
//! dataset file (`.fvecs`, `.npy`, and `.hdf5` with the `hdf5` feature).

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rand::Rng;
use rand_distr::{Distribution, Normal};

use crate::config::{Metric, RandomSpec, VectorSourceSpec};
use crate::sample::SeededRng;

pub trait VectorSource: Send + Sync {
    fn dimension(&self) -> usize;
    /// Number of held-out query vectors, when the dataset has a test set.
    fn test_len(&self) -> Option<u64> {
        None
    }
    /// The `i`-th held-out query vector (wraps around `test_len`).
    fn test_vector(&self, _i: u64) -> Option<Vec<f32>> {
        None
    }
    /// Row indexes of the true nearest neighbours of test vector `i`, when
    /// the dataset ships ground truth. Reserved for the correctness mode.
    #[allow(dead_code)]
    fn neighbors(&self, _i: u64) -> Option<&[i32]> {
        None
    }
    /// The `i`-th vector. Random sources ignore `i` and draw from `rng`;
    /// dataset sources wrap around `len()`.
    fn vector(&self, i: u64, rng: &mut SeededRng) -> Vec<f32>;
    fn describe(&self) -> String;
}

pub struct RandomSource {
    dim: usize,
    range: [f64; 2],
    centroids: Vec<Vec<f32>>,
    noise: Normal<f64>,
    normalize: bool,
}

impl RandomSource {
    pub fn new(dim: usize, spec: &RandomSpec, rng: &mut SeededRng) -> Result<Self> {
        let [lo, hi] = spec.centroid_range;
        let centroids = (0..spec.centroids)
            .map(|_| (0..dim).map(|_| rng.random_range(lo..=hi) as f32).collect())
            .collect();
        let noise = Normal::new(spec.mean, spec.stddev)
            .map_err(|e| anyhow!("invalid random vector distribution: {e}"))?;
        Ok(Self {
            dim,
            range: spec.centroid_range,
            centroids,
            noise,
            normalize: spec.normalize,
        })
    }
}

impl VectorSource for RandomSource {
    fn dimension(&self) -> usize {
        self.dim
    }
    fn vector(&self, _i: u64, rng: &mut SeededRng) -> Vec<f32> {
        let c = &self.centroids[rng.random_range(0..self.centroids.len())];
        let mut v: Vec<f32> = c
            .iter()
            .map(|x| x + self.noise.sample(rng) as f32)
            .collect();
        if self.normalize {
            let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if n > 0.0 {
                v.iter_mut().for_each(|x| *x /= n);
            }
        }
        v
    }
    fn describe(&self) -> String {
        format!(
            "random: {} centroids in [{}, {}], dim {}, noise N({}, {})",
            self.centroids.len(),
            self.range[0],
            self.range[1],
            self.dim,
            self.noise.mean(),
            self.noise.std_dev()
        )
    }
}

pub struct DatasetSource {
    dim: usize,
    data: Vec<f32>, // row-major, len = rows * dim
    name: String,
    /// Held-out query vectors, row-major with the same dimension.
    test: Vec<f32>,
    /// Ground truth: `neighbors_k` train row indexes per test vector.
    neighbors: Vec<i32>,
    neighbors_k: usize,
}

/// The arrays a dataset file may hold. Only `train` is mandatory.
#[derive(Default)]
struct Loaded {
    dim: usize,
    train: Vec<f32>,
    test: Vec<f32>,
    neighbors: Vec<i32>,
    neighbors_k: usize,
}

impl VectorSource for DatasetSource {
    fn dimension(&self) -> usize {
        self.dim
    }
    fn test_len(&self) -> Option<u64> {
        (!self.test.is_empty()).then(|| (self.test.len() / self.dim) as u64)
    }
    fn test_vector(&self, i: u64) -> Option<Vec<f32>> {
        let rows = self.test_len()?;
        let r = (i % rows) as usize;
        Some(self.test[r * self.dim..(r + 1) * self.dim].to_vec())
    }
    fn neighbors(&self, i: u64) -> Option<&[i32]> {
        let rows = self.test_len()?;
        if self.neighbors_k == 0 || self.neighbors.is_empty() {
            return None;
        }
        let r = (i % rows) as usize;
        Some(&self.neighbors[r * self.neighbors_k..(r + 1) * self.neighbors_k])
    }
    fn vector(&self, i: u64, _rng: &mut SeededRng) -> Vec<f32> {
        let rows = self.data.len() / self.dim;
        let r = (i % rows as u64) as usize;
        self.data[r * self.dim..(r + 1) * self.dim].to_vec()
    }
    fn describe(&self) -> String {
        let mut d = format!(
            "dataset {}: {} vectors, dim {}",
            self.name,
            self.data.len() / self.dim,
            self.dim
        );
        if let Some(t) = self.test_len() {
            d.push_str(&format!(", {t} test vectors"));
            if self.neighbors_k > 0 {
                d.push_str(&format!(
                    " with {} ground-truth neighbours each",
                    self.neighbors_k
                ));
            }
        }
        d
    }
}

/// A downloadable dataset.
pub struct Dataset {
    pub name: &'static str,
    pub url: &'static str,
    pub dimension: usize,
    /// The metric the dataset was built for. `-angular` datasets are meant
    /// for cosine indexes, `-euclidean` ones for euclidean indexes.
    pub metric: Metric,
    /// Number of vectors in the `train` dataset.
    pub train_vectors: u64,
}

/// Known downloadable datasets. All are ann-benchmarks HDF5 files with a
/// `train` dataset of float32 rows (requires the `hdf5` feature).
pub const DATASETS: &[Dataset] = &[
    Dataset {
        name: "fashion-mnist-784-euclidean",
        url: "http://ann-benchmarks.com/fashion-mnist-784-euclidean.hdf5",
        dimension: 784,
        metric: Metric::Euclidean,
        train_vectors: 60_000,
    },
    Dataset {
        name: "sift-128-euclidean",
        url: "http://ann-benchmarks.com/sift-128-euclidean.hdf5",
        dimension: 128,
        metric: Metric::Euclidean,
        train_vectors: 1_000_000,
    },
    Dataset {
        name: "gist-960-euclidean",
        url: "http://ann-benchmarks.com/gist-960-euclidean.hdf5",
        dimension: 960,
        metric: Metric::Euclidean,
        train_vectors: 1_000_000,
    },
    Dataset {
        name: "glove-25-angular",
        url: "http://ann-benchmarks.com/glove-25-angular.hdf5",
        dimension: 25,
        metric: Metric::Cosine,
        train_vectors: 1_183_514,
    },
    Dataset {
        name: "glove-50-angular",
        url: "http://ann-benchmarks.com/glove-50-angular.hdf5",
        dimension: 50,
        metric: Metric::Cosine,
        train_vectors: 1_183_514,
    },
    Dataset {
        name: "glove-100-angular",
        url: "http://ann-benchmarks.com/glove-100-angular.hdf5",
        dimension: 100,
        metric: Metric::Cosine,
        train_vectors: 1_183_514,
    },
    Dataset {
        name: "nytimes-256-angular",
        url: "http://ann-benchmarks.com/nytimes-256-angular.hdf5",
        dimension: 256,
        metric: Metric::Cosine,
        train_vectors: 290_000,
    },
    Dataset {
        name: "deep-image-96-angular",
        url: "http://ann-benchmarks.com/deep-image-96-angular.hdf5",
        dimension: 96,
        metric: Metric::Cosine,
        train_vectors: 9_990_000,
    },
];

pub fn find_dataset(name: &str) -> Option<&'static Dataset> {
    DATASETS.iter().find(|d| d.name == name)
}

pub fn dataset_names() -> String {
    DATASETS
        .iter()
        .map(|d| d.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Build the source described by the config. `dim` is the configured index
/// dimension; when it is None the dimension comes from the dataset, and a
/// random source cannot be built.
pub async fn build(
    spec: &VectorSourceSpec,
    dim: Option<usize>,
    datasets_dir: &Path,
    rng: &mut SeededRng,
) -> Result<Box<dyn VectorSource>> {
    match spec {
        VectorSourceSpec::Random(r) => {
            let dim = dim.ok_or_else(|| {
                anyhow!("entities.index.dimension is required with a random vector source")
            })?;
            Ok(Box::new(RandomSource::new(dim, r, rng)?))
        }
        VectorSourceSpec::File(path) => {
            let ds = load_file(path, path.display().to_string())?;
            check_dim(&ds, dim)?;
            Ok(Box::new(ds))
        }
        VectorSourceSpec::Dataset(name) => {
            let d = find_dataset(name)
                .ok_or_else(|| anyhow!("unknown dataset '{name}'; known: {}", dataset_names()))?;
            if let Some(dim) = dim {
                if d.dimension != dim {
                    bail!(
                        "dataset '{name}' has dimension {}, index dimension is {dim}",
                        d.dimension
                    );
                }
            }
            let path = download(d.url, datasets_dir).await?;
            let ds = load_file(&path, name.clone())?;
            check_dim(&ds, dim)?;
            Ok(Box::new(ds))
        }
    }
}

fn check_dim(ds: &DatasetSource, dim: Option<usize>) -> Result<()> {
    let Some(dim) = dim else { return Ok(()) };
    if ds.dim != dim {
        bail!(
            "dataset {} has dimension {}, index dimension is {}",
            ds.name,
            ds.dim,
            dim
        );
    }
    Ok(())
}

async fn download(url: &str, dir: &Path) -> Result<PathBuf> {
    let file = url.rsplit('/').next().unwrap_or("dataset");
    let path = dir.join(file);
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    tracing::info!("downloading {url} to {}", path.display());
    let resp = reqwest::get(url).await?.error_for_status()?;
    let bytes = resp.bytes().await?;
    let tmp = path.with_extension("part");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

fn load_file(path: &Path, name: String) -> Result<DatasetSource> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let l = match ext.as_str() {
        "fvecs" => {
            let (dim, train) = load_fvecs(path)?;
            Loaded {
                dim,
                train,
                ..Default::default()
            }
        }
        "npy" => {
            let (dim, train) = load_npy(path)?;
            Loaded {
                dim,
                train,
                ..Default::default()
            }
        }
        "hdf5" | "h5" => load_hdf5(path)?,
        _ => bail!("unsupported dataset file type '{ext}' ({})", path.display()),
    };
    if l.dim == 0 || l.train.is_empty() {
        bail!("dataset {} is empty", path.display());
    }
    if !l.test.is_empty() && l.test.len() % l.dim != 0 {
        bail!(
            "dataset {}: test set dimension differs from train",
            path.display()
        );
    }
    Ok(DatasetSource {
        dim: l.dim,
        data: l.train,
        name,
        test: l.test,
        neighbors: l.neighbors,
        neighbors_k: l.neighbors_k,
    })
}

fn load_fvecs(path: &Path) -> Result<(usize, Vec<f32>)> {
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let mut data = Vec::new();
    let mut dim = 0usize;
    let mut off = 0usize;
    while off + 4 <= buf.len() {
        let d = i32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        if dim == 0 {
            dim = d;
        } else if d != dim {
            bail!(
                "inconsistent dimension in {}: {} vs {}",
                path.display(),
                d,
                dim
            );
        }
        if off + 4 * d > buf.len() {
            bail!("truncated fvecs file {}", path.display());
        }
        data.extend(
            buf[off..off + 4 * d]
                .chunks_exact(4)
                .map(|c| f32::from_le_bytes(c.try_into().unwrap())),
        );
        off += 4 * d;
    }
    Ok((dim, data))
}

fn load_npy(path: &Path) -> Result<(usize, Vec<f32>)> {
    use ndarray::Array2;
    use ndarray_npy::read_npy;
    let arr: Array2<f32> = match read_npy(path) {
        Ok(a) => a,
        Err(_) => {
            let a64: Array2<f64> =
                read_npy(path).with_context(|| format!("reading {}", path.display()))?;
            a64.mapv(|x| x as f32)
        }
    };
    let dim = arr.ncols();
    let data = arr.into_iter().collect();
    Ok((dim, data))
}

/// ann-benchmarks layout: `train` (mandatory), `test` (query vectors) and
/// `neighbors` (train row indexes of the true nearest neighbours of each
/// test vector), all 2-D.
#[cfg(feature = "hdf5")]
fn load_hdf5(path: &Path) -> Result<Loaded> {
    let file = hdf5::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let ds = file
        .dataset("train")
        .context("hdf5 file has no 'train' dataset")?;
    let arr: ndarray::Array2<f32> = ds.read_2d().context("reading 'train' as 2-D float32")?;
    let mut l = Loaded {
        dim: arr.ncols(),
        train: arr.into_iter().collect(),
        ..Default::default()
    };
    if let Ok(ds) = file.dataset("test") {
        let t: ndarray::Array2<f32> = ds.read_2d().context("reading 'test' as 2-D float32")?;
        if t.ncols() != l.dim {
            bail!(
                "{}: 'test' has dimension {}, 'train' has {}",
                path.display(),
                t.ncols(),
                l.dim
            );
        }
        l.test = t.into_iter().collect();
        if let Ok(ds) = file.dataset("neighbors") {
            let n: ndarray::Array2<i32> =
                ds.read_2d().context("reading 'neighbors' as 2-D int32")?;
            l.neighbors_k = n.ncols();
            l.neighbors = n.into_iter().collect();
        }
    }
    Ok(l)
}

#[cfg(not(feature = "hdf5"))]
fn load_hdf5(path: &Path) -> Result<Loaded> {
    bail!(
        "{} is an HDF5 file; rebuild with `--features hdf5` (needs libhdf5) or convert it to .npy/.fvecs",
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::SeedableRng;

    #[test]
    fn random_vectors_have_dimension_and_are_normalized() {
        let mut rng = SeededRng::seed_from_u64(1);
        let spec = RandomSpec {
            centroids: 3,
            centroid_range: [-1.0, 1.0],
            mean: 0.0,
            stddev: 0.1,
            normalize: true,
        };
        let src = RandomSource::new(16, &spec, &mut rng).unwrap();
        let v = src.vector(0, &mut rng);
        assert_eq!(v.len(), 16);
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((n - 1.0).abs() < 1e-4);
    }

    #[test]
    fn centroids_respect_range() {
        let mut rng = SeededRng::seed_from_u64(3);
        let spec = RandomSpec {
            centroids: 4,
            centroid_range: [10.0, 20.0],
            mean: 0.0,
            stddev: 0.0,
            normalize: false,
        };
        let src = RandomSource::new(8, &spec, &mut rng).unwrap();
        for i in 0..50 {
            let v = src.vector(i, &mut rng);
            assert!(v.iter().all(|x| (10.0..=20.0).contains(x)), "{v:?}");
        }
    }

    #[test]
    fn dataset_test_set_and_neighbors() {
        let ds = DatasetSource {
            dim: 2,
            data: vec![0.0, 0.0, 1.0, 1.0, 2.0, 2.0],
            name: "t".into(),
            test: vec![9.0, 9.0, 8.0, 8.0],
            neighbors: vec![2, 1, 1, 0],
            neighbors_k: 2,
        };
        assert_eq!(ds.test_len(), Some(2));
        assert_eq!(ds.test_vector(1), Some(vec![8.0, 8.0]));
        assert_eq!(ds.test_vector(2), Some(vec![9.0, 9.0])); // wraps around
        assert_eq!(ds.neighbors(0), Some(&[2, 1][..]));
        let mut rng = SeededRng::seed_from_u64(1);
        assert_eq!(ds.vector(4, &mut rng), vec![1.0, 1.0]); // 4 % 3 rows
        let plain = DatasetSource {
            test: vec![],
            neighbors: vec![],
            neighbors_k: 0,
            ..ds
        };
        assert_eq!(plain.test_len(), None);
        assert_eq!(plain.test_vector(0), None);
        assert_eq!(plain.neighbors(0), None);
    }

    #[test]
    fn fvecs_roundtrip() {
        let dir = std::env::temp_dir().join(format!("s3vtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.fvecs");
        let mut bytes = Vec::new();
        for row in [[1.0f32, 2.0], [3.0, 4.0]] {
            bytes.extend(2i32.to_le_bytes());
            for x in row {
                bytes.extend(x.to_le_bytes());
            }
        }
        std::fs::write(&p, bytes).unwrap();
        let (dim, data) = load_fvecs(&p).unwrap();
        assert_eq!(dim, 2);
        assert_eq!(data, vec![1.0, 2.0, 3.0, 4.0]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
