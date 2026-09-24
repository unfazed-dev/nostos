//! `all`-mode startup warning (ADR-0031, Task 13).

use nostos_application::ports::TableStat;

/// Render the `sync_mode = "all"` guardrail banner. Pure formatting so it is
/// testable without a database. `None` row estimates render as "unknown"
/// (Postgres reports `reltuples = -1` for a never-analyzed table).
pub(crate) fn format_all_mode_warning(stats: &[TableStat]) -> String {
    let mut lines = vec![
        "WARNING: sync_mode = \"all\" — every replicated row reaches every authorised client."
            .to_string(),
    ];

    // Unknown estimates are never folded into `total` — a missing planner
    // stat must not silently masquerade as zero rows.
    let mut total: u64 = 0;
    for stat in stats {
        let table = &stat.table;
        let line = match stat.estimated_rows {
            Some(n) => {
                total += n;
                let est = format_thousands(n);
                format!("  {table}    ~{est} rows")
            }
            None => format!("  {table}    unknown rows (never analyzed)"),
        };
        lines.push(line);
    }

    let count = stats.len();
    let plural = if count == 1 { "" } else { "s" };
    let total_fmt = format_thousands(total);
    lines.push(format!(
        "  {count} table{plural}, ~{total_fmt} rows estimated."
    ));
    lines.push("  This is the zero-config development default. For production, run".to_string());
    lines.push("  `nostos rules init` and switch sync_mode to \"toggles\".".to_string());

    lines.join("\n")
}

/// Thousands-grouped decimal rendering (`12400` → `"12,400"`), for the
/// `all`-mode startup banner's row-count estimates.
fn format_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().rev().enumerate() {
        if i != 0 && i % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped.chars().rev().collect()
}

#[cfg(test)]
mod all_mode_warning_tests {
    use super::{format_all_mode_warning, TableStat};

    #[test]
    fn warning_lists_tables_and_total() {
        let stats = vec![
            TableStat {
                table: "tasks".to_string(),
                estimated_rows: Some(12_400),
            },
            TableStat {
                table: "notes".to_string(),
                estimated_rows: Some(340),
            },
        ];

        let out = format_all_mode_warning(&stats);

        assert!(out.starts_with("WARNING: sync_mode = \"all\""));
        assert!(out.contains("tasks"));
        assert!(out.contains("~12,400 rows"));
        assert!(out.contains("notes"));
        assert!(out.contains("~340 rows"));
        // 12,400 + 340 = 12,740.
        assert!(out.contains("2 tables, ~12,740 rows estimated."));
        assert!(out.contains("nostos rules init"));
    }

    #[test]
    fn unknown_estimate_renders_unknown() {
        let stats = vec![
            TableStat {
                table: "tasks".to_string(),
                estimated_rows: Some(100),
            },
            TableStat {
                table: "audit".to_string(),
                estimated_rows: None,
            },
        ];

        let out = format_all_mode_warning(&stats);

        assert!(out.contains("audit    unknown rows (never analyzed)"));
        // audit's unknown estimate is excluded from the total — only tasks's
        // 100 rows are counted, even though both tables are counted.
        assert!(out.contains("2 tables, ~100 rows estimated."));
        // No minus sign immediately followed by a digit anywhere (e.g. the
        // reltuples = -1 "never analyzed" sentinel must never leak through).
        // The banner's own prose ("zero-config") legitimately contains a
        // hyphen, so this checks for "-<digit>", not for any hyphen.
        assert!(
            !out.chars()
                .zip(out.chars().skip(1))
                .any(|(a, b)| a == '-' && b.is_ascii_digit()),
            "no negative number may appear: {out}"
        );
    }

    #[test]
    fn empty_stats_still_warns() {
        let out = format_all_mode_warning(&[]);

        assert!(out.starts_with("WARNING: sync_mode = \"all\""));
        assert!(out.contains("0 tables, ~0 rows estimated."));
    }
}
