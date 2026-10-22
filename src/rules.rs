//! The judgement calls: which group a customer ticket belongs to, what counts
//! as a blocker, how a release projects, and the verdicts. An idle time that
//! could not be read never counts as fresh: where a rule needs it, unknown is
//! treated as at risk.

use anyhow::{Context, Result};
use chrono::{Duration, NaiveDate};
use regex::Regex;
use std::collections::BTreeMap;

use crate::config::Config;
use crate::model::{
    Blocker, Category, DateGroup, DateSource, FixState, IssueKey, PmRef, Severity, StatusCount,
    TargetSource, Ticket, Tone, Track, Verdict,
};

/// Priority that marks a blocker.
pub(crate) const BLOCKER: &str = "Blocker";
/// Priority counted in the "Critical" number.
pub(crate) const CRITICAL: &str = "Critical";
/// Priorities that order release items, most urgent first.
const PRIORITY_ORDER: [&str; 3] = [BLOCKER, CRITICAL, "Major"];
/// Link text of a blocking link, from the blocked ticket's side.
const IS_BLOCKED_BY: &str = "is blocked by";
/// Workflow statuses containing this word ("Blocked", "Blocked External") mean blocked.
const BLOCKED_WORD: &str = "blocked";
/// Issue type of test cases.
const TEST_CASE: &str = "Test";

/// The customer group a ticket belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Bucket {
    /// NVIDIA, all departments.
    Nvidia,
    /// Google Cloud.
    Gcp,
    /// Every other customer.
    Other,
    /// A test ticket; left out of every customer view.
    Test,
}

/// Sorts customer tickets into buckets.
pub(crate) struct Classifier {
    nvidia_site: Regex,
    nvidia_summary: Regex,
    nvidia_projects: Vec<String>,
    gcp: Regex,
    gcp_projects: Vec<String>,
    test: Regex,
}

impl Classifier {
    /// Compiles the configured patterns.
    pub(crate) fn new(cfg: &Config) -> Result<Self> {
        Ok(Self {
            nvidia_site: Regex::new(&cfg.nvidia_site).context("nvidia_site")?,
            nvidia_summary: Regex::new(&cfg.nvidia_summary).context("nvidia_summary")?,
            nvidia_projects: cfg.nvidia_projects.clone(),
            gcp: Regex::new(&cfg.gcp_pattern).context("gcp_pattern")?,
            gcp_projects: cfg.gcp_projects.clone(),
            test: Regex::new(&cfg.test_pattern).context("test_pattern")?,
        })
    }

    /// The ticket's bucket: test tickets first, then by project, then by
    /// "Site Affected" or the case title.
    pub(crate) fn bucket(&self, t: &Ticket) -> Bucket {
        let site = t.site.as_deref().unwrap_or_default();
        let project = t.key.project();
        if self.test.is_match(&t.summary) {
            Bucket::Test
        } else if self.nvidia_projects.iter().any(|p| p == project) {
            Bucket::Nvidia
        } else if self.gcp_projects.iter().any(|p| p == project) {
            Bucket::Gcp
        } else if self.nvidia_site.is_match(site) || self.nvidia_summary.is_match(&t.summary) {
            Bucket::Nvidia
        } else if self.gcp.is_match(site) || self.gcp.is_match(&t.summary) {
            Bucket::Gcp
        } else {
            Bucket::Other
        }
    }
}

/// Folds a ticket and its copy in a mirror project (NCP-163 and NP-131, for
/// example), recognised by the same summary, into one row. The first one seen
/// keeps the row; see `merge` for what it takes from the copy.
pub(crate) fn dedup(tickets: Vec<Ticket>, mirrors: &[(String, String)]) -> Vec<Ticket> {
    let mirrored = |a: &str, b: &str| {
        mirrors
            .iter()
            .any(|(x, y)| (x == a && y == b) || (x == b && y == a))
    };
    let mut rows: Vec<Ticket> = Vec::with_capacity(tickets.len());
    let mut by_summary: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for t in tickets {
        let norm = t
            .summary
            .to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let copy_of = by_summary
            .get(&norm)
            .filter(|_| !norm.is_empty())
            .and_then(|seen| {
                seen.iter().copied().find(|&i| {
                    rows.get(i)
                        .is_some_and(|r| mirrored(r.key.project(), t.key.project()))
                })
            });
        match copy_of.and_then(|i| rows.get_mut(i)) {
            Some(kept) => merge(kept, t),
            None => {
                by_summary.entry(norm).or_default().push(rows.len());
                rows.push(t);
            }
        }
    }
    rows
}

/// Folds a mirror copy into its row: the more severe rating (with where it
/// came from), the more recent activity, and any link the row lacks.
fn merge(kept: &mut Ticket, copy: Ticket) {
    if copy
        .severity_level()
        .is_some_and(|s| kept.severity_level().is_none_or(|k| s < k))
    {
        kept.severity = copy.severity;
    }
    if copy
        .idle_days
        .is_some_and(|c| kept.idle_days.is_none_or(|k| c < k))
    {
        kept.last_comment = copy.last_comment;
        kept.idle_days = copy.idle_days;
    }
    for link in copy.links {
        if !kept.links.iter().any(|l| l.key == link.key) {
            kept.links.push(link);
        }
    }
    kept.also.push(copy.key);
}

/// The ticket with a severity taken from its priority, when it has none and
/// its project rates by priority (`priority_severity`: Blocker and Critical as S1, Major
/// as S2 for GCP, NVDCSE and NCP).
pub(crate) fn with_priority_severity(cfg: &Config, mut t: Ticket) -> Ticket {
    let rule = &cfg.priority_severity;
    if t.severity.is_none() && rule.projects.iter().any(|p| p == t.key.project()) {
        let rated = t.priority.as_deref().and_then(|p| {
            rule.levels
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(p))
                .map(|(_, level)| Severity::from_priority(*level, p))
        });
        if rated.is_some() {
            t.severity = rated;
        }
    }
    t
}

/// Days since the newest human comment, or since creation when no person
/// ever commented. Only meaningful once the comments were read.
pub(crate) fn idle_days(t: &Ticket, today: NaiveDate) -> Option<i64> {
    t.last_comment
        .as_ref()
        .map(|c| c.date)
        .or(t.created)
        .map(|d| (today - d).num_days())
}

/// Today plus the time the weighted backlog needs at the current close rate.
pub(crate) fn projected(today: NaiveDate, weighted: f64, per_week: f64) -> Option<NaiveDate> {
    if weighted <= 0.0 {
        return Some(today);
    }
    if per_week <= 0.0 {
        return None;
    }
    // The epsilon keeps a sum like 20 x 0.3 (5.999...) from losing a whole day.
    // A projection past the end of the calendar is none.
    Duration::try_days((weighted / per_week * 7.0 + 1e-6).floor() as i64)
        .and_then(|span| today.checked_add_signed(span))
}

