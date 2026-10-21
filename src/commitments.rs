//! Dates promised to customers, kept in `commitments.json` because Jira has
//! no field for them. Seeded once from the NVIDIA tracker, edited in the UI.

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::model::IssueKey;
use crate::store::read_optional;

/// Who set a date from the dashboard.
pub(crate) const DASHBOARD_SOURCE: &str = "dashboard";

/// One promised date.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Commitment {
    /// The date.
    pub(crate) date: NaiveDate,
    /// Free text kept with it.
    pub(crate) note: Option<String>,
    /// Where it came from: the seed tracker or the dashboard.
    pub(crate) source: String,
    /// When it was last set.
    pub(crate) updated: DateTime<Local>,
}

/// All promised dates, by ticket.
pub(crate) type Commitments = BTreeMap<IssueKey, Commitment>;

/// Parses the file content.
pub(crate) fn parse(text: &str) -> Result<Commitments> {
    serde_json::from_str(text).context("the commitments file is not valid")
}

/// Renders the file content.
pub(crate) fn render(commitments: &Commitments) -> Result<String> {
    Ok(serde_json::to_string_pretty(commitments)? + "\n")
}

/// Reads the file; a missing file means no commitments, an unreadable one fails.
pub(crate) fn load(path: &Path) -> Result<Commitments> {
    match read_optional(path)? {
        Some(text) => parse(&text).with_context(|| format!("reading {}", path.display())),
        None => Ok(Commitments::new()),
    }
}

/// Sets `key`'s date, or removes it when `date` is `None`.
pub(crate) fn set(
    commitments: &mut Commitments,
    key: IssueKey,
    date: Option<NaiveDate>,
    note: Option<String>,
    now: DateTime<Local>,
) {
    match date {
        Some(date) => {
            commitments.insert(
                key,
                Commitment {
                    date,
                    note,
                    source: DASHBOARD_SOURCE.into(),
                    updated: now,
                },
            );
        }
        None => {
            commitments.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(s: &str) -> IssueKey {
        s.parse().unwrap()
    }

    fn day(d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, d).unwrap()
    }

    #[test]
    fn set_adds_replaces_and_clears() {
        let mut c = Commitments::new();
        let now = Local::now();

        set(&mut c, key("NVDCSE-247"), Some(day(1)), None, now);
        set(
            &mut c,
            key("NVDCSE-247"),
            Some(day(15)),
            Some("after the demo".into()),
            now,
        );
        set(&mut c, key("NCP-170"), Some(day(1)), None, now);
        set(&mut c, key("NCP-170"), None, None, now);

        assert_eq!(c.len(), 1);
        let kept = c.get(&key("NVDCSE-247")).unwrap();
        assert_eq!(
            (kept.date, kept.note.as_deref(), kept.source.as_str()),
            (day(15), Some("after the demo"), DASHBOARD_SOURCE)
        );
    }

    #[test]
    fn render_and_parse_round_trip() {
        let mut c = Commitments::new();
        set(&mut c, key("GCP-315"), Some(day(30)), None, Local::now());

        assert_eq!(parse(&render(&c).unwrap()).unwrap(), c);
    }

    #[test]
    fn bad_content_is_an_error_not_an_empty_set() {
        let message = |text: &str| format!("{:#}", parse(text).unwrap_err());

        assert!(
            message("[]").contains("expected a map"),
            "{}",
            message("[]")
        );
        assert!(message(r#"{"not a key": {"date": "2026-11-01", "source": "x", "updated": "2026-10-21T10:00:00+03:00"}}"#).contains("is not a Jira key"));
        assert!(
            message(r#"{"GCP-1": {"date": "2026-11-01", "source": "x", "updated": "2026-10-21T10:00:00+03:00", "extra": 1}}"#).contains("unknown field `extra`")
        );
    }

    #[test]
    fn load_treats_a_missing_file_as_empty() {
        let missing = std::env::temp_dir().join(format!(
            "exa-status-no-commitments-{}.json",
            std::process::id()
        ));

        assert!(load(&missing).unwrap().is_empty());
    }
}
