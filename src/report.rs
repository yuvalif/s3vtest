//! Table and JSON reports.

use comfy_table::{presets::UTF8_FULL_CONDENSED, Cell, CellAlignment, Table};
use serde::Serialize;

use crate::metrics::StepSummary;

#[derive(Debug, Clone, Serialize)]
pub struct JobReport {
    pub job: String,
    pub seed: u64,
    pub threads: usize,
    pub repeat: u32,
    /// Set when the job stopped early, with the reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aborted: Option<String>,
    pub steps: Vec<StepSummary>,
}

impl JobReport {
    pub fn total_errors(&self) -> u64 {
        self.steps.iter().map(|s| s.errors).sum::<u64>() + u64::from(self.aborted.is_some())
    }

    pub fn to_table(&self) -> String {
        let mut t = Table::new();
        t.load_style(UTF8_FULL_CONDENSED);
        t.set_header(vec![
            "step",
            "ops",
            "errors",
            "ops/s",
            "p50 ms",
            "p90 ms",
            "p99 ms",
            "max ms",
            "up MB/s",
            "down MB/s",
        ]);
        for s in &self.steps {
            let mb = |b: Option<f64>| match b {
                Some(v) => format!("{:.2}", v / 1_000_000.0),
                None => "-".to_string(),
            };
            let row = vec![
                Cell::new(&s.step),
                Cell::new(s.ops),
                Cell::new(s.errors),
                Cell::new(format!("{:.1}", s.ops_per_sec)),
                Cell::new(format!("{:.2}", s.p50_ms)),
                Cell::new(format!("{:.2}", s.p90_ms)),
                Cell::new(format!("{:.2}", s.p99_ms)),
                Cell::new(format!("{:.2}", s.max_ms)),
                Cell::new(mb(s.upload_bytes_per_sec)),
                Cell::new(mb(s.download_bytes_per_sec)),
            ];
            t.add_row(row.into_iter().enumerate().map(|(i, c)| {
                if i == 0 {
                    c
                } else {
                    c.set_alignment(CellAlignment::Right)
                }
            }));
        }
        let mut out = format!(
            "job: {}  (seed {}, threads {}, repeat {})\n{}\n",
            self.job, self.seed, self.threads, self.repeat, t
        );
        if let Some(a) = &self.aborted {
            out.push_str(&format!("  ABORTED: {a}\n"));
        }
        for s in &self.steps {
            if !s.error_codes.is_empty() {
                let codes: Vec<String> = s
                    .error_codes
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect();
                out.push_str(&format!("  {} errors: {}\n", s.step, codes.join(", ")));
            }
        }
        out
    }
}
