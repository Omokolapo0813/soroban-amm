//! Baseline parsing, regression and headroom checks, and the markdown summary.
//!
//! Everything here works on plain data (no contracts, no budget), so it is
//! covered by the unit tests at the bottom of the file.

use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;

/// Allowed growth over the baseline before `--check` fails, in basis points.
pub const REGRESSION_BPS: u128 = 500;
/// The per-transaction CPU instruction limit the Stellar network enforces.
pub const TX_MAX_INSTRUCTIONS: u64 = 100_000_000;
/// Share of [`TX_MAX_INSTRUCTIONS`] a bench may use before it needs an
/// allowlist entry.
pub const DEFAULT_HEADROOM_PCT: u64 = 60;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metric {
    pub name: String,
    pub cpu_instructions: u64,
    pub mem_bytes: u64,
}

// ── Baseline ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct BaselineFile {
    metrics: Vec<BaselineEntry>,
}

#[derive(Deserialize)]
struct BaselineEntry {
    name: String,
    cpu_instructions: u64,
    mem_bytes: u64,
}

/// `name -> (cpu_instructions, mem_bytes)`.
pub type Baseline = BTreeMap<String, (u64, u64)>;

pub fn parse_baseline(text: &str) -> Result<Baseline, String> {
    let file: BaselineFile =
        serde_json::from_str(text).map_err(|err| format!("benches/baseline.json: {err}"))?;
    let mut out = Baseline::new();
    for entry in file.metrics {
        if out
            .insert(
                entry.name.clone(),
                (entry.cpu_instructions, entry.mem_bytes),
            )
            .is_some()
        {
            return Err(format!(
                "benches/baseline.json lists {} more than once",
                entry.name
            ));
        }
    }
    Ok(out)
}

pub fn render_json(metrics: &[Metric]) -> String {
    let mut out = String::from("{\n  \"metrics\": [\n");
    for (idx, metric) in metrics.iter().enumerate() {
        let comma = if idx + 1 == metrics.len() { "" } else { "," };
        out.push_str(&format!(
            "    {{ \"name\": \"{}\", \"cpu_instructions\": {}, \"mem_bytes\": {} }}{}\n",
            metric.name, metric.cpu_instructions, metric.mem_bytes, comma
        ));
    }
    out.push_str("  ]\n}\n");
    out
}

pub fn check_regressions(metrics: &[Metric], baseline: &Baseline) -> Result<(), String> {
    for metric in metrics {
        let (cpu, mem) = baseline.get(&metric.name).ok_or_else(|| {
            format!(
                "missing baseline entry for {}; add it with `cargo run -p benches -- --write-baseline`",
                metric.name
            )
        })?;
        assert_within(
            &metric.name,
            "cpu_instructions",
            metric.cpu_instructions,
            *cpu,
        )?;
        assert_within(&metric.name, "mem_bytes", metric.mem_bytes, *mem)?;
    }
    Ok(())
}

fn assert_within(name: &str, key: &str, current: u64, baseline: u64) -> Result<(), String> {
    let allowed = (baseline as u128) * (10_000 + REGRESSION_BPS) / 10_000;
    if (current as u128) > allowed {
        return Err(format!(
            "{name} {key} regressed: current {current}, baseline {baseline}, allowed {allowed}\n\
             \n\
             If this increase is expected (e.g. a storage/TTL change to a hot-path\n\
             contract), regenerate the baseline and commit it in THIS pull request:\n\
             \x20   cargo run -p benches -- --write-baseline\n\
             Otherwise, your change made {name} slower than intended — investigate before merging."
        ));
    }
    Ok(())
}

/// Fails when the baseline names a bench that no longer exists, so removing a
/// bench from the code without removing it from the baseline is caught.
pub fn check_stale_baseline(metrics: &[Metric], baseline: &Baseline) -> Result<(), String> {
    let current: BTreeSet<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
    let stale: Vec<&str> = baseline
        .keys()
        .map(String::as_str)
        .filter(|name| !current.contains(name))
        .collect();
    if stale.is_empty() {
        return Ok(());
    }
    Err(format!(
        "benches/baseline.json has entries with no matching bench: {}\n\
         Remove them with `cargo run -p benches -- --write-baseline`.",
        stale.join(", ")
    ))
}

// ── Headroom ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AllowFile {
    allow: Vec<AllowEntry>,
}