/// "9 May 2027".
pub(crate) fn fmt_date(d: NaiveDate) -> String {
    d.format("%-d %b %Y").to_string()
}

/// The ticket's priority is `name`, ignoring case.
pub(crate) fn has_priority(t: &Ticket, name: &str) -> bool {
    t.priority
        .as_deref()
        .is_some_and(|p| p.eq_ignore_ascii_case(name))
}

/// Where the ticket's priority sorts: Blocker, Critical, Major, then the rest.
pub(crate) fn priority_rank(t: &Ticket) -> usize {
    PRIORITY_ORDER
        .iter()
        .position(|p| has_priority(t, p))
        .unwrap_or(PRIORITY_ORDER.len())
}

/// The workflow status says the ticket is blocked ("Blocked", "Blocked External").
pub(crate) fn is_blocked_status(t: &Ticket) -> bool {
    t.status.to_lowercase().contains(BLOCKED_WORD)
}

/// Test cases carry Blocker/Critical as test priority, not as a statement
/// about the work, so release blocker rules skip them.
pub(crate) fn is_test_case(t: &Ticket) -> bool {
    t.issue_type.eq_ignore_ascii_case(TEST_CASE)
}

/// The status or label that says the ticket's fix awaits verification, as
/// the ticket writes it.
pub(crate) fn verification(cfg: &Config, t: &Ticket) -> Option<String> {
    let listed = |list: &[String], text: &str| list.iter().any(|s| s.eq_ignore_ascii_case(text));
    if listed(&cfg.verification_statuses, &t.status) {
        return Some(t.status.clone());
    }
    t.labels
        .iter()
        .find(|l| listed(&cfg.verification_labels, l))
        .cloned()
}

/// The ticket's fixes: linked `fix_projects` tickets, except through a relation
/// in `non_fix_relations`.
pub(crate) fn fix_keys<'a>(
    cfg: &'a Config,
    t: &'a Ticket,
) -> impl Iterator<Item = &'a IssueKey> + 'a {
    t.links
        .iter()
        .filter(|l| {
            cfg.fix_projects.iter().any(|p| p == l.key.project())
                && cfg.is_fix_relation(&l.relation)
        })
        .map(|l| &l.key)
}

fn blocked_by(t: &Ticket) -> Option<&str> {
    t.links
        .iter()
        .find(|l| l.relation.trim().eq_ignore_ascii_case(IS_BLOCKED_BY) && !l.done)
        .map(|l| l.key.as_str())
}

/// The ticket has been idle longer than `days`, or its idle time is unknown.
fn idle_over(t: &Ticket, days: i64) -> bool {
    t.idle_days.is_none_or(|i| i > days)
}

/// Everything needed to judge one dated piece of work.
pub(crate) struct TrackInput<'a> {
    /// Display name.
    pub(crate) name: String,
    /// Its open items.
    pub(crate) items: Vec<&'a Ticket>,
    /// Items closed during the closing window.
    pub(crate) closed_in_window: usize,
    /// Date on the PM status page.
    pub(crate) pm_date: Option<NaiveDate>,
    /// The PM status page.
    pub(crate) pm: Option<PmRef>,
    /// A PM status page describes this work but could not be read.
    pub(crate) pm_unread: bool,
    /// Date in Jira.
    pub(crate) jira_date: Option<NaiveDate>,
    /// Start date in Jira.
    pub(crate) start_date: Option<NaiveDate>,
    /// The release date agreed outside Jira and the PM page (`release_dates`).
    pub(crate) agreed_date: Option<NaiveDate>,
}

/// Projects the work and judges it against the agreed release date, else
/// the PM page date, else the Jira date.
pub(crate) fn evaluate_track(cfg: &Config, today: NaiveDate, input: TrackInput<'_>) -> Track {
    let open = input.items.len();
    let weighted: f64 = input
        .items
        .iter()
        .map(|t| cfg.weight(&t.status, t.category))
        .sum();
    let per_week = input.closed_in_window as f64 * 7.0 / cfg.closed_window_days as f64;
    let projection = projected(today, weighted, per_week);
    let (ref_date, ref_source) = match (input.agreed_date, input.pm_date, input.jira_date) {
        (Some(d), _, _) => (Some(d), Some(DateSource::Agreed)),
        (None, Some(d), _) => (Some(d), Some(DateSource::PmPage)),
        (None, None, Some(d)) => (Some(d), Some(DateSource::Jira)),
        (None, None, None) => (None, None),
    };
    let gap_days = input
        .pm_date
        .zip(input.jira_date)
        .map(|(p, j)| (p - j).num_days().abs());
    let projection_text = match projection {
        Some(p) => format!("projection: {}", fmt_date(p)),
        None => format!(
            "projection: none, nothing closed in {} days",
            cfg.closed_window_days
        ),
    };
    let source_text = match ref_source {
        Some(DateSource::Agreed) => "agreed date",
        Some(DateSource::PmPage) => "PM page",
        Some(DateSource::Jira) | None => "Jira",
    };
    let mut per_item = false;

    let (mut verdict, mut reason) = if open == 0 {
        (Verdict::OnTrack, "Nothing open".to_string())
    } else if let Some(start) = input
        .start_date
        .filter(|s| *s > today && input.closed_in_window == 0)
    {
        (Verdict::NotStarted, format!("Starts {}", fmt_date(start)))
    } else if let Some(r) = ref_date {
        if r < today {
            (
                Verdict::OffTrack,
                format!("Target {} passed with {open} open", fmt_date(r)),
            )
        } else if open <= cfg.small_release_max {
            per_item = true;
            let days_left = (r - today).num_days();
            let at_risk = input
                .items
                .iter()
                .filter(|t| {
                    days_left <= cfg.risk_window_days
                        && (t.category == Category::New || idle_over(t, cfg.idle_alert_days))
                })
                .count();
            let v = if at_risk > 0 {
                Verdict::AtRisk
            } else {
                Verdict::OnTrack
            };
            (
                v,
                format!("Judged per item: {at_risk} of {open} at risk ({projection_text})"),
            )
        } else {
            match projection {
                None => (
                    Verdict::OffTrack,
                    format!("Nothing closed in the last {} days", cfg.closed_window_days),
                ),
                Some(p) => {
                    let late = (p - r).num_days();
                    let v = if late <= 0 {
                        Verdict::OnTrack
                    } else if late <= cfg.late_tolerance_days {
                        Verdict::AtRisk
                    } else {
                        Verdict::OffTrack
                    };
                    let days = |n: i64| {
                        if n == 1 {
                            "1 day".to_string()
                        } else {
                            format!("{n} days")
                        }
                    };
                    let how = if late <= 0 {
                        format!("{} early", days(-late))
                    } else {
                        format!("{} late", days(late))
                    };
                    (
                        v,
                        format!(
                            "Projected {} against {} ({source_text}), {how}",
                            fmt_date(p),
                            fmt_date(r)
                        ),
                    )
                }
            }
        }
    } else {
        (
            Verdict::NoDate,
            "No date in Jira or on the PM page".to_string(),
        )
    };

    let gap_flagged = gap_days.is_some_and(|g| g > cfg.date_gap_days);
    if let Some(gap) = gap_days.filter(|_| gap_flagged) {
        // Two dates for the same work a week or more apart: the plan itself is
        // unsettled, unless a date was agreed; then the two are only out of date.
        if verdict == Verdict::OnTrack && open > 0 && ref_source != Some(DateSource::Agreed) {
            verdict = Verdict::AtRisk;
        }
        reason = format!("{reason}. PM page and Jira dates are {gap} days apart");
    }
    if input.pm_unread {
        reason = format!("{reason}. PM page not read");
    }

    Track {
        name: input.name,
        open,
        weighted: (weighted * 10.0).round() / 10.0,
        closed_per_week: (per_week * 100.0).round() / 100.0,
        projected: projection,
        pm_date: input.pm_date,
        pm: input.pm,
        pm_unread: input.pm_unread,
        jira_date: input.jira_date,
        start_date: input.start_date,
        ref_date,
        ref_source,
        gap_days,
        gap_flagged,
        verdict,
        reason,
        per_item,
    }
}

