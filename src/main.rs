mod client;
mod config;
mod ext;
mod flow;
mod metadata;
mod metrics;
mod recall;
mod report;
mod sample;
mod vectors;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "s3vtest",
    version,
    about = "S3 Vectors functional, performance and reliability test tool"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Connection settings that override the configuration file.
#[derive(clap::Args, Clone, Debug)]
struct ConnArgs {
    /// S3 endpoint, overrides `connection.endpoint`.
    #[arg(long, env = "AWS_ENDPOINT_URL")]
    endpoint: Option<String>,
    /// Access key, overrides `connection.access_key`.
    #[arg(long, env = "AWS_ACCESS_KEY_ID", hide_env_values = true)]
    access_key: Option<String>,
    /// Secret key, overrides `connection.secret_key`. Prefer the environment
    /// variable, which does not show up in the process list.
    #[arg(long, env = "AWS_SECRET_ACCESS_KEY", hide_env_values = true)]
    secret_key: Option<String>,
    /// Region, overrides `connection.region`. AWS_DEFAULT_REGION is used
    /// when AWS_REGION is not set.
    #[arg(long, env = "AWS_REGION")]
    region: Option<String>,
}

impl ConnArgs {
    fn overrides(&self) -> config::ConnectionOverrides {
        let non_empty = |v: &Option<String>| v.clone().filter(|s| !s.is_empty());
        config::ConnectionOverrides {
            endpoint: non_empty(&self.endpoint),
            access_key: non_empty(&self.access_key),
            secret_key: non_empty(&self.secret_key),
            region: non_empty(&self.region)
                .or_else(|| non_empty(&std::env::var("AWS_DEFAULT_REGION").ok())),
        }
    }
}

/// A job is named after its file; standard input is named "stdin".
fn job_name(p: &std::path::Path) -> String {
    if p == std::path::Path::new("-") {
        return "stdin".to_string();
    }
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("job")
        .to_string()
}

fn check_single_stdin(configs: &[PathBuf]) -> Result<()> {
    if configs.iter().filter(|p| p.as_os_str() == "-").count() > 1 {
        anyhow::bail!("standard input ('-') can be given only once");
    }
    Ok(())
}