#[derive(Deserialize)]
struct AllowEntry {
    name: String,
    reason: String,
    issue: String,
    max_limit_pct: u64,
}

/// `name -> highest share of the limit (in percent) the exception covers`.
pub type Allowlist = BTreeMap<String, u64>;

/// Parses `benches/allowlist.json`. Every entry must say why (`reason`), link
/// the work that removes the exception (`issue`, an http(s) URL) and cap how
/// far over the threshold it may go (`max_limit_pct`, 1..=100), otherwise the
/// whole file is rejected.
pub fn parse_allowlist(text: &str) -> Result<Allowlist, String> {
    let file: AllowFile =
        serde_json::from_str(text).map_err(|err| format!("benches/allowlist.json: {err}"))?;
    let mut out = Allowlist::new();
    for entry in file.allow {
        let name = entry.name;
        if entry.reason.trim().len() < 20 {
            return Err(format!(
                "benches/allowlist.json: {name} needs a real `reason` (at least 20 characters)"
            ));
        }
        if !(entry.issue.starts_with("https://") || entry.issue.starts_with("http://")) {
            return Err(format!(
                "benches/allowlist.json: {name} needs an `issue` URL, got {:?}",
                entry.issue
            ));
        }
        if entry.max_limit_pct == 0 || entry.max_limit_pct > 100 {
            return Err(format!(
                "benches/allowlist.json: {name} has max_limit_pct {} outside 1..=100",
                entry.max_limit_pct
            ));
        }
        if out.insert(name.clone(), entry.max_limit_pct).is_some() {
            return Err(format!(
                "benches/allowlist.json lists {name} more than once"
            ));
        }
    }
    Ok(out)
}

/// Percent of the per-transaction limit, rounded down, for display.
pub fn limit_pct(cpu_instructions: u64) -> u64 {
    (u128::from(cpu_instructions) * 100 / u128::from(TX_MAX_INSTRUCTIONS)) as u64
}

fn exceeds_pct(cpu_instructions: u64, pct: u64) -> bool {
    u128::from(cpu_instructions) * 100 > u128::from(pct) * u128::from(TX_MAX_INSTRUCTIONS)
}

/// Fails when a bench uses more than `threshold_pct` of the per-transaction
/// instruction limit and is not covered by an allowlist entry whose own cap it
/// stays under. Also fails for an allowlist entry with no matching bench.
pub fn check_headroom(
    metrics: &[Metric],
    threshold_pct: u64,
    allowlist: &Allowlist,
) -> Result<(), String> {
    for metric in metrics {
        if !exceeds_pct(metric.cpu_instructions, threshold_pct) {
            continue;
        }
        match allowlist.get(&metric.name) {
            Some(cap) if !exceeds_pct(metric.cpu_instructions, *cap) => {}
            Some(cap) => {
                return Err(format!(
                    "{} uses {}% of the per-transaction instruction limit, over its allowlisted {}%.\n\
                     Lower the cost, or raise max_limit_pct in benches/allowlist.json with a justification.",
                    metric.name,
                    limit_pct(metric.cpu_instructions),
                    cap
                ));
            }
            None => {
                return Err(format!(
                    "{} uses {}% of the per-transaction instruction limit (threshold {}%).\n\
                     Reduce its cost, or add it to benches/allowlist.json with a reason and an issue link.",
                    metric.name,
                    limit_pct(metric.cpu_instructions),
                    threshold_pct
                ));
            }
        }
    }
    let current: BTreeSet<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
    for name in allowlist.keys() {
        if !current.contains(name.as_str()) {
            return Err(format!(
                "benches/allowlist.json has an entry for {name}, which is not a bench"
            ));
        }
    }
    Ok(())
}

// ── Summary ───────────────────────────────────────────────────────────────────