/// A customer ticket's target date: the committed date, else the release date
/// of its linked fix.
pub(crate) fn target(t: &Ticket) -> Option<(NaiveDate, TargetSource)> {
    match (&t.committed, &t.fix) {
        (Some(c), _) => Some((c.date, TargetSource::Committed)),
        (None, Some(f)) => Some((f.date, TargetSource::LinkedFix)),
        (None, None) => None,
    }
}

/// Groups dated customer tickets by target date and judges each group.
pub(crate) fn date_groups(cfg: &Config, today: NaiveDate, tickets: &[&Ticket]) -> Vec<DateGroup> {
    let mut by_date: BTreeMap<NaiveDate, Vec<&Ticket>> = BTreeMap::new();
    for t in tickets {
        if let Some((date, _)) = target(t) {
            by_date.entry(date).or_default().push(t);
        }
    }
    by_date
        .into_iter()
        .map(|(date, group)| {
            let days_left = (date - today).num_days();
            // Only a group inside the risk window can be at risk; its
            // stalled tickets are the reason.
            let stalled: Vec<IssueKey> = if (0..=cfg.risk_window_days).contains(&days_left) {
                group
                    .iter()
                    .filter(|t| {
                        // Work on a linked fix counts as started, whatever the
                        // customer ticket's own status says.
                        let not_started = t.category == Category::New
                            && matches!(t.fix_state, FixState::NoFix | FixState::NotStarted);
                        not_started || idle_over(t, cfg.idle_alert_days)
                    })
                    .map(|t| t.key.clone())
                    .collect()
            } else {
                Vec::new()
            };
            let (verdict, reason) = if days_left < 0 {
                (Verdict::OffTrack, Some("Date passed".to_string()))
            } else if stalled.is_empty() {
                (Verdict::OnTrack, None)
            } else {
                let n = stalled.len();
                (Verdict::AtRisk, Some(format!("{n} not started or idle")))
            };
            let mut status: BTreeMap<&str, usize> = BTreeMap::new();
            for t in &group {
                *status.entry(t.status.as_str()).or_default() += 1;
            }
            DateGroup {
                date,
                days_left,
                keys: group.iter().map(|t| t.key.clone()).collect(),
                by_status: status
                    .into_iter()
                    .map(|(s, count)| StatusCount {
                        status: s.to_string(),
                        count,
                    })
                    .collect(),
                verdict,
                reason,
                stalled,
            }
        })
        .collect()
}

/// Customer health from severity and idle time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SeverityIdle {
    /// Off track when an S1/S2 ticket is idle longer than `idle_offtrack_days`;
    /// at risk when one is past its idle limit or its idle time is unknown;
    /// "no date" without S1/S2 tickets.
    pub(crate) verdict: Verdict,
    /// S1/S2 tickets past their idle limit.
    pub(crate) past_limit: usize,
    /// Of those, the ones idle longer than `idle_offtrack_days`.
    pub(crate) off_track: usize,
    /// S1/S2 tickets whose idle time could not be read.
    pub(crate) unknown: usize,
}

/// Judges the S1/S2 tickets by idle time.
pub(crate) fn severity_idle(cfg: &Config, tickets: &[&Ticket]) -> SeverityIdle {
    let (mut rated, mut past_limit, mut off_track, mut unknown) = (0, 0, 0, 0);
    for t in tickets {
        let Some((_, limit)) = idle_limit(cfg, t) else {
            continue;
        };
        rated += 1;
        match t.idle_days {
            None => unknown += 1,
            Some(idle) if idle > limit => {
                past_limit += 1;
                if idle > cfg.idle_offtrack_days {
                    off_track += 1;
                }
            }
            Some(_) => {}
        }
    }
    let verdict = if off_track > 0 {
        Verdict::OffTrack
    } else if past_limit > 0 || unknown > 0 {
        Verdict::AtRisk
    } else if rated > 0 {
        Verdict::OnTrack
    } else {
        Verdict::NoDate
    };
    SeverityIdle {
        verdict,
        past_limit,
        off_track,
        unknown,
    }
}

/// Why a ticket is a blocker, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Rule {
    /// An S1 ticket past its idle limit, or with unknown idle time.
    S1Idle,
    /// An S2 ticket past its idle limit, or with unknown idle time.
    S2Idle,
    /// A date promised to the customer has passed.
    CommittedPassed,
    /// Priority Blocker.
    BlockerPriority,
    /// A "Blocked" workflow status.
    BlockedStatus,
    /// An open "is blocked by" link.
    BlockedBy,
    /// A fix release date or a due date has passed.
    DatePassed,
}

fn blocker(cfg: &Config, t: &Ticket, reason: String, tone: Tone) -> Blocker {
    Blocker {
        key: t.key.clone(),
        url: t.url.clone(),
        summary: t.summary.clone(),
        owner: t.assignee.clone(),
        reason,
        tone,
        verification: verification(cfg, t),
    }
}

/// Orders blockers by rule, then longest idle first (unknown idle first of all).
fn ranked(mut found: Vec<(Rule, Option<i64>, Blocker)>) -> Vec<Blocker> {
    found.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.unwrap_or(i64::MAX).cmp(&a.1.unwrap_or(i64::MAX)))
    });
    found.into_iter().map(|(_, _, b)| b).collect()
}