#[derive(Subcommand)]
enum Cmd {
    /// Run one job per configuration file; jobs run concurrently.
    Run {
        /// YAML configuration files. `-` reads one from standard input.
        #[arg(required = true)]
        configs: Vec<PathBuf>,
        #[command(flatten)]
        conn: ConnArgs,
        /// Report format.
        #[arg(long, value_enum, default_value_t = ReportFormat::Table)]
        report: ReportFormat,
        /// Override `execution.seed` for every job.
        #[arg(long)]
        seed: Option<u64>,
        /// Override `execution.threads` for every job.
        #[arg(long)]
        threads: Option<usize>,
        /// Override `execution.repeat` for every job.
        #[arg(long)]
        repeat: Option<u32>,
    },
    /// Parse and validate configuration files without running them.
    Validate {
        /// YAML configuration files. `-` reads one from standard input.
        #[arg(required = true)]
        configs: Vec<PathBuf>,
        #[command(flatten)]
        conn: ConnArgs,
    },
    /// Print an example configuration file.
    Example,
    /// List the datasets that `entities.vectors.source.dataset` accepts.
    Datasets,
    /// Measure query correctness: list the vectors each index holds, run
    /// queries, and compare the results with an exact nearest-neighbour
    /// search computed locally. Run it after a flow that inserted vectors
    /// and before one that deletes them.
    Recall {
        /// YAML configuration file. `-` reads it from standard input.
        config: PathBuf,
        #[command(flatten)]
        conn: ConnArgs,
        /// Queries per index.
        #[arg(long, default_value_t = 1000)]
        queries: u64,
        /// Results per query (topK).
        #[arg(long, default_value_t = 10)]
        top_k: u32,
        /// Where the query vectors come from: auto, test, train or random.
        #[arg(long, value_enum, default_value_t = SourceArg::Auto)]
        source: SourceArg,
        /// Relative tolerance on the k-th distance for counting ties as correct.
        #[arg(long, default_value_t = 1e-3)]
        epsilon: f64,
        /// Fail (exit 2) when the mean recall of any index is below this.
        #[arg(long)]
        min_recall: Option<f64>,
        /// Concurrent queries. Defaults to `execution.threads`.
        #[arg(long)]
        threads: Option<usize>,
        /// ListVectors page size used to read the indexes back.
        #[arg(long, default_value_t = 500)]
        page_size: i32,
        /// Do not list the indexes; use the ground truth shipped with the
        /// dataset instead. You guarantee that each index holds the complete
        /// dataset and nothing else. Needs a dataset with ground truth and
        /// the `test` query source; top-k is limited to the neighbours the
        /// file provides (100 for the ann-benchmarks datasets).
        #[arg(long)]
        no_list: bool,
        #[arg(long, value_enum, default_value_t = ReportFormat::Table)]
        report: ReportFormat,
    },
    /// Delete every vector bucket (and backing bucket) whose name starts with
    /// the configured prefix, along with its indexes.
    Cleanup {
        /// YAML configuration file. `-` reads it from standard input.
        config: PathBuf,
        #[command(flatten)]
        conn: ConnArgs,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum SourceArg {
    Auto,
    Test,
    Train,
    Random,
}

impl From<SourceArg> for config::QuerySource {
    fn from(s: SourceArg) -> Self {
        match s {
            SourceArg::Auto => config::QuerySource::Auto,
            SourceArg::Test => config::QuerySource::Test,
            SourceArg::Train => config::QuerySource::Train,
            SourceArg::Random => config::QuerySource::Random,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ReportFormat {
    Table,
    Json,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "s3vtest=info,warn".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Example => {
            print!("{}", config::EXAMPLE_YAML);
            Ok(())
        }
        Cmd::Datasets => {
            println!(
                "{:<30} {:>9} {:<9} {:>12}  url",
                "name", "dimension", "metric", "vectors"
            );
            for d in vectors::DATASETS {
                println!(
                    "{:<30} {:>9} {:<9} {:>12}  {}",
                    d.name,
                    d.dimension,
                    format!("{:?}", d.metric).to_lowercase(),
                    d.train_vectors,
                    d.url
                );
            }
            println!("\nAll are HDF5 files and need a build with `--features hdf5`.");
            Ok(())
        }
        Cmd::Validate { configs, conn } => {
            check_single_stdin(&configs)?;
            for p in &configs {
                config::Config::load(p, &conn.overrides())?;
                println!("{}: ok", p.display());
            }
            Ok(())
        }
        Cmd::Recall {
            config,
            conn,
            queries,
            top_k,
            source,
            epsilon,
            min_recall,
            threads,
            page_size,
            no_list,
            report,
        } => {
            let cfg = config::Config::load(&config, &conn.overrides())?;
            if top_k == 0 || top_k > config::limits::MAX_TOP_K {
                anyhow::bail!(
                    "--top-k must be between 1 and {}",
                    config::limits::MAX_TOP_K
                );
            }
            if !(1..=config::limits::MAX_LIST_PAGE as i32).contains(&page_size) {
                anyhow::bail!(
                    "--page-size must be between 1 and {}",
                    config::limits::MAX_LIST_PAGE
                );
            }
            let opts = recall::RecallOpts {
                queries,
                top_k,
                source: source.into(),
                epsilon,
                threads: threads.unwrap_or(cfg.execution.threads),
                page_size,
                no_list,
            };
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(opts.threads.clamp(1, 256))
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            let results = rt.block_on(recall::recall(&cfg, &opts))?;
            match report {
                ReportFormat::Table => print!("{}", recall::table(&results)),
                ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&results)?),
            }
            let failed = results.iter().any(|r| {
                r.query_errors > 0
                    || r.unknown_keys > 0
                    || min_recall.is_some_and(|m| r.recall_mean < m)
            });
            if failed {
                std::process::exit(2);
            }
            Ok(())
        }
        Cmd::Cleanup { config, conn } => {
            let cfg = config::Config::load(&config, &conn.overrides())?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(flow::cleanup(&cfg))
        }
        Cmd::Run {
            configs,
            conn,
            report,
            seed,
            threads,
            repeat,
        } => {
            check_single_stdin(&configs)?;
            let mut jobs = Vec::new();
            for p in &configs {
                let mut cfg = config::Config::load(p, &conn.overrides())?;
                if let Some(s) = seed {
                    cfg.execution.seed = Some(s);
                }
                if let Some(t) = threads {
                    cfg.execution.threads = t;
                }
                if let Some(r) = repeat {
                    cfg.execution.repeat = r;
                }
                jobs.push((job_name(p), cfg));
            }
            let total_threads: usize = jobs.iter().map(|(_, c)| c.execution.threads).sum();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(total_threads.clamp(1, 256))
                .enable_all()
                .build()
                .context("building tokio runtime")?;
            let reports = rt.block_on(async {
                let handles: Vec<_> = jobs
                    .into_iter()
                    .map(|(name, cfg)| tokio::spawn(async move { flow::run_job(name, cfg).await }))
                    .collect();
                let mut out = Vec::new();
                for h in handles {
                    out.push(h.await.context("job task panicked")??);
                }
                Ok::<_, anyhow::Error>(out)
            })?;

            match report {
                ReportFormat::Table => {
                    for r in &reports {
                        println!("{}", r.to_table());
                    }
                }
                ReportFormat::Json => println!("{}", serde_json::to_string_pretty(&reports)?),
            }
            let errors: u64 = reports.iter().map(|r| r.total_errors()).sum();
            if errors > 0 {
                std::process::exit(2);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_falls_back_to_aws_default_region() {
        let args = |region: Option<&str>| ConnArgs {
            endpoint: None,
            access_key: None,
            secret_key: None,
            region: region.map(str::to_string),
        };
        std::env::set_var("AWS_DEFAULT_REGION", "from-default");
        assert_eq!(
            args(None).overrides().region.as_deref(),
            Some("from-default")
        );
        // the flag, or AWS_REGION which clap maps onto it, wins
        assert_eq!(
            args(Some("explicit")).overrides().region.as_deref(),
            Some("explicit")
        );
        std::env::remove_var("AWS_DEFAULT_REGION");
        assert_eq!(args(None).overrides().region, None);
        assert_eq!(job_name(std::path::Path::new("-")), "stdin");
        assert_eq!(job_name(std::path::Path::new("/a/b/job1.yaml")), "job1");
    }
}
