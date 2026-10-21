//! `exa-status refresh-once`: one refresh, run by the server as its own
//! process so the memory a refresh needs goes back to the system afterwards.

use anyhow::Result;
use chrono::{Local, NaiveDate};
use std::path::Path;
use std::time::Instant;
use tracing::warn;

use crate::build::{self, GerritState, Timing};
use crate::config::Config;
use crate::fetch;
use crate::model::{HealthPart, Snapshot};
use crate::store;

/// Reads everything, writes the snapshot, then adds Gerrit patch state (the
/// slowest part) and writes it again, so the views never wait for Gerrit.
/// Holds the refresh lock throughout, so two refreshes never interleave.
/// With `out`, the snapshot goes there and no history line is written.
pub(crate) fn refresh_once(cfg: &Config, out: Option<&Path>, print: bool) -> Result<()> {
    let _lock = store::lock_refresh(&cfg.data_dir)?;
    let live = cfg.data_file(store::SNAPSHOT_FILE);
    let path = out.unwrap_or(&live);
    let started = Instant::now();
    let timing = || Timing {
        generated_at: Local::now(),
        duration_secs: started.elapsed().as_secs_f64(),
    };
    let raw = fetch::gather(cfg)?;
    let first = build::snapshot(cfg, &raw, GerritState::NotRead, timing())?;
    store::write_json(path, &first)?;

    let watched = build::watch_list(cfg, &first);
    let snap = match fetch::patches(cfg, &watched) {
        Ok(patches) => build::snapshot(cfg, &raw, GerritState::Read(&patches), timing())?,
        Err(e) => {
            warn!("Gerrit: {e:#}");
            let message = format!("{e:#}");
            build::snapshot(cfg, &raw, GerritState::Failed(&message), timing())?
        }
    };
    store::write_json(path, &snap)?;
    if out.is_none() {
        let line = store::history_line(&snap, !build::complete(&raw));
        // The history is not needed for the page, so a failed append only warns.
        if let Err(e) = store::append_history(&cfg.data_file(store::HISTORY_FILE), &line) {
            warn!("{e:#}");
        }
    }
    if print {
        println!("{}", summary(&snap));
    }
    Ok(())
}

/// "55.8% of 500", "none of 0 (2 cancelled, ...)", or the error.
fn part_text(part: &HealthPart) -> String {
    if let Some(e) = &part.error {
        return format!("unavailable ({e})");
    }
    let rate = part
        .rate
        .map_or_else(|| "none".to_string(), |r| format!("{r:.1}%"));
    let capped = if part.capped {
        ", EXA Reporter's limit"
    } else {
        ""
    };
    let excluded = if part.excluded > 0 {
        format!(
            " ({} cancelled, skipped, timed out or still running)",
            part.excluded
        )
    } else {
        String::new()
    };
    format!("{rate} of {}{capped}{excluded}", part.count)
}

/// The numbers a person checks against Jira, one view per block.
fn summary(s: &Snapshot) -> String {
    let date = |d: Option<NaiveDate>| d.map_or_else(|| "-".to_string(), |d| d.to_string());
    let mut lines = vec![format!(
        "EXA status snapshot {} ({:.1}s)",
        s.generated_at.to_rfc3339(),
        s.duration_secs
    )];
    for src in &s.sources {
        let state = src
            .error
            .as_deref()
            .map_or_else(|| "ok".to_string(), |e| format!("FAILED {e}"));
        lines.push(format!("  source {:<15} {state}", src.name));
    }
    for tab in &s.tabs {
        lines.push(format!("\n[{}] {:?}", tab.label, tab.verdict));
        for v in &tab.views {
            lines.push(format!(
                "  {:<12} {:<10} open={:<5} blockers={:<3} {}",
                v.label,
                format!("{:?}", v.verdict),
                v.items.len(),
                v.blocker_count(),
                v.summary
            ));
            for k in &v.kpis {
                lines.push(format!("      {:<22} {:<10} {}", k.label, k.value, k.note));
            }
            for t in v
                .tracks
                .iter()
                .chain(v.initiatives.iter().map(|c| &c.track))
            {
                lines.push(format!(
                    "      track {:<30} open={:<4} weighted={:<6} closed/wk={:<5} projected={:<11} pm={:<11} jira={:<11} {:?}: {}",
                    t.name,
                    t.open,
                    t.weighted,
                    t.closed_per_week,
                    date(t.projected),
                    date(t.pm_date),
                    date(t.jira_date),
                    t.verdict,
                    t.reason
                ));
            }
            for g in &v.date_groups {
                lines.push(format!(
                    "      due {} ({}d) {:?} {} tickets",
                    g.date,
                    g.days_left,
                    g.verdict,
                    g.keys.len()
                ));
            }
            let patched = v.items.iter().filter(|t| !t.patches.is_empty()).count();
            if patched > 0 {
                lines.push(format!("      {patched} items with Gerrit changes"));
            }
            if let Some(h) = &v.health {
                for (name, part) in [("es6", &h.es6), ("es7", &h.es7), ("EMF", &h.emf)] {
                    if let Some(part) = part {
                        lines.push(format!("      health {name} {}", part_text(part)));
                    }
                }
            }
            for n in &v.notes {
                lines.push(format!("      note: {n}"));
            }
        }
    }
    lines.join("\n")
}