/// "idle 8d", or "idle unknown".
fn idle_text(idle: Option<i64>) -> String {
    idle.map_or_else(|| "idle unknown".to_string(), |i| format!("idle {i}d"))
}

/// The blocker rule and idle limit of an S1 or S2 ticket.
fn idle_limit(cfg: &Config, t: &Ticket) -> Option<(Rule, i64)> {
    match t.severity_level()? {
        1 => Some((Rule::S1Idle, cfg.idle_s1_days)),
        2 => Some((Rule::S2Idle, cfg.idle_s2_days)),
        _ => None,
    }
}

/// Customer blockers, most urgent first.
pub(crate) fn customer_blockers(
    cfg: &Config,
    today: NaiveDate,
    tickets: &[&Ticket],
) -> Vec<Blocker> {
    let found = tickets
        .iter()
        .filter_map(|t| {
            let idle = t.idle_days;
            let passed = target(t)
                .filter(|(d, _)| *d < today)
                .map(|(_, source)| source);
            let (rule, reason, tone) = if let Some((rule, limit)) = idle_limit(cfg, t)
                && let Some(severity) = &t.severity
                && idle_over(t, limit)
            {
                (
                    rule,
                    format!("{}, {}", severity.label(), idle_text(idle)),
                    Tone::Danger,
                )
            } else if passed == Some(TargetSource::Committed) {
                (
                    Rule::CommittedPassed,
                    "Committed date passed".into(),
                    Tone::Danger,
                )
            } else if has_priority(t, BLOCKER) {
                let idle_note = idle.map(|i| format!(", idle {i}d")).unwrap_or_default();
                (
                    Rule::BlockerPriority,
                    format!("{BLOCKER}{idle_note}"),
                    Tone::Warning,
                )
            } else if is_blocked_status(t) {
                (Rule::BlockedStatus, t.status.clone(), Tone::Warning)
            } else if let Some(k) = blocked_by(t) {
                (Rule::BlockedBy, format!("Blocked by {k}"), Tone::Warning)
            } else if passed == Some(TargetSource::LinkedFix) {
                (Rule::DatePassed, "Fix date passed".into(), Tone::Warning)
            } else {
                return None;
            };
            Some((rule, idle, blocker(cfg, t, reason, tone)))
        })
        .collect();
    ranked(found)
}

/// Release blockers, most urgent first. Test cases are skipped.
pub(crate) fn release_blockers(
    cfg: &Config,
    today: NaiveDate,
    tickets: &[&Ticket],
) -> Vec<Blocker> {
    let found = tickets
        .iter()
        .filter(|t| !is_test_case(t))
        .filter_map(|t| {
            let idle = t.idle_days;
            let (rule, reason, tone) = if has_priority(t, BLOCKER) {
                let idle_note = idle
                    .filter(|i| *i > cfg.idle_alert_days)
                    .map(|i| format!(", idle {i}d"))
                    .unwrap_or_default();
                (
                    Rule::BlockerPriority,
                    format!("{BLOCKER}{idle_note}"),
                    Tone::Danger,
                )
            } else if is_blocked_status(t) {
                (Rule::BlockedStatus, t.status.clone(), Tone::Warning)
            } else if let Some(k) = blocked_by(t) {
                (Rule::BlockedBy, format!("Blocked by {k}"), Tone::Warning)
            } else {
                let due = t.due.filter(|d| *d < today)?;
                (
                    Rule::DatePassed,
                    format!("Past due {}d", (today - due).num_days()),
                    Tone::Warning,
                )
            };
            Some((rule, idle, blocker(cfg, t, reason, tone)))
        })
        .collect();
    ranked(found)
}

