mod client;
mod config;
mod ext;
mod flow;
mod metadata;
mod metrics;
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

#[derive(Subcommand)]
enum Cmd {
    /// Run one job per configuration file; jobs run concurrently.
    Run {
        /// YAML configuration files.
        #[arg(required = true)]
        configs: Vec<PathBuf>,
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
        #[arg(required = true)]
        configs: Vec<PathBuf>,
    },
    /// Print an example configuration file.
    Example,
    /// List the datasets that `entities.vectors.source.dataset` accepts.
    Datasets,
    /// Delete every vector bucket (and backing bucket) whose name starts with
    /// the configured prefix, along with its indexes.
    Cleanup { config: PathBuf },
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
        Cmd::Validate { configs } => {
            for p in &configs {
                config::Config::load(p)?;
                println!("{}: ok", p.display());
            }
            Ok(())
        }
        Cmd::Cleanup { config } => {
            let cfg = config::Config::load(&config)?;
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(flow::cleanup(&cfg))
        }
        Cmd::Run {
            configs,
            report,
            seed,
            threads,
            repeat,
        } => {
            let mut jobs = Vec::new();
            for p in &configs {
                let mut cfg = config::Config::load(p)?;
                if let Some(s) = seed {
                    cfg.execution.seed = Some(s);
                }
                if let Some(t) = threads {
                    cfg.execution.threads = t;
                }
                if let Some(r) = repeat {
                    cfg.execution.repeat = r;
                }
                let name = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("job")
                    .to_string();
                jobs.push((name, cfg));
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