/// Markdown table for `$GITHUB_STEP_SUMMARY`: name, instructions, share of the
/// per-transaction limit, change against the baseline.
pub fn render_summary(metrics: &[Metric], baseline: &Baseline) -> String {
    let mut out = String::from("| bench | instructions | % of tx limit | vs baseline |\n");
    out.push_str("| --- | ---: | ---: | ---: |\n");
    for metric in metrics {
        let delta = match baseline.get(&metric.name) {
            Some((cpu, _)) if *cpu > 0 => {
                let bps = (i128::from(metric.cpu_instructions) - i128::from(*cpu)) * 10_000
                    / i128::from(*cpu);
                format!("{:+}.{:02}%", bps / 100, (bps % 100).abs())
            }
            _ => "new".to_string(),
        };
        out.push_str(&format!(
            "| {} | {} | {}% | {} |\n",
            metric.name,
            metric.cpu_instructions,
            limit_pct(metric.cpu_instructions),
            delta
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(name: &str, cpu: u64) -> Metric {
        Metric {
            name: name.to_string(),
            cpu_instructions: cpu,
            mem_bytes: 1_000,
        }
    }

    fn baseline(entries: &[(&str, u64)]) -> Baseline {
        entries
            .iter()
            .map(|(name, cpu)| (name.to_string(), (*cpu, 1_000)))
            .collect()
    }

    const ALLOW_OK: &str = r#"{ "$comment": "x", "allow": [
        { "name": "a.big", "reason": "grows with the number of hops until the cap lands",
          "issue": "https://github.com/promisszn/soroban-amm/issues/1", "max_limit_pct": 80 } ] }"#;

    #[test]
    fn parses_the_committed_baseline_format() {
        let text = r#"{ "metrics": [
            { "name": "a", "cpu_instructions": 10, "mem_bytes": 2 },
            { "name": "b", "cpu_instructions": 30, "mem_bytes": 4 } ] }"#;
        let parsed = parse_baseline(text).unwrap();
        assert_eq!(parsed.get("a"), Some(&(10, 2)));
        assert_eq!(parsed.get("b"), Some(&(30, 4)));
    }

    #[test]
    fn rendered_json_round_trips_through_the_parser() {
        let metrics = vec![metric("x.y", 123), metric("z", 456)];
        let parsed = parse_baseline(&render_json(&metrics)).unwrap();
        assert_eq!(parsed.get("x.y"), Some(&(123, 1_000)));
        assert_eq!(parsed.len(), 2);
    }

    #[test]
    fn rejects_malformed_and_duplicate_baseline_entries() {
        assert!(parse_baseline("{ not json").is_err());
        assert!(parse_baseline(r#"{ "metrics": [ { "name": "a" } ] }"#).is_err());
        let dup = r#"{ "metrics": [
            { "name": "a", "cpu_instructions": 1, "mem_bytes": 1 },
            { "name": "a", "cpu_instructions": 2, "mem_bytes": 1 } ] }"#;
        assert!(parse_baseline(dup).unwrap_err().contains("more than once"));
    }

    #[test]
    fn a_name_that_is_a_prefix_of_another_is_not_confused_with_it() {
        // The old string-search reader matched `"name": "amm.swap` inside
        // `"name": "amm.swap_exact_out"`.
        let b = baseline(&[("amm.swap_exact_out", 999), ("amm.swap", 5)]);
        assert_eq!(b.get("amm.swap"), Some(&(5, 1_000)));
        assert!(check_regressions(&[metric("amm.swap", 5)], &b).is_ok());
    }

    #[test]
    fn regressions_within_five_percent_pass_and_beyond_fail() {
        let b = baseline(&[("a", 1_000_000)]);
        assert!(check_regressions(&[metric("a", 1_050_000)], &b).is_ok());
        let err = check_regressions(&[metric("a", 1_050_001)], &b).unwrap_err();
        assert!(err.contains("a cpu_instructions regressed"));
    }

    #[test]
    fn a_bench_missing_from_the_baseline_is_reported_by_name() {
        let err = check_regressions(&[metric("new.bench", 1)], &Baseline::new()).unwrap_err();
        assert!(err.contains("missing baseline entry for new.bench"));
    }

    #[test]
    fn a_baseline_entry_without_a_bench_is_stale() {
        let b = baseline(&[("kept", 1), ("removed.bench", 1)]);
        let err = check_stale_baseline(&[metric("kept", 1)], &b).unwrap_err();
        assert!(err.contains("removed.bench"));
        assert!(!err.contains("kept,"));
        assert!(check_stale_baseline(&[metric("kept", 1), metric("removed.bench", 1)], &b).is_ok());
    }

    #[test]
    fn headroom_fails_naming_the_entry_and_lowering_the_threshold_trips_it() {
        let metrics = vec![metric("small", 10_000_000), metric("a.big", 55_000_000)];
        let none = Allowlist::new();
        assert!(check_headroom(&metrics, DEFAULT_HEADROOM_PCT, &none).is_ok());

        let err = check_headroom(&metrics, 50, &none).unwrap_err();
        assert!(err.contains("a.big"), "{err}");
        assert!(err.contains("55%"), "{err}");
        let err = check_headroom(&metrics, 5, &none).unwrap_err();
        assert!(err.contains("small") || err.contains("a.big"));
    }

    #[test]
    fn headroom_is_exact_at_the_boundary() {
        let none = Allowlist::new();
        // 60% of 100M exactly is fine; one instruction more is not.
        assert!(check_headroom(&[metric("edge", 60_000_000)], 60, &none).is_ok());
        assert!(check_headroom(&[metric("edge", 60_000_001)], 60, &none).is_err());
    }

    #[test]
    fn an_allowlist_entry_covers_a_bench_up_to_its_own_cap() {
        let allow = parse_allowlist(ALLOW_OK).unwrap();
        assert!(check_headroom(&[metric("a.big", 75_000_000)], 60, &allow).is_ok());
        assert!(check_headroom(&[metric("a.big", 80_000_000)], 60, &allow).is_ok());
        let err = check_headroom(&[metric("a.big", 80_000_001)], 60, &allow).unwrap_err();
        assert!(err.contains("allowlisted 80%"), "{err}");
    }

    #[test]
    fn an_allowlist_entry_for_a_removed_bench_is_an_error() {
        let allow = parse_allowlist(ALLOW_OK).unwrap();
        let err = check_headroom(&[metric("other", 1)], 60, &allow).unwrap_err();
        assert!(err.contains("a.big"));
    }

    #[test]
    fn allowlist_entries_need_a_reason_an_issue_link_and_a_sane_cap() {
        let with = |reason: &str, issue: &str, cap: u64| {
            format!(
                r#"{{ "allow": [ {{ "name": "n", "reason": "{reason}", "issue": "{issue}", "max_limit_pct": {cap} }} ] }}"#
            )
        };
        let long = "the cost grows with the path length until a cap lands";
        assert!(parse_allowlist(&with(long, "https://example.com/issues/1", 70)).is_ok());
        assert!(parse_allowlist(&with("todo", "https://example.com/1", 70))
            .unwrap_err()
            .contains("reason"));
        assert!(parse_allowlist(&with(long, "", 70))
            .unwrap_err()
            .contains("issue"));
        assert!(parse_allowlist(&with(long, "#12", 70))
            .unwrap_err()
            .contains("issue"));
        assert!(parse_allowlist(&with(long, "https://example.com/1", 0)).is_err());
        assert!(parse_allowlist(&with(long, "https://example.com/1", 101)).is_err());
        assert!(parse_allowlist(r#"{ "allow": [ { "name": "n" } ] }"#).is_err());
    }

    #[test]
    fn duplicate_allowlist_entries_are_rejected() {
        let text = r#"{ "allow": [
            { "name": "n", "reason": "a long enough reason for this", "issue": "https://x.y/1", "max_limit_pct": 70 },
            { "name": "n", "reason": "a long enough reason for this", "issue": "https://x.y/1", "max_limit_pct": 70 } ] }"#;
        assert!(parse_allowlist(text)
            .unwrap_err()
            .contains("more than once"));
    }

    #[test]
    fn an_empty_allowlist_is_valid() {
        assert!(
            parse_allowlist(r#"{ "$comment": "none yet", "allow": [] }"#)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_summary_table_shows_share_of_limit_and_delta() {
        let b = baseline(&[("a", 20_000_000), ("flat", 1_000)]);
        let table = render_summary(
            &[
                metric("a", 21_000_000),
                metric("flat", 1_000),
                metric("fresh", 5),
            ],
            &b,
        );
        assert!(table.starts_with("| bench | instructions | % of tx limit | vs baseline |"));
        assert!(table.contains("| a | 21000000 | 21% | +5.00% |"), "{table}");
        assert!(table.contains("| flat | 1000 | 0% | +0.00% |"), "{table}");
        assert!(table.contains("| fresh | 5 | 0% | new |"), "{table}");
    }

    #[test]
    fn the_summary_shows_improvements_as_negative() {
        let b = baseline(&[("a", 1_000)]);
        let table = render_summary(&[metric("a", 900)], &b);
        assert!(table.contains("-10.00%"), "{table}");
    }
}
