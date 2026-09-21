//! Report generation — JSON artifact, human-readable RESULTS.md, and an SVG
//! chart drawing the Nostos throughput curve against PowerSync's published
//! ceiling.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::{BenchConfig, RunResult};

/// Format a float with thousands separators (e.g. 12345.6 → "12,346").
fn grouped(v: f64) -> String {
    let n = v.round() as u64;
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let len = bytes.len();
    for (i, b) in bytes.iter().enumerate() {
        let from_end = len - i;
        if i != 0 && from_end % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Recorded environment for the report (reproducibility).
#[derive(Debug, Clone, Serialize)]
pub struct Environment {
    pub rustc: String,
    pub profile: String,
    pub buffer: usize,
    pub events: u64,
    pub hostname: String,
    pub cpu_cores: usize,
}

impl Environment {
    pub fn collect(cfg: &BenchConfig) -> Self {
        Self {
            rustc: rustc_version(),
            profile: cfg.profile.clone(),
            buffer: cfg.buffer,
            events: cfg.events,
            hostname: hostname(),
            cpu_cores: std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get),
        }
    }
}

/// The sentinel returned when a real value can't be captured (binary missing,
/// non-zero exit, non-UTF-8 output). Kept as a constant so callers and tests
/// agree on the string.
pub const UNKNOWN: &str = "unknown";

/// Capture `rustc --version` once at report time. Falls back to [`UNKNOWN`] on
/// any error (missing binary, non-zero exit, non-UTF-8).
///
/// Factored out so a unit test can exercise it directly.
pub fn rustc_version() -> String {
    run_capture("rustc", &["--version"])
}

/// Capture the machine hostname once at report time via `hostname`. Falls back
/// to [`UNKNOWN`] on any error. The legacy `NOSTOS_BENCH_HOST` env override is
/// still honored (useful for reproducible local runs).
pub fn hostname() -> String {
    if let Ok(h) = std::env::var("NOSTOS_BENCH_HOST") {
        return h;
    }
    run_capture("hostname", &[])
}

/// Shell out once, returning trimmed UTF-8 stdout or [`UNKNOWN`] on any error.
fn run_capture(cmd: &str, args: &[&str]) -> String {
    match std::process::Command::new(cmd).args(args).output() {
        Ok(out) if out.status.success() => match String::from_utf8(out.stdout) {
            Ok(s) => s.trim().to_string(),
            Err(_) => UNKNOWN.to_string(),
        },
        _ => UNKNOWN.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On the build machine `rustc` is installed, so the helper must return the
    /// real toolchain line — which always starts with `rustc 1.`. We allow
    /// `unknown` too so the test is robust to sandboxed envs where `rustc` is
    /// not on PATH, but in practice the build machine has it.
    #[test]
    fn rustc_version_is_real_on_build_machine() {
        let v = rustc_version();
        assert!(
            v.starts_with("rustc 1.") || v == UNKNOWN,
            "rustc_version() returned an unexpected value: {v:?}"
        );
    }

    fn run(clients: usize, ops: f64, drop_rate: f64, throughput_valid: bool) -> RunResult {
        RunResult {
            clients,
            events_total: 100_000,
            events_delivered: 1,
            events_superseded: 0,
            ops_per_sec: ops,
            drop_rate,
            p50_us: 10.0,
            p99_us: 86.0,
            elapsed_secs: 120.0,
            throughput_valid,
            profile: "small".into(),
        }
    }

    fn report_of(runs: Vec<RunResult>) -> String {
        render_markdown(&FullReport {
            environment: Environment {
                rustc: UNKNOWN.into(),
                profile: "small".into(),
                buffer: 1024,
                events: 100_000,
                hostname: "test".into(),
                cpu_cores: 8,
            },
            runs,
            powersync_ceiling_ops_per_sec_low: 2_000,
            powersync_ceiling_ops_per_sec_high: 4_000,
        })
    }

    /// A run that hit `--timeout-secs` delivered only part of the workload, so
    /// its ops/sec is a floor and its drop% counts in-flight events as lost.
    /// Neither may reach the table or the headline.
    ///
    /// Regression guard for the 2026-09-22 finding: a 120s default expired at
    /// the 1k tier on a 4-core host and the report rendered 824,882 ops/sec and
    /// "1.01% drops" as if they were measurements.
    #[test]
    fn a_timed_out_run_is_withheld_from_the_table_and_every_aggregate() {
        // The timed-out run is the FASTEST on paper — so if it leaked into the
        // aggregates it would take the headline outright, not hide in it.
        let md = report_of(vec![
            run(1000, 9_999_999.0, 0.42, false),
            run(5000, 500_000.0, 0.01, true),
        ]);

        assert!(md.contains("_timed out_"), "the row must be marked:\n{md}");
        assert!(
            !md.contains("9,999,999"),
            "a timed-out ops/sec reached the report:\n{md}"
        );
        assert!(
            md.contains("500,000 ops/sec"),
            "the completed run must still headline:\n{md}"
        );
        assert!(
            md.contains("1.00%"),
            "max drop% must come from completed runs only (0.01 -> 1.00%):\n{md}"
        );
    }

    /// Every tier timing out must not render a 0 ops/sec headline — `max` over
    /// an empty set is 0.0, which would read as a measured collapse.
    #[test]
    fn all_runs_timed_out_yields_no_headline_figure() {
        let md = report_of(vec![run(1000, 9_999_999.0, 0.42, false)]);
        assert!(
            md.contains("No run produced a valid throughput figure"),
            "expected an explicit refusal:\n{md}"
        );
        assert!(
            !md.contains("Peak sustained throughput"),
            "a headline was emitted with no valid run:\n{md}"
        );
    }
}

#[derive(Debug, Serialize)]
struct FullReport {
    environment: Environment,
    runs: Vec<RunResult>,
    powersync_ceiling_ops_per_sec_low: u64,
    powersync_ceiling_ops_per_sec_high: u64,
}

/// Write JSON, markdown, and SVG artifacts to `cfg.out_dir`.
pub fn write_reports(cfg: &BenchConfig, runs: &[RunResult], env: &Environment) -> Result<()> {
    fs::create_dir_all(&cfg.out_dir).context("create out dir")?;

    let report = FullReport {
        environment: env.clone(),
        runs: runs.to_vec(),
        // PowerSync's published small-row server ceiling: 2,000–4,000 ops/sec.
        // Source: https://docs.powersync.com/resources/performance-and-limits
        powersync_ceiling_ops_per_sec_low: 2_000,
        powersync_ceiling_ops_per_sec_high: 4_000,
    };

    // JSON
    let json = serde_json::to_string_pretty(&report).context("serialize json")?;
    fs::write(
        Path::new(&cfg.out_dir).join("nostos-bench-results.json"),
        json,
    )
    .context("write json")?;

    // Markdown
    let md = render_markdown(&report);
    fs::write(Path::new(&cfg.out_dir).join("RESULTS.md"), md).context("write md")?;

    // SVG chart
    let svg = render_svg(&report);
    fs::write(Path::new(&cfg.out_dir).join("chart.svg"), svg).context("write svg")?;

    Ok(())
}

fn render_markdown(r: &FullReport) -> String {
    let mut s = String::new();
    s.push_str("# Nostos Week-1 Benchmark — Results\n\n");
    s.push_str("> Generated by `nostos-bench`. Methodology: ");
    s.push_str("[`docs/BENCHMARK-METHODOLOGY.md`](../docs/BENCHMARK-METHODOLOGY.md).\n\n");

    s.push_str("## Environment\n\n");
    s.push_str(&format!("- **Rust:** `{}`\n", r.environment.rustc));
    s.push_str(&format!(
        "- **Host:** `{}` ({} cores)\n",
        r.environment.hostname, r.environment.cpu_cores
    ));
    s.push_str(&format!(
        "- **Profile:** `{}` (events/run: {})\n",
        r.environment.profile, r.environment.events
    ));
    s.push_str(&format!(
        "- **Per-session buffer:** {}\n",
        r.environment.buffer
    ));
    s.push_str("- **Build:** `--release` (lto=fat, codegen-units=1)\n\n");

    s.push_str("## Throughput vs PowerSync\n\n");
    s.push_str(
        "PowerSync publishes a **server-side ceiling of ~2,000–4,000 ops/sec** for small rows. \
         Nostos's measurement is of the same logical operation (fanning row-change events to \
         connected clients) with a synthetic replicator on loopback.\n\n",
    );

    s.push_str("| Clients | ops/sec | drop% | p50 (ms) | p99 (ms) | delivered | vs PS high |\n");
    s.push_str("|---:|---:|---:|---:|---:|---:|---:|\n");
    for run in &r.runs {
        // A run that hit `--timeout-secs` never finished the workload, so its
        // ops/sec is a floor and its drop% counts in-flight events as lost.
        // Rendering either as a number invites the quote this table exists to
        // prevent — the row stays (delivered and latency are real) but those
        // two cells are struck out.
        if !run.throughput_valid {
            s.push_str(&format!(
                "| {} | _timed out_ | — | {:.2} | {:.2} | {} | — |\n",
                run.clients,
                run.p50_us / 1000.0,
                run.p99_us / 1000.0,
                run.events_delivered,
            ));
            continue;
        }
        let ratio = run.ops_per_sec / r.powersync_ceiling_ops_per_sec_high as f64;
        s.push_str(&format!(
            "| {} | {} | {:.2}% | {:.2} | {:.2} | {} | **{:.1}×** |\n",
            run.clients,
            grouped(run.ops_per_sec),
            run.drop_rate * 100.0,
            run.p50_us / 1000.0,
            run.p99_us / 1000.0,
            run.events_delivered,
            ratio,
        ));
    }

    let timed_out = r.runs.iter().filter(|x| !x.throughput_valid).count();
    if timed_out > 0 {
        s.push_str(&format!(
            "\n> **{timed_out} run(s) hit the wall-clock timeout** and delivered only part of \
             the workload. Their ops/sec and drop% are withheld above and excluded from every \
             figure below — a truncated window measures the clock, not the system. Re-run those \
             tiers with a larger `--timeout-secs`.\n"
        ));
    }

    s.push_str("\n## Interpretation\n\n");
    // Every aggregate below is computed over completed runs ONLY. A timed-out
    // run's ops/sec is a floor, and a floor silently entering a `max` would
    // understate the peak while looking like a measurement.
    let valid: Vec<&RunResult> = r.runs.iter().filter(|x| x.throughput_valid).collect();
    if valid.is_empty() {
        s.push_str(
            "- **No run produced a valid throughput figure** — every tier hit the wall-clock \
             timeout. Raise `--timeout-secs` and re-run before citing anything from this file.\n",
        );
    } else {
        let best = valid.iter().map(|x| x.ops_per_sec).fold(0.0_f64, f64::max);
        let ratio = best / r.powersync_ceiling_ops_per_sec_high as f64;
        s.push_str(&format!(
            "- **Peak sustained throughput: {} ops/sec** — **{:.1}×** PowerSync's published \
             high ceiling (4,000 ops/sec) and **{:.1}×** the low (2,000 ops/sec).\n",
            grouped(best),
            ratio,
            best / r.powersync_ceiling_ops_per_sec_low as f64,
        ));
        let max_drop = valid.iter().map(|x| x.drop_rate).fold(0.0_f64, f64::max);
        s.push_str(&format!(
            "- **Max drop rate across runs: {:.2}%** (lower is better; >1% is flagged as not \
             fully honest throughput in the methodology).\n",
            max_drop * 100.0,
        ));
    }
    s.push_str(
        "- The synthetic `FakeReplicator` generates events faster than the router pushes \
         them, so the measured ceiling is the **router + WebSocket fan-out path**, not Postgres. \
         Real `pgoutput` parsing cost is added in Week 2.\n",
    );
    s.push_str("\n## Caveats (stated for honesty)\n\n");
    s.push_str("- Loopback (127.0.0.1); no WAN latency.\n");
    s.push_str("- Synthetic replicator; real PG logical replication arrives Week 2.\n");
    s.push_str("- No client-side SQLite apply (no client SDK yet).\n");
    s.push_str("- Single server process.\n");
    s
}

fn render_svg(r: &FullReport) -> String {
    // A minimal hand-rolled bar chart. Not pretty; honest and dependency-free.
    let width = 720usize;
    let height = 420usize;
    let pad_l = 64usize;
    let pad_b = 60usize;
    let pad_t = 40usize;
    let pad_r = 40usize;
    let plot_w = width - pad_l - pad_r;
    let plot_h = height - pad_t - pad_b;

    let best = r.runs.iter().map(|x| x.ops_per_sec).fold(1.0_f64, f64::max);
    // Y axis goes a bit above the best to leave headroom; include PS ceiling.
    let y_max = best.max(r.powersync_ceiling_ops_per_sec_high as f64) * 1.15;
    let bar_count = r.runs.len();
    let group_w = plot_w / bar_count.max(1);
    let bar_w = (group_w * 3 / 5).max(8);

    let mut svg = String::new();
    svg.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" \
         font-family=\"sans-serif\" font-size=\"12\">\n"
    ));
    svg.push_str("<rect width=\"100%\" height=\"100%\" fill=\"white\"/>\n");
    svg.push_str(&format!(
        "<text x=\"{}\" y=\"20\" font-size=\"16\" font-weight=\"bold\">Nostos throughput vs PowerSync ceiling (ops/sec)</text>\n",
        pad_l
    ));

    // Y gridlines + labels (4 divisions).
    for i in 0..=4_u32 {
        let frac = f64::from(i) / 4.0;
        let y = pad_t + ((1.0 - frac) * plot_h as f64) as usize;
        let val = (y_max * frac) as u64;
        svg.push_str(&format!(
            "<line x1=\"{pad_l}\" y1=\"{y}\" x2=\"{}\" y2=\"{y}\" stroke=\"#eee\"/>\n",
            width - pad_r
        ));
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" text-anchor=\"end\" fill=\"#666\">{}</text>\n",
            pad_l - 8,
            y + 4,
            val
        ));
    }

    // Bars.
    for (i, run) in r.runs.iter().enumerate() {
        let x = pad_l + i * group_w + (group_w - bar_w) / 2;
        let h = ((run.ops_per_sec / y_max) * plot_h as f64) as usize;
        let y = pad_t + (plot_h - h);
        svg.push_str(&format!(
            "<rect x=\"{x}\" y=\"{y}\" width=\"{bar_w}\" height=\"{h}\" fill=\"#7c3aed\"/>\n"
        ));
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" font-weight=\"bold\">{}</text>\n",
            x + bar_w / 2,
            y - 6,
            grouped(run.ops_per_sec),
        ));
        svg.push_str(&format!(
            "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" fill=\"#444\">{} clients</text>\n",
            x + bar_w / 2,
            height - pad_b + 22,
            run.clients
        ));
    }

    // PowerSync ceiling reference line.
    let ps_y = pad_t
        + ((1.0 - r.powersync_ceiling_ops_per_sec_high as f64 / y_max) * plot_h as f64) as usize;
    svg.push_str(&format!(
        "<line x1=\"{pad_l}\" y1=\"{ps_y}\" x2=\"{}\" y2=\"{ps_y}\" stroke=\"#dc2626\" stroke-width=\"2\" stroke-dasharray=\"6,4\"/>\n",
        width - pad_r
    ));
    svg.push_str(&format!(
        "<text x=\"{}\" y=\"{}\" fill=\"#dc2626\" font-weight=\"bold\">PowerSync ceiling ~{} ops/sec</text>\n",
        width - pad_r - 4,
        ps_y - 6,
        r.powersync_ceiling_ops_per_sec_high
    ));

    svg.push_str("</svg>\n");
    svg
}
