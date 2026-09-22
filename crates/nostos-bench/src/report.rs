//! Report generation — JSON artifact, human-readable RESULTS.md, and an SVG
//! chart drawing the Nostos throughput curve against PowerSync's published
//! ceiling.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::stats::trimmed_mean;
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
    /// Offered arrival rate, events/sec. `0` = unpaced flood.
    pub rate: u64,
    /// Measured repetitions per tier, and the warm-ups discarded before them.
    pub reps: usize,
    pub warmup_reps: usize,
    /// Seed the tier/rep order was shuffled with — the run order is part of
    /// the method, so it belongs in the recorded environment.
    pub order_seed: u64,
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
            rate: cfg.rate,
            reps: cfg.reps,
            warmup_reps: cfg.warmup_reps,
            order_seed: cfg.order_seed,
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
            rep: 0,
            events_total: 100_000,
            events_delivered: 1,
            events_superseded: 0,
            ops_per_sec: ops,
            drop_rate,
            p50_us: 10.0,
            p99_us: 86.0,
            elapsed_secs: 120.0,
            throughput_valid,
            target_rate: 0,
            rate_held: true,
            profile: "small".into(),
        }
    }

    /// A paced run whose generator fell behind its own schedule: it offered
    /// less load than it claimed, so its throughput and drop rate describe the
    /// generator.
    fn missed_rate(clients: usize, ops: f64) -> RunResult {
        RunResult {
            target_rate: 500_000,
            rate_held: false,
            ..run(clients, ops, 0.42, true)
        }
    }

    fn report_of(runs: Vec<RunResult>) -> String {
        let tiers = summarize(&runs);
        render_markdown(&FullReport {
            environment: Environment {
                rustc: UNKNOWN.into(),
                profile: "small".into(),
                buffer: 1024,
                events: 100_000,
                hostname: "test".into(),
                cpu_cores: 8,
                rate: 0,
                reps: 5,
                warmup_reps: 1,
                order_seed: 0x000C_A110_5EED,
            },
            tiers,
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

    /// The headline is the trimmed mean of a tier's repetitions, never its best
    /// one. A `max` over raw reps reports the luckiest run of the session as
    /// the figure — the exact variance dishonesty `--reps` exists to stop.
    ///
    /// The spread, by contrast, MUST show the outlier: that is what tells a
    /// reader the tier is unstable.
    #[test]
    fn the_headline_is_the_trimmed_mean_not_the_luckiest_rep() {
        let md = report_of(vec![
            run(1000, 400_000.0, 0.001, true),
            run(1000, 500_000.0, 0.001, true),
            run(1000, 500_000.0, 0.001, true),
            run(1000, 500_000.0, 0.001, true),
            run(1000, 9_999_999.0, 0.001, true),
        ]);

        assert!(
            md.contains("**Peak sustained throughput: 500,000 ops/sec**"),
            "headline must be the trimmed mean:\n{md}"
        );
        assert!(
            md.contains("400,000–9,999,999"),
            "the spread must still expose the outlier:\n{md}"
        );
        assert!(md.contains("| 5/5 |"), "rep count must be visible:\n{md}");
    }

    /// A generator that fell behind its schedule offered less load than it
    /// claimed, so its numbers are the generator's, not the server's. Withheld
    /// exactly like a timeout — and labelled differently, because the fix is
    /// different (lower the rate, or generate off-box).
    #[test]
    fn a_run_that_missed_its_offered_rate_is_withheld_like_a_timeout() {
        // Again the invalid run is the fastest in the set.
        let md = report_of(vec![
            missed_rate(1000, 9_999_999.0),
            run(5000, 500_000.0, 0.01, true),
        ]);

        assert!(
            md.contains("_rate not held_"),
            "the row must name the failure:\n{md}"
        );
        assert!(
            !md.contains("9,999,999"),
            "an unmet-rate ops/sec reached the report:\n{md}"
        );
        assert!(
            md.contains("**Peak sustained throughput: 500,000 ops/sec**"),
            "the valid tier must still headline:\n{md}"
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

/// One client tier, aggregated over its measured repetitions.
///
/// Per-rep rows stay in the JSON; this is what the markdown table and the chart
/// render, because a lone repetition is not a result. The spread across reps is
/// part of the figure, not a footnote to it — a tier whose reps disagree by 20%
/// has not measured anything, however good its mean looks.
#[derive(Debug, Clone, Serialize)]
pub struct TierSummary {
    pub clients: usize,
    /// Trimmed mean over the valid reps: fastest and slowest dropped (MLPerf).
    pub ops_per_sec: f64,
    pub ops_min: f64,
    pub ops_max: f64,
    /// Worst (highest) drop rate among the valid reps.
    pub drop_rate: f64,
    /// Worst latency across ALL reps, valid or not. A truncated window does not
    /// bias the frames that did land, so those samples still count.
    pub p50_us: f64,
    pub p99_us: f64,
    pub events_delivered: u64,
    pub reps_valid: usize,
    pub reps_total: usize,
    pub timed_out: usize,
    pub rate_missed: usize,
}

impl TierSummary {
    /// What goes in the ops/sec cell when no repetition was usable. Naming the
    /// reason matters: a timeout says raise `--timeout-secs`, a missed rate
    /// says the generator, not the server, was the bottleneck.
    #[must_use]
    pub fn invalid_label(&self) -> &'static str {
        if self.timed_out > 0 {
            "_timed out_"
        } else {
            "_rate not held_"
        }
    }
}

/// Collapse per-rep runs into one summary per client tier, ascending.
///
/// A rep counts toward the throughput figures only if it both finished inside
/// its window (`throughput_valid`) and offered the rate it claimed
/// (`rate_held`). Everything else is reported, then excluded.
#[must_use]
pub fn summarize(runs: &[RunResult]) -> Vec<TierSummary> {
    let mut tiers: Vec<usize> = runs.iter().map(|r| r.clients).collect();
    tiers.sort_unstable();
    tiers.dedup();

    tiers
        .into_iter()
        .map(|clients| {
            let all: Vec<&RunResult> = runs.iter().filter(|r| r.clients == clients).collect();
            let valid: Vec<&&RunResult> = all
                .iter()
                .filter(|r| r.throughput_valid && r.rate_held)
                .collect();
            let ops: Vec<f64> = valid.iter().map(|r| r.ops_per_sec).collect();
            let max_of =
                |f: fn(&RunResult) -> f64| all.iter().map(|r| f(r)).fold(0.0_f64, f64::max);
            TierSummary {
                clients,
                ops_per_sec: trimmed_mean(&ops),
                ops_min: if ops.is_empty() {
                    0.0
                } else {
                    ops.iter().copied().fold(f64::INFINITY, f64::min)
                },
                ops_max: ops.iter().copied().fold(0.0_f64, f64::max),
                drop_rate: valid.iter().map(|r| r.drop_rate).fold(0.0_f64, f64::max),
                p50_us: max_of(|r| r.p50_us),
                p99_us: max_of(|r| r.p99_us),
                events_delivered: all.iter().map(|r| r.events_delivered).max().unwrap_or(0),
                reps_valid: valid.len(),
                reps_total: all.len(),
                timed_out: all.iter().filter(|r| !r.throughput_valid).count(),
                rate_missed: all.iter().filter(|r| !r.rate_held).count(),
            }
        })
        .collect()
}

#[derive(Debug, Serialize)]
struct FullReport {
    environment: Environment,
    runs: Vec<RunResult>,
    tiers: Vec<TierSummary>,
    powersync_ceiling_ops_per_sec_low: u64,
    powersync_ceiling_ops_per_sec_high: u64,
}

/// Write JSON, markdown, and SVG artifacts to `cfg.out_dir`.
pub fn write_reports(cfg: &BenchConfig, runs: &[RunResult], env: &Environment) -> Result<()> {
    fs::create_dir_all(&cfg.out_dir).context("create out dir")?;

    let report = FullReport {
        environment: env.clone(),
        runs: runs.to_vec(),
        tiers: summarize(runs),
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

    s.push_str(
        "| Clients | ops/sec | spread (min–max) | drop% | p50 (ms) | p99 (ms) | reps | vs PS high |\n",
    );
    s.push_str("|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for t in &r.tiers {
        let reps = format!("{}/{}", t.reps_valid, t.reps_total);
        // A tier with no usable repetition keeps its row — the delivered count
        // and the latencies are real — but the figures its invalid runs
        // corrupted are named rather than rendered as numbers someone could
        // quote. The label says which failure it was, because the two have
        // different fixes.
        if t.reps_valid == 0 {
            s.push_str(&format!(
                "| {} | {} | — | — | {:.2} | {:.2} | {} | — |\n",
                t.clients,
                t.invalid_label(),
                t.p50_us / 1000.0,
                t.p99_us / 1000.0,
                reps,
            ));
            continue;
        }
        let ratio = t.ops_per_sec / r.powersync_ceiling_ops_per_sec_high as f64;
        s.push_str(&format!(
            "| {} | {} | {}–{} | {:.2}% | {:.2} | {:.2} | {} | **{:.1}×** |\n",
            t.clients,
            grouped(t.ops_per_sec),
            grouped(t.ops_min),
            grouped(t.ops_max),
            t.drop_rate * 100.0,
            t.p50_us / 1000.0,
            t.p99_us / 1000.0,
            reps,
            ratio,
        ));
    }

    s.push_str(&format!(
        "\n> **Repetition policy:** {} measured repetitions per tier — fastest and slowest \
         dropped, mean of the rest (MLPerf). {} warm-up repetition(s) per tier run first and \
         discarded. Tier/rep order randomised (seed `{:#x}`) so no tier permanently owns the \
         cold-cache slot. The `spread` column is the min-max across valid reps and is **part of \
         the figure**: a tier whose reps disagree has not measured anything, however good its \
         mean looks.\n",
        r.environment.reps, r.environment.warmup_reps, r.environment.order_seed,
    ));

    if r.environment.rate > 0 {
        s.push_str(&format!(
            "\n> **Offered rate: {} events/sec into the router** (so `rate x clients` \
             deliveries/sec), held open-loop (event `i` is due at \
             `start + i/rate` regardless of what the router did with `i-1`). These figures \
             answer *\"does the system hold this rate under the drop bar\"* — a run whose \
             generator fell behind its own schedule is marked `_rate not held_` and excluded, \
             because it offered less load than it claimed.\n",
            grouped(r.environment.rate as f64),
        ));
    } else {
        s.push_str(
            "\n> **Unpaced:** the generator floods as fast as the router accepts, so these \
             figures answer *\"where does it fall over\"*, not *\"does it meet rate R at under \
             1% loss\"*. The second question is the one a user with a workload has; `--rate` \
             asks it.\n",
        );
    }

    let timed_out = r.runs.iter().filter(|x| !x.throughput_valid).count();
    if timed_out > 0 {
        s.push_str(&format!(
            "\n> **{timed_out} repetition(s) hit the wall-clock timeout** and delivered only \
             part of the workload. Their ops/sec and drop% are excluded from every figure in \
             this file — a truncated window measures the clock, not the system. Re-run those \
             tiers with a larger `--timeout-secs`.\n"
        ));
    }
    let rate_missed = r.runs.iter().filter(|x| !x.rate_held).count();
    if rate_missed > 0 {
        s.push_str(&format!(
            "\n> **{rate_missed} repetition(s) could not hold the offered rate** — the \
             generator fell behind its own schedule, so the load offered was below the load \
             requested and the drop rate describes the generator. Excluded. Lower `--rate`, or \
             move the generator off-box.\n"
        ));
    }

    s.push_str("\n## Interpretation\n\n");
    // Every aggregate below is computed over completed runs ONLY. A timed-out
    // run's ops/sec is a floor, and a floor silently entering a `max` would
    // understate the peak while looking like a measurement.
    // Peak is a max over TIER means, never over raw repetitions: a max over
    // reps would report the luckiest run of the session as the headline, which
    // is exactly the variance dishonesty the repetition policy exists to stop.
    let valid: Vec<&TierSummary> = r.tiers.iter().filter(|x| x.reps_valid > 0).collect();
    if valid.is_empty() {
        s.push_str(
            "- **No run produced a valid throughput figure** — every tier either hit the \
             wall-clock timeout or failed to hold its offered rate. Fix that and re-run before \
             citing anything from this file.\n",
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

    // Chart the tier means, not the repetitions: one bar per tier is the claim.
    let bars: Vec<&TierSummary> = r.tiers.iter().filter(|t| t.reps_valid > 0).collect();
    let best = bars.iter().map(|x| x.ops_per_sec).fold(1.0_f64, f64::max);
    // Y axis goes a bit above the best to leave headroom; include PS ceiling.
    let y_max = best.max(r.powersync_ceiling_ops_per_sec_high as f64) * 1.15;
    let bar_count = bars.len();
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
    for (i, run) in bars.iter().enumerate() {
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