/// Release items a rule needs idle time for.
pub(crate) fn needs_idle(t: &Ticket, today: NaiveDate) -> bool {
    !is_test_case(t)
        && (has_priority(t, BLOCKER)
            || is_blocked_status(t)
            || blocked_by(t).is_some()
            || t.due.is_some_and(|d| d < today))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{Comment, Committed, FixRef, Instance, LinkRef};
    use test_case::test_case;
    use url::Url;

    pub(crate) fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    pub(crate) fn today() -> NaiveDate {
        d(2026, 10, 21)
    }

    /// A ticket with neutral fields; tests change what they exercise. Its
    /// idle time is unknown, as for a ticket whose comments were not read.
    pub(crate) fn ticket(key: &str, summary: &str) -> Ticket {
        let key: IssueKey = key.parse().unwrap();
        Ticket {
            url: Url::parse(&format!("https://jira.example/browse/{key}")).unwrap(),
            key,
            instance: Instance::Cloud,
            summary: summary.into(),
            status: "Open".into(),
            category: Category::New,
            priority: Some("Medium".into()),
            severity: None,
            assignee: None,
            issue_type: "Bug".into(),
            created: Some(d(2026, 10, 1)),
            updated: "2026-10-01T00:00:00.000+0000".into(),
            due: None,
            fix_versions: vec![],
            site: None,
            links: vec![],
            last_comment: None,
            idle_days: None,
            also: vec![],
            source: None,
            fix: None,
            fix_state: FixState::NoFix,
            committed: None,
            patches: vec![],
            labels: vec![],
            verification: None,
        }
    }

    /// The ticket with a known idle time.
    pub(crate) fn idle(mut t: Ticket, days: i64) -> Ticket {
        t.idle_days = Some(days);
        t
    }

    pub(crate) fn committed(key: &str, date: NaiveDate) -> Committed {
        Committed {
            date,
            note: None,
            key: key.parse().unwrap(),
        }
    }

    fn fix(date: NaiveDate, category: Category) -> FixRef {
        FixRef {
            key: "EX-1".parse().unwrap(),
            url: Url::parse("https://jira.example/browse/EX-1").unwrap(),
            version: "ES6.3.10".into(),
            date,
            status: "To Do".into(),
            category,
        }
    }

    fn link(key: &str, relation: &str, done: bool) -> LinkRef {
        LinkRef {
            key: key.parse().unwrap(),
            relation: relation.into(),
            done,
        }
    }

    #[test_case("Awaiting Verification", &[], Some("Awaiting Verification") ; "status, any case")]
    #[test_case("In Progress", &["nvidia", "Awaiting_Validation"], Some("Awaiting_Validation") ; "label, any case")]
    #[test_case("Test Ready", &["nvidia"], None ; "other verify statuses and labels")]
    fn verification_comes_from_the_status_or_a_label(
        status: &str,
        labels: &[&str],
        expected: Option<&str>,
    ) {
        let mut t = ticket("NVDCSE-250", "Slow recovery");
        t.status = status.into();
        t.labels = labels.iter().map(ToString::to_string).collect();

        assert_eq!(verification(&Config::default(), &t).as_deref(), expected);
    }

    #[test_case("DDN-7162", "Case 01349748 | NVIDIA | AI400X3i-BF3| HCAs missing", None, Bucket::Nvidia ; "case title names NVIDIA")]
    #[test_case("DDN-7096", "Case #01344810 | Nvidia - Internal | GDS checksum", None, Bucket::Nvidia ; "NVIDIA department")]
    #[test_case("DDN-6679", "Nvidia GPUDirect Storage Installation Guide", None, Bucket::Other ; "technology name is not the customer")]
    #[test_case("DDN-7175", "EMF Grafana fails to start", Some("Nvidia - DGXC (*aka NGC)"), Bucket::Nvidia ; "site field")]
    #[test_case("DDN-7054", "GCP / Google - 543851784 - NodeUnschedulable", None, Bucket::Gcp ; "GCP case title")]
    #[test_case("DDN-5885", "GCP - P1 (406563983) [TEST] Test Support Case - P1", None, Bucket::Test ; "test ticket")]
    #[test_case("NVDCSE-246", "Client recovery stops choosing a node", None, Bucket::Nvidia ; "NVIDIA project")]
    #[test_case("NP-131", "OCI-IAD evictions", None, Bucket::Nvidia ; "NCP copy project")]
    #[test_case("GCP-315", "Client hung in D-state", None, Bucket::Gcp ; "GCP project")]
    #[test_case("DDN-6334", "Exa Encryption with K8S/CSI", Some("Together.ai"), Bucket::Other ; "other customer")]
    fn buckets_follow_project_site_and_case_title(
        key: &str,
        summary: &str,
        site: Option<&str>,
        expected: Bucket,
    ) {
        let classifier = Classifier::new(&Config::default()).unwrap();
        let mut t = ticket(key, summary);
        t.site = site.map(String::from);

        assert_eq!(classifier.bucket(&t), expected);
    }

    fn keys(rows: &[Ticket]) -> Vec<&str> {
        rows.iter().map(|t| t.key.as_str()).collect()
    }

    #[test]
    fn dedup_merges_mirror_copies_only() {
        let rows = dedup(
            vec![
                ticket(
                    "NCP-163",
                    "OCI-IAD:c419n1 - Production   Lustre client evictions",
                ),
                ticket(
                    "NP-131",
                    "oci-iad:c419n1 - production lustre client evictions",
                ),
                ticket("GCP-315", "Client hung in D-state"),
                ticket("DDN-7000", "Client hung in D-state"),
                ticket("DDN-1", "Same title"),
                ticket("DDN-2", "Same title"),
                ticket("NCP-3", ""),
                ticket("NP-3", ""),
            ],
            &Config::default().mirror_projects,
        );

        assert_eq!(
            keys(&rows),
            [
                "NCP-163", "GCP-315", "DDN-7000", "DDN-1", "DDN-2", "NCP-3", "NP-3"
            ]
        );
        assert_eq!(
            rows.first().map(|t| t.also.clone()),
            Some(vec!["NP-131".parse().unwrap()])
        );
    }

    #[test]
    fn a_copy_finds_its_row_past_a_same_titled_ticket_of_another_customer() {
        let rows = dedup(
            vec![
                ticket("NCP-1", "Client hung"),
                ticket("DDN-5", "Client hung"),
                ticket("NP-1", "Client hung"),
            ],
            &Config::default().mirror_projects,
        );

        assert_eq!(keys(&rows), ["NCP-1", "DDN-5"]);
    }

    #[test]
    fn a_merged_row_takes_the_worse_severity_the_newer_comment_and_the_missing_links() {
        let mut ncp = idle(ticket("NCP-163", "evictions"), 30);
        ncp.links = vec![link("EX-1", "is related to", false)];
        let mut np = idle(ticket("NP-131", "evictions"), 2);
        np.severity = Some(Severity::field(2));
        np.last_comment = Some(Comment {
            date: d(2026, 10, 19),
            author: Some("Jason".into()),
        });
        np.links = vec![
            link("EX-1", "is related to", false),
            link("EX-2", "is related to", false),
        ];

        let rows = dedup(vec![ncp, np], &Config::default().mirror_projects);

        let row = rows.first().unwrap();
        assert_eq!((row.severity_level(), row.idle_days), (Some(2), Some(2)));
        assert_eq!(
            row.last_comment.as_ref().and_then(|c| c.author.as_deref()),
            Some("Jason")
        );
        assert_eq!(
            row.links.iter().map(|l| l.key.as_str()).collect::<Vec<_>>(),
            ["EX-1", "EX-2"]
        );
    }

    #[test]
    fn a_merged_row_keeps_where_its_severity_came_from() {
        let cfg = Config::default();
        let rated = |key: &str, priority: &str, severity: Option<Severity>| {
            let mut t = ticket(key, "evictions");
            (t.priority, t.severity) = (Some(priority.into()), severity);
            with_priority_severity(&cfg, t)
        };
        let label = |rows: Vec<Ticket>| {
            rows.first()
                .and_then(|t| t.severity.as_ref())
                .map(Severity::label)
        };

        let real_s1 = dedup(
            vec![
                rated("NCP-1", "Major", None),
                rated("NP-1", "Major", Some(Severity::field(1))),
            ],
            &cfg.mirror_projects,
        );
        let copy_rated = dedup(
            vec![
                rated("NCP-2", "Medium", None),
                rated("NP-2", "Critical", None),
            ],
            &cfg.mirror_projects,
        );

        assert_eq!(label(real_s1), Some("S1".into()));
        assert_eq!(label(copy_rated), Some("S1 (Critical)".into()));
    }

    #[test_case(171.6, 6.0, Some(d(2027, 5, 9)) ; "6.3.10 on 21 Oct")]
    #[test_case(6.0, 7.0, Some(d(2026, 10, 27)) ; "float sum does not lose a day")]
    #[test_case(3.0, 0.0, None ; "nothing closed")]
    #[test_case(0.0, 0.0, Some(d(2026, 10, 21)) ; "nothing left")]
    #[test_case(1e18, 1e-9, None ; "past the end of the calendar")]
    fn projection(weighted: f64, per_week: f64, expected: Option<NaiveDate>) {
        assert_eq!(projected(today(), weighted, per_week), expected);
    }

    fn open_items(n: usize, status: &str, category: Category) -> Vec<Ticket> {
        (1..=n)
            .map(|i| {
                let mut t = ticket(&format!("EX-{i}"), "x");
                t.status = status.into();
                t.category = category;
                t
            })
            .collect()
    }

    fn input(
        items: &[Ticket],
        closed: usize,
        pm: Option<NaiveDate>,
        jira: Option<NaiveDate>,
    ) -> TrackInput<'_> {
        TrackInput {
            name: "R".into(),
            items: items.iter().collect(),
            closed_in_window: closed,
            pm_date: pm,
            pm: None,
            pm_unread: false,
            jira_date: jira,
            start_date: None,
            agreed_date: None,
        }
    }

    /// More items than a small release holds, so the projection decides.
    fn large(status: &str, category: Category) -> Vec<Ticket> {
        open_items(20, status, category)
    }

    // 20 items "In Review" weigh 6.0; 28 closed in 28 days is 7 a week, so the projection is 27 Oct.
    #[test_case(28, Some(d(2026, 11, 30)), Verdict::OnTrack, "Projected 27 Oct 2026 against 30 Nov 2026 (PM page), 34 days early" ; "early")]
    #[test_case(28, Some(d(2026, 10, 28)), Verdict::OnTrack, "Projected 27 Oct 2026 against 28 Oct 2026 (PM page), 1 day early" ; "one day early")]
    #[test_case(28, Some(d(2026, 10, 22)), Verdict::AtRisk, "Projected 27 Oct 2026 against 22 Oct 2026 (PM page), 5 days late" ; "a little late")]
    #[test_case(28, Some(d(2026, 10, 26)), Verdict::AtRisk, "Projected 27 Oct 2026 against 26 Oct 2026 (PM page), 1 day late" ; "one day late")]
    #[test_case(4, Some(d(2026, 10, 25)), Verdict::OffTrack, "Projected 2 Dec 2026 against 25 Oct 2026 (PM page), 38 days late" ; "far too late")]
    #[test_case(28, Some(d(2026, 10, 20)), Verdict::OffTrack, "Target 20 Oct 2026 passed with 20 open" ; "date passed")]
    #[test_case(0, Some(d(2027, 1, 1)), Verdict::OffTrack, "Nothing closed in the last 28 days" ; "nothing closed")]
    #[test_case(5, None, Verdict::NoDate, "No date in Jira or on the PM page" ; "no date")]
    fn verdict_thresholds(closed: usize, pm: Option<NaiveDate>, expected: Verdict, reason: &str) {
        let items = large("In Review", Category::Indeterminate);
        assert!(items.len() > Config::default().small_release_max);

        let track = evaluate_track(&Config::default(), today(), input(&items, closed, pm, None));

        assert_eq!((track.verdict, track.reason.as_str()), (expected, reason));
    }

    #[test]
    fn an_agreed_date_outranks_the_pm_page_and_jira_and_settles_their_gap() {
        let items = large("In Review", Category::Indeterminate);
        // 39 days apart, so a gap flag.
        let (pm, jira) = (Some(d(2026, 10, 22)), Some(d(2026, 11, 30)));
        let judge = |agreed_date| {
            let input = TrackInput {
                agreed_date,
                ..input(&items, 28, pm, jira)
            };
            evaluate_track(&Config::default(), today(), input)
        };

        let agreed = judge(Some(d(2026, 11, 2)));
        let without = judge(None);

        assert_eq!(
            (agreed.ref_source, agreed.ref_date, agreed.verdict),
            (
                Some(DateSource::Agreed),
                Some(d(2026, 11, 2)),
                Verdict::OnTrack
            )
        );
        assert_eq!(
            agreed.reason,
            "Projected 27 Oct 2026 against 2 Nov 2026 (agreed date), 6 days early. PM page and Jira dates are 39 days apart"
        );
        assert!(
            agreed.gap_flagged,
            "the out-of-date sources are still pointed out"
        );
        assert_eq!(
            (without.ref_source, without.verdict),
            (Some(DateSource::PmPage), Verdict::AtRisk)
        );
    }

    #[test]
    fn the_jira_date_is_used_when_there_is_no_pm_date() {
        let items = large("In Review", Category::Indeterminate);

        let track = evaluate_track(
            &Config::default(),
            today(),
            input(&items, 28, None, Some(d(2026, 11, 30))),
        );

        assert_eq!(
            (track.ref_source, track.ref_date),
            (Some(DateSource::Jira), Some(d(2026, 11, 30)))
        );
        assert_eq!(
            track.reason,
            "Projected 27 Oct 2026 against 30 Nov 2026 (Jira), 34 days early"
        );
    }

    #[test]
    fn a_date_gap_turns_on_track_into_at_risk_and_the_pm_date_wins() {
        let items = large("In Review", Category::Indeterminate);

        let track = evaluate_track(
            &Config::default(),
            today(),
            input(&items, 28, Some(d(2027, 1, 18)), Some(d(2026, 12, 15))),
        );

        assert_eq!(
            (track.ref_source, track.ref_date, track.gap_days),
            (Some(DateSource::PmPage), Some(d(2027, 1, 18)), Some(34))
        );
        assert_eq!((track.verdict, track.gap_flagged), (Verdict::AtRisk, true));
        assert_eq!(
            track.reason,
            "Projected 27 Oct 2026 against 18 Jan 2027 (PM page), 83 days early. PM page and Jira dates are 34 days apart"
        );
    }

    #[test]
    fn an_unread_pm_page_is_named_in_the_reason() {
        let items = large("In Review", Category::Indeterminate);
        let mut unread = input(&items, 28, None, Some(d(2026, 11, 30)));
        unread.pm_unread = true;

        let track = evaluate_track(&Config::default(), today(), unread);

        assert!(track.pm_unread);
        assert_eq!(
            track.reason,
            "Projected 27 Oct 2026 against 30 Nov 2026 (Jira), 34 days early. PM page not read"
        );
    }

    #[test]
    fn a_date_gap_does_not_put_finished_work_at_risk() {
        let track = evaluate_track(
            &Config::default(),
            today(),
            input(&[], 3, Some(d(2027, 1, 18)), Some(d(2026, 12, 15))),
        );

        assert_eq!((track.verdict, track.gap_flagged), (Verdict::OnTrack, true));
    }

    #[test]
    fn a_small_gap_is_not_flagged() {
        let items = large("In Review", Category::Indeterminate);

        let track = evaluate_track(
            &Config::default(),
            today(),
            input(&items, 28, Some(d(2026, 12, 20)), Some(d(2026, 12, 15))),
        );

        assert_eq!(
            (track.verdict, track.gap_days, track.gap_flagged),
            (Verdict::OnTrack, Some(5), false)
        );
    }

    #[test_case(d(2026, 12, 1), Verdict::OnTrack, "Judged per item: 0 of 2 at risk (projection: none, nothing closed in 28 days)" ; "outside the window")]
    #[test_case(d(2026, 11, 2), Verdict::AtRisk, "Judged per item: 1 of 2 at risk (projection: none, nothing closed in 28 days)" ; "inside the window")]
    fn a_small_release_is_judged_per_item(jira: NaiveDate, expected: Verdict, reason: &str) {
        let mut items: Vec<Ticket> = open_items(2, "In Review", Category::Indeterminate)
            .into_iter()
            .map(|t| idle(t, 1))
            .collect();
        if let Some(second) = items.get_mut(1) {
            second.status = "To Do".into();
            second.category = Category::New;
        }

        let track = evaluate_track(
            &Config::default(),
            today(),
            input(&items, 0, None, Some(jira)),
        );

        assert!(track.per_item);
        assert_eq!((track.verdict, track.reason.as_str()), (expected, reason));
    }

    #[test_case(Some(8), Verdict::AtRisk ; "idle")]
    #[test_case(None, Verdict::AtRisk ; "idle time unknown")]
    #[test_case(Some(1), Verdict::OnTrack ; "fresh")]
    fn idle_or_unknown_items_count_in_the_per_item_judgement(
        idle_days: Option<i64>,
        expected: Verdict,
    ) {
        let mut items = open_items(1, "In Review", Category::Indeterminate);
        if let Some(first) = items.first_mut() {
            first.idle_days = idle_days;
        }

        let track = evaluate_track(
            &Config::default(),
            today(),
            input(&items, 0, None, Some(d(2026, 11, 2))),
        );

        assert_eq!(track.verdict, expected);
    }

    #[test]
    fn a_phase_that_has_not_started_is_not_judged() {
        let items = open_items(10, "New", Category::New);
        let mut not_started = input(&items, 0, None, Some(d(2027, 4, 30)));
        not_started.start_date = Some(d(2027, 2, 1));

        let track = evaluate_track(&Config::default(), today(), not_started);

        assert_eq!(
            (track.verdict, track.reason.as_str()),
            (Verdict::NotStarted, "Starts 1 Feb 2027")
        );
    }

    #[test]
    fn nothing_open_is_on_track() {
        let track = evaluate_track(
            &Config::default(),
            today(),
            input(&[], 3, None, Some(d(2026, 11, 1))),
        );

        assert_eq!(
            (track.verdict, track.reason.as_str()),
            (Verdict::OnTrack, "Nothing open")
        );
    }

    #[test]
    fn a_committed_date_wins_over_the_fix_date() {
        let mut t = ticket("NVDCSE-247", "x");
        t.fix = Some(fix(d(2026, 12, 15), Category::New));

        assert_eq!(target(&t), Some((d(2026, 12, 15), TargetSource::LinkedFix)));

        t.committed = Some(committed("NVDCSE-247", d(2026, 11, 1)));

        assert_eq!(target(&t), Some((d(2026, 11, 1), TargetSource::Committed)));
        assert_eq!(target(&ticket("NVDCSE-1", "x")), None);
    }

    #[test]
    fn date_groups_flag_stalled_tickets_inside_the_window() {
        let cfg = Config::default();
        let mut passed = idle(ticket("NVDCSE-3", "c"), 1);
        passed.fix = Some(fix(d(2026, 10, 1), Category::New));
        let mut soon = idle(ticket("NVDCSE-1", "a"), 1);
        soon.committed = Some(committed("NVDCSE-1", d(2026, 11, 1)));
        let mut soon_moving = idle(ticket("NVDCSE-4", "d"), 1);
        soon_moving.committed = Some(committed("NVDCSE-4", d(2026, 11, 1)));
        soon_moving.category = Category::Indeterminate;
        let mut soon_unknown = ticket("NVDCSE-5", "e");
        soon_unknown.committed = Some(committed("NVDCSE-5", d(2026, 11, 3)));
        soon_unknown.category = Category::Indeterminate;
        let mut later = idle(ticket("NVDCSE-2", "b"), 1);
        later.fix = Some(fix(d(2027, 1, 31), Category::New));
        // Open customer tickets whose fix work has started are not stalled.
        let mut delivered = idle(ticket("NVDCSE-6", "f"), 1);
        delivered.committed = Some(committed("NVDCSE-6", d(2026, 11, 2)));
        delivered.fix_state = FixState::Delivered;
        let mut undated_fix_under_way = idle(ticket("NVDCSE-7", "g"), 1);
        undated_fix_under_way.committed = Some(committed("NVDCSE-7", d(2026, 11, 2)));
        undated_fix_under_way.fix_state = FixState::UnderWay;

        let groups = date_groups(
            &cfg,
            today(),
            &[
                &soon,
                &later,
                &passed,
                &soon_moving,
                &soon_unknown,
                &delivered,
                &undated_fix_under_way,
            ],
        );

        let summary: Vec<(NaiveDate, i64, Verdict, Option<&str>)> = groups
            .iter()
            .map(|g| (g.date, g.days_left, g.verdict, g.reason.as_deref()))
            .collect();
        assert_eq!(
            summary,
            [
                (d(2026, 10, 1), -20, Verdict::OffTrack, Some("Date passed")),
                (
                    d(2026, 11, 1),
                    11,
                    Verdict::AtRisk,
                    Some("1 not started or idle")
                ),
                (d(2026, 11, 2), 12, Verdict::OnTrack, None),
                (
                    d(2026, 11, 3),
                    13,
                    Verdict::AtRisk,
                    Some("1 not started or idle")
                ),
                (d(2027, 1, 31), 102, Verdict::OnTrack, None),
            ]
        );
        let stalled: Vec<Vec<&str>> = groups
            .iter()
            .map(|g| g.stalled.iter().map(IssueKey::as_str).collect())
            .collect();
        assert_eq!(
            stalled,
            [vec![], vec!["NVDCSE-1"], vec![], vec!["NVDCSE-5"], vec![]],
            "only the tickets that put a group at risk are listed"
        );
        assert_eq!(
            groups.get(1).map(|g| g.by_status.clone()),
            Some(vec![StatusCount {
                status: "Open".into(),
                count: 2
            }])
        );
        assert_eq!(
            groups
                .get(1)
                .map(|g| g.keys.iter().map(IssueKey::as_str).collect::<Vec<_>>()),
            Some(vec!["NVDCSE-1", "NVDCSE-4"]),
            "the started ticket is in the group but not among the stalled ones"
        );
    }

    #[test]
    fn idle_s1_s2_set_the_customer_verdict() {
        let cfg = Config::default();
        let rated = |key: &str, severity: u8, idle_days: Option<i64>| {
            let mut t = ticket(key, "x");
            (t.severity, t.idle_days) = (Some(Severity::field(severity)), idle_days);
            t
        };
        let fresh = [rated("DDN-1", 1, Some(2)), rated("DDN-2", 2, Some(7))];
        let s1_late = [rated("DDN-1", 1, Some(8)), rated("DDN-2", 2, Some(7))];
        let s2_month = [rated("DDN-1", 1, Some(8)), rated("DDN-2", 2, Some(31))];
        let unknown = [rated("DDN-1", 1, Some(2)), rated("DDN-2", 2, None)];
        let unrated = [ticket("NVDCSE-1", "x"), rated("DDN-9", 3, Some(400))];

        let judged = |tickets: &[Ticket]| severity_idle(&cfg, &tickets.iter().collect::<Vec<_>>());
        let expect = |verdict, past_limit, off_track, unknown| SeverityIdle {
            verdict,
            past_limit,
            off_track,
            unknown,
        };

        assert_eq!(judged(&fresh), expect(Verdict::OnTrack, 0, 0, 0));
        assert_eq!(judged(&s1_late), expect(Verdict::AtRisk, 1, 0, 0));
        assert_eq!(judged(&s2_month), expect(Verdict::OffTrack, 2, 1, 0));
        assert_eq!(judged(&unknown), expect(Verdict::AtRisk, 0, 0, 1));
        assert_eq!(judged(&unrated), expect(Verdict::NoDate, 0, 0, 0));
    }

    #[test]
    fn customer_blockers_rank_and_explain() {
        let cfg = Config::default();
        let mut s1 = idle(ticket("DDN-1", "a"), 8);
        s1.severity = Some(Severity::field(1));
        let mut s1_unknown = ticket("DDN-8", "a2");
        s1_unknown.severity = Some(Severity::field(1));
        let mut s2 = idle(ticket("DDN-2", "b"), 37);
        s2.severity = Some(Severity::field(2));
        let mut s2_fresh = idle(ticket("DDN-3", "c"), 2);
        s2_fresh.severity = Some(Severity::field(2));
        let mut blocker_priority = idle(ticket("NVDA-1", "d"), 1);
        blocker_priority.priority = Some("Blocker".into());
        let mut blocked_status = idle(ticket("DDN-6", "e"), 1);
        blocked_status.status = "Blocked External".into();
        let mut blocked_link = idle(ticket("DDN-4", "f"), 1);
        blocked_link.links = vec![link("EX-9", "is blocked by", false)];
        let mut late_fix = idle(ticket("DDN-5", "g"), 1);
        late_fix.fix = Some(fix(d(2026, 10, 1), Category::New));
        let mut late_promise = idle(ticket("GCP-1", "h"), 1);
        late_promise.committed = Some(committed("GCP-1", d(2026, 10, 2)));

        let found = customer_blockers(
            &cfg,
            today(),
            &[
                &s2,
                &s2_fresh,
                &s1,
                &blocked_link,
                &late_fix,
                &blocker_priority,
                &blocked_status,
                &late_promise,
                &s1_unknown,
            ],
        );

        let summary: Vec<(&str, &str, Tone)> = found
            .iter()
            .map(|b| (b.key.as_str(), b.reason.as_str(), b.tone))
            .collect();
        assert_eq!(
            summary,
            [
                ("DDN-8", "S1, idle unknown", Tone::Danger),
                ("DDN-1", "S1, idle 8d", Tone::Danger),
                ("DDN-2", "S2, idle 37d", Tone::Danger),
                ("GCP-1", "Committed date passed", Tone::Danger),
                ("NVDA-1", "Blocker, idle 1d", Tone::Warning),
                ("DDN-6", "Blocked External", Tone::Warning),
                ("DDN-4", "Blocked by EX-9", Tone::Warning),
                ("DDN-5", "Fix date passed", Tone::Warning),
            ]
        );
    }

    #[test]
    fn release_blockers_skip_test_cases_and_explain() {
        let cfg = Config::default();
        let mut blocker_idle = idle(ticket("EX-1", "a"), 26);
        blocker_idle.priority = Some("Blocker".into());
        let mut blocker_fresh = idle(ticket("EX-2", "b"), 2);
        blocker_fresh.priority = Some("Blocker".into());
        let mut test_case = ticket("EXM-3", "c");
        (test_case.priority, test_case.issue_type) = (Some("Blocker".into()), "Test".into());
        let mut external = ticket("EX-4", "d");
        external.status = "Blocked External".into();
        let mut linked = ticket("EX-5", "e");
        linked.links = vec![link("EX-6", "is blocked by", false)];
        let mut done_link = ticket("EX-7", "f");
        done_link.links = vec![link("EX-8", "is blocked by", true)];
        let mut overdue = ticket("EX-9", "g");
        overdue.due = Some(d(2026, 10, 16));

        let found = release_blockers(
            &cfg,
            today(),
            &[
                &overdue,
                &external,
                &blocker_fresh,
                &test_case,
                &linked,
                &done_link,
                &blocker_idle,
            ],
        );

        let summary: Vec<(&str, &str)> = found
            .iter()
            .map(|b| (b.key.as_str(), b.reason.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("EX-1", "Blocker, idle 26d"),
                ("EX-2", "Blocker"),
                ("EX-4", "Blocked External"),
                ("EX-5", "Blocked by EX-6"),
                ("EX-9", "Past due 5d")
            ]
        );
    }

    #[test]
    fn fix_keys_skip_escalation_links() {
        let cfg = Config::default();
        let mut t = ticket("DDN-6741", "x");
        t.links = vec![
            link("EX-1", "is related to ", false),
            link("EX-2", "Occurred in Escalation", false),
            link("LU-3", "is related to", false),
            link("EXR-4", "is blocked by", false),
        ];

        let fixes: Vec<&str> = fix_keys(&cfg, &t).map(IssueKey::as_str).collect();

        assert_eq!(fixes, ["EX-1", "EXR-4"]);
    }

    #[test]
    fn priority_rank_orders_blocker_critical_major_then_the_rest() {
        let with = |p: &str| {
            let mut t = ticket("EX-1", "x");
            t.priority = Some(p.into());
            priority_rank(&t)
        };

        assert_eq!(
            [
                with("blocker"),
                with("Critical"),
                with("Major"),
                with("Minor")
            ],
            [0, 1, 2, PRIORITY_ORDER.len()]
        );
    }

    #[test]
    fn needs_idle_only_for_ticket_a_rule_reads() {
        let mut overdue = ticket("EX-1", "a");
        overdue.due = Some(d(2026, 10, 1));
        let mut test_case = ticket("EXM-2", "b");
        (test_case.priority, test_case.issue_type) = (Some("Blocker".into()), "Test".into());

        assert!(needs_idle(&overdue, today()));
        assert!(!needs_idle(&test_case, today()));
        assert!(!needs_idle(&ticket("EX-3", "c"), today()));
    }

    #[test]
    fn idle_counts_from_the_comment_else_creation() {
        let mut t = ticket("DDN-1", "x");

        assert_eq!(idle_days(&t, today()), Some(20));

        t.last_comment = Some(Comment {
            date: d(2026, 10, 19),
            author: Some("Jason".into()),
        });

        assert_eq!(idle_days(&t, today()), Some(2));
    }
}
