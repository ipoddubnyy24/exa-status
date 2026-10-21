//! Types shared by the refresh worker, the rules and the web UI. A snapshot of
//! these is written to `data/latest.json` and handed to the browser unchanged.

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Local, NaiveDate};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use url::Url;

/// Version of the snapshot layout. The server serves a snapshot only when it
/// matches, so a server built before a layout change never feeds its embedded
/// page a snapshot written by a newer worker.
pub(crate) const SNAPSHOT_FORMAT: u32 = 8;

/// A view (sub-tab). Notes name the views they concern, and the UI keeps the
/// selected one across refreshes by its id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "&'static str")]
pub(crate) enum ViewId {
    /// Every customer group together.
    All,
    /// Customers other than GCP and NVIDIA.
    Others,
    /// GCP.
    Gcp,
    /// NVIDIA.
    Nvidia,
    /// The EXA6 releases.
    Exa6,
    /// EXA7 Beta.
    Exa7Beta,
    /// EXA7 GA.
    Exa7Ga,
    /// EXA SD.
    ExaSd,
    /// Future work.
    Future,
}

impl ViewId {
    /// Every view.
    pub(crate) const ALL: [Self; 9] = [
        Self::All,
        Self::Others,
        Self::Gcp,
        Self::Nvidia,
        Self::Exa6,
        Self::Exa7Beta,
        Self::Exa7Ga,
        Self::ExaSd,
        Self::Future,
    ];
    /// The customer views: `All`, which shows every group, and one per group.
    pub(crate) const CUSTOMERS: [Self; 4] = [Self::All, Self::Others, Self::Gcp, Self::Nvidia];
    /// The views of EX releases.
    pub(crate) const RELEASES: [Self; 3] = [Self::Exa6, Self::Exa7Beta, Self::Exa7Ga];

    /// Its sub-tab label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Others => "All others",
            Self::Gcp => "GCP",
            Self::Nvidia => "NVIDIA",
            Self::Exa6 => "EXA6",
            Self::Exa7Beta => "EXA7 Beta",
            Self::Exa7Ga => "EXA7 GA",
            Self::ExaSd => "EXA SD",
            Self::Future => "Future work",
        }
    }

    /// The id the UI knows the view by.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::All => "customers-all",
            Self::Others => "customers-others",
            Self::Gcp => "customers-gcp",
            Self::Nvidia => "customers-nvidia",
            Self::Exa6 => "dev-exa6",
            Self::Exa7Beta => "dev-exa7-beta",
            Self::Exa7Ga => "dev-exa7-ga",
            Self::ExaSd => "dev-exa-sd",
            Self::Future => "future",
        }
    }
}

impl From<ViewId> for &'static str {
    fn from(id: ViewId) -> Self {
        id.as_str()
    }
}

impl TryFrom<String> for ViewId {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|v| v.as_str() == s)
            .ok_or_else(|| anyhow!("{s:?} is not a view"))
    }
}

/// The Jira instance a ticket lives on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Instance {
    /// ime-ddn.atlassian.net: DDN, EX, EXR, EXM, EXAUI, SFAFE, NP.
    Cloud,
    /// jira.whamcloud.com: GCP, NVDCSE, NCP.
    Whamcloud,
}

impl fmt::Display for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cloud => "cloud",
            Self::Whamcloud => "whamcloud",
        })
    }
}

/// Jira's status category, which every workflow status maps to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Category {
    /// Work has not started ("To Do", "Open", "New", ...).
    New,
    /// Work is under way ("In Progress", "In Review", ...).
    Indeterminate,
    /// Finished.
    Done,
}

/// A Jira issue key such as `DDN-7162`, validated when constructed.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub(crate) struct IssueKey(String);

impl IssueKey {
    /// The project part of the key: `DDN` for `DDN-7162`.
    pub(crate) fn project(&self) -> &str {
        self.0
            .split_once('-')
            .map_or(self.0.as_str(), |(project, _)| project)
    }

    /// The key as text.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// A Jira project key such as `DDN` or `E7S`.
pub(crate) fn is_project_key(s: &str) -> bool {
    s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

impl FromStr for IssueKey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let Some((project, number)) = s.split_once('-') else {
            bail!("{s:?} is not a Jira key");
        };
        let number_ok = !number.is_empty()
            && !number.starts_with('0')
            && number.chars().all(|c| c.is_ascii_digit());
        if !(is_project_key(project) && number_ok) {
            bail!("{s:?} is not a Jira key");
        }
        Ok(Self(s.to_string()))
    }
}

impl TryFrom<String> for IssueKey {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        s.parse()
    }
}

impl From<IssueKey> for String {
    fn from(key: IssueKey) -> Self {
        key.0
    }
}

impl fmt::Display for IssueKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One issue link, seen from the ticket that carries it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct LinkRef {
    /// The other ticket.
    pub(crate) key: IssueKey,
    /// The link text from this ticket's side, e.g. "is blocked by".
    pub(crate) relation: String,
    /// Whether the other ticket is done.
    pub(crate) done: bool,
}

/// The linked EX fix whose fixVersion gives a customer ticket its date (EXR
/// fixVersions are planning quarters, not releases).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct FixRef {
    /// The fix ticket.
    pub(crate) key: IssueKey,
    /// Its browse link.
    pub(crate) url: Url,
    /// The fixVersion whose release date is used.
    pub(crate) version: String,
    /// That fixVersion's release date.
    pub(crate) date: NaiveDate,
    /// The fix ticket's workflow status.
    pub(crate) status: String,
    /// The fix ticket's status category.
    pub(crate) category: Category,
}

/// A date promised to a customer, kept in the local commitments file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Committed {
    /// The promised date.
    pub(crate) date: NaiveDate,
    /// Free text kept with the date.
    pub(crate) note: Option<String>,
    /// The key it is filed under: the row's own, or its copy in a mirror
    /// project. Editing the date on the page changes this entry.
    pub(crate) key: IssueKey,
}

/// A comment written by a person.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Comment {
    /// When it was written, in local time.
    pub(crate) date: NaiveDate,
    /// Who wrote it; `None` when Jira names no author.
    pub(crate) author: Option<String>,
}

/// A ticket's severity, 1 (highest) to 4, and where it came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(into = "SeverityShown")]
pub(crate) struct Severity {
    level: u8,
    /// The priority it was taken from, for projects without a severity field.
    from_priority: Option<String>,
}

impl Severity {
    /// A level from DDN's Severity field.
    pub(crate) fn field(level: u8) -> Self {
        Self {
            level,
            from_priority: None,
        }
    }

    /// A level taken from the ticket's priority (`priority_severity`).
    pub(crate) fn from_priority(level: u8, priority: &str) -> Self {
        Self {
            level,
            from_priority: Some(priority.to_string()),
        }
    }

    /// 1 (highest) to 4.
    pub(crate) fn level(&self) -> u8 {
        self.level
    }

    /// The level was taken from the priority.
    pub(crate) fn is_from_priority(&self) -> bool {
        self.from_priority.is_some()
    }

    /// "S1", or "S1 (Critical)" when taken from the priority. The page shows
    /// this text, so the table and the blocker reasons always agree.
    pub(crate) fn label(&self) -> String {
        match &self.from_priority {
            Some(priority) => format!("S{} ({priority})", self.level),
            None => format!("S{}", self.level),
        }
    }
}

/// A severity as written into the snapshot: with its label.
#[derive(Serialize)]
struct SeverityShown {
    level: u8,
    from_priority: Option<String>,
    label: String,
}

impl From<Severity> for SeverityShown {
    fn from(s: Severity) -> Self {
        Self {
            label: s.label(),
            level: s.level,
            from_priority: s.from_priority,
        }
    }
}

/// How far the linked fixes of a customer ticket are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FixState {
    /// No linked fix, or none could be read.
    #[default]
    NoFix,
    /// No linked fix that was read has started.
    NotStarted,
    /// Work on a fix has started: one is under way, or some are done and
    /// others are not (or could not be read).
    UnderWay,
    /// Every linked fix was read and is done, while the ticket is still open.
    Delivered,
}

/// Gerrit's change status.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ChangeStatus {
    /// Open for review.
    New,
    /// Merged into its branch.
    Merged,
    /// Abandoned.
    Abandoned,
}

/// A Gerrit change that mentions a watched ticket.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PatchRef {
    /// Gerrit change number.
    pub(crate) number: u64,
    /// Link to the change.
    pub(crate) url: Url,
    /// Target branch, e.g. `b_es6_0`.
    pub(crate) branch: String,
    /// Review state.
    pub(crate) status: ChangeStatus,
    /// First ddn tag that contains the merged change, e.g. `2.14.0-ddn262`.
    pub(crate) first_tag: Option<String>,
}

/// A Jira ticket with what the dashboard derives about it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Ticket {
    /// The issue key.
    pub(crate) key: IssueKey,
    /// The Jira holding it.
    pub(crate) instance: Instance,
    /// Browse link.
    pub(crate) url: Url,
    /// One-line summary.
    pub(crate) summary: String,
    /// Workflow status name.
    pub(crate) status: String,
    /// Status category.
    pub(crate) category: Category,
    /// Priority name, when the project sets one.
    pub(crate) priority: Option<String>,
    /// From DDN's Severity field, or from the priority where a project has
    /// none (`priority_severity`).
    pub(crate) severity: Option<Severity>,
    /// Assignee display name.
    pub(crate) assignee: Option<String>,
    /// Issue type name.
    pub(crate) issue_type: String,
    /// Creation date.
    pub(crate) created: Option<NaiveDate>,
    /// Jira's last-updated stamp, used only to tell when cached data is stale.
    pub(crate) updated: String,
    /// Due date.
    pub(crate) due: Option<NaiveDate>,
    /// fixVersion names.
    pub(crate) fix_versions: Vec<String>,
    /// "Site Affected" (DDN customer field).
    pub(crate) site: Option<String>,
    /// Issue links.
    pub(crate) links: Vec<LinkRef>,
    /// The newest comment by a person.
    pub(crate) last_comment: Option<Comment>,
    /// Days since that comment, or since creation when no person ever
    /// commented; `None` when the comments were not read.
    pub(crate) idle_days: Option<i64>,
    /// Duplicates (same summary in another project) folded into this row.
    pub(crate) also: Vec<IssueKey>,
    /// Configured customer source the ticket came from, e.g. "DDN".
    pub(crate) source: Option<String>,
    /// Dated linked fix.
    pub(crate) fix: Option<FixRef>,
    /// How far its linked EX/EXR fixes are.
    pub(crate) fix_state: FixState,
    /// Date promised to the customer.
    pub(crate) committed: Option<Committed>,
    /// Gerrit changes for watched tickets.
    pub(crate) patches: Vec<PatchRef>,
}

impl Ticket {
    /// The severity level, 1 (highest) to 4.
    pub(crate) fn severity_level(&self) -> Option<u8> {
        self.severity.as_ref().map(Severity::level)
    }
}

/// How well something is going. Ordered so `max()` gives the worst; "no date"
/// and "not started" never outrank a real verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    /// Nothing dated to judge.
    NoDate,
    /// A phase whose start date is still ahead.
    NotStarted,
    /// On course for its date.
    OnTrack,
    /// Close to slipping.
    AtRisk,
    /// Slipping or slipped.
    OffTrack,
}

/// Emphasis for a number or a blocker reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Tone {
    /// Needs action now.
    Danger,
    /// Worth a look.
    Warning,
}

/// One number in the "Where we are" row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Kpi {
    /// What is counted.
    pub(crate) label: String,
    /// The value as displayed.
    pub(crate) value: String,
    /// Detail under the value.
    pub(crate) note: String,
    /// Emphasis, if any.
    pub(crate) tone: Option<Tone>,
    /// The counted tickets by customer source, which the page offers as
    /// filters of the table; empty on every other tile.
    pub(crate) by_source: Vec<SourceCount>,
}

/// How many of a view's tickets came from one customer source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceCount {
    /// The source's name, as its tickets carry it in `source`.
    pub(crate) source: String,
    /// Tickets from it.
    pub(crate) count: usize,
}

/// Where a track's reference date came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DateSource {
    /// The weekly PM status page in Confluence.
    PmPage,
    /// A Jira fixVersion release date or epic due date.
    Jira,
}

/// What the PM status page says.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct PmRef {
    /// Page title.
    pub(crate) title: String,
    /// Page link.
    pub(crate) url: Url,
    /// Declared status, e.g. "ON TRACK".
    pub(crate) status: Option<String>,
    /// Text of the Target Date cell.
    pub(crate) target: Option<String>,
    /// The page's "Date of Update".
    pub(crate) updated: Option<NaiveDate>,
}

/// A dated piece of work judged as one unit: a release, a phase, an initiative.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Track {
    /// Display name.
    pub(crate) name: String,
    /// Open items.
    pub(crate) open: usize,
    /// Open items weighted by status.
    pub(crate) weighted: f64,
    /// Items closed per week over the closing window.
    pub(crate) closed_per_week: f64,
    /// Projected finish at the current rate.
    pub(crate) projected: Option<NaiveDate>,
    /// Date on the PM status page.
    pub(crate) pm_date: Option<NaiveDate>,
    /// The PM status page itself.
    pub(crate) pm: Option<PmRef>,
    /// A PM status page describes this work but could not be read.
    pub(crate) pm_unread: bool,
    /// Date in Jira.
    pub(crate) jira_date: Option<NaiveDate>,
    /// Start date in Jira.
    pub(crate) start_date: Option<NaiveDate>,
    /// The date the verdict is measured against.
    pub(crate) ref_date: Option<NaiveDate>,
    /// Where `ref_date` came from.
    pub(crate) ref_source: Option<DateSource>,
    /// Days between the PM page and Jira dates.
    pub(crate) gap_days: Option<i64>,
    /// The gap is over `date_gap_days` and is called out.
    pub(crate) gap_flagged: bool,
    /// The judgement.
    pub(crate) verdict: Verdict,
    /// Why, in one sentence.
    pub(crate) reason: String,
    /// Judged item by item rather than by projection.
    pub(crate) per_item: bool,
}

/// How many tickets share a status.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct StatusCount {
    /// Workflow status name.
    pub(crate) status: String,
    /// Tickets in it.
    pub(crate) count: usize,
}

/// Where a customer ticket's date came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TargetSource {
    /// The local commitments file.
    Committed,
    /// The release date of the linked EX fix.
    LinkedFix,
}

/// Customer tickets that share one target date.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct DateGroup {
    /// The target date.
    pub(crate) date: NaiveDate,
    /// Days from today, negative when passed.
    pub(crate) days_left: i64,
    /// Tickets due that day.
    pub(crate) keys: Vec<IssueKey>,
    /// Their statuses.
    pub(crate) by_status: Vec<StatusCount>,
    /// The judgement.
    pub(crate) verdict: Verdict,
    /// Why, when not on track.
    pub(crate) reason: Option<String>,
    /// The tickets `reason` counts when the group is at risk: not started,
    /// or idle over `idle_alert_days` (unknown counts). Empty when on or off
    /// track.
    pub(crate) stalled: Vec<IssueKey>,
}

/// One entry in a blocker list.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Blocker {
    /// The ticket.
    pub(crate) key: IssueKey,
    /// Its browse link.
    pub(crate) url: Url,
    /// Its summary.
    pub(crate) summary: String,
    /// Its assignee.
    pub(crate) owner: Option<String>,
    /// Why it counts as a blocker.
    pub(crate) reason: String,
    /// Emphasis.
    pub(crate) tone: Tone,
}

/// One pass rate on the build-health tile: Lustre sessions with no failure
/// on a branch, or passing EMF nightly runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct HealthPart {
    /// Percent passing; `None` when nothing ran or it could not be read.
    pub(crate) rate: Option<f64>,
    /// Sessions or runs the rate is taken over.
    pub(crate) count: u64,
    /// Runs left out of the rate: cancelled, skipped, timed out or still running.
    pub(crate) excluded: u64,
    /// `count` reached EXA Reporter's limit, so the rate covers the newest
    /// sessions of the window only.
    pub(crate) capped: bool,
    /// Why it could not be read.
    pub(crate) error: Option<String>,
}

/// The build-health tile, read from EXA Reporter. A view shows the parts that
/// concern it; the others are `None`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct BuildHealth {
    /// EXA Reporter's address, linked from the tile.
    pub(crate) url: Url,
    /// Days the numbers cover.
    pub(crate) days: u32,
    /// es6 (b_es6_0) Maloo sessions.
    pub(crate) es6: Option<HealthPart>,
    /// es7 (b_es7_0) Maloo sessions.
    pub(crate) es7: Option<HealthPart>,
    /// EMF nightly runs.
    pub(crate) emf: Option<HealthPart>,
}

impl BuildHealth {
    /// What could not be read, part by part.
    pub(crate) fn errors(&self) -> Vec<String> {
        [("es6", &self.es6), ("es7", &self.es7), ("EMF", &self.emf)]
            .into_iter()
            .filter_map(|(name, part)| {
                part.as_ref()?
                    .error
                    .as_ref()
                    .map(|e| format!("{name}: {e}"))
            })
            .collect()
    }
}

/// An epic in a Future work initiative.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct EpicRef {
    /// The epic.
    pub(crate) key: IssueKey,
    /// Its browse link.
    pub(crate) url: Url,
    /// Its summary.
    pub(crate) summary: String,
    /// Its status.
    pub(crate) status: String,
    /// Its assignee.
    pub(crate) owner: Option<String>,
}

/// One Future work card.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct InitiativeCard {
    /// The dated judgement of the initiative's open work.
    pub(crate) track: Track,
    /// Its epics.
    pub(crate) epics: Vec<EpicRef>,
    /// Its blockers, most urgent first.
    pub(crate) blockers: Vec<Blocker>,
}

/// Which layout the UI uses for a view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ViewKind {
    /// Customer tickets.
    Customers,
    /// Release scope.
    Release,
    /// Future work initiatives.
    Initiatives,
}

/// One sub-tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct View {
    /// Which view this is.
    pub(crate) id: ViewId,
    /// Sub-tab label.
    pub(crate) label: String,
    /// Layout.
    pub(crate) kind: ViewKind,
    /// Overall judgement.
    pub(crate) verdict: Verdict,
    /// The reason for it.
    pub(crate) summary: String,
    /// "Where we are" numbers.
    pub(crate) kpis: Vec<Kpi>,
    /// Dated release work.
    pub(crate) tracks: Vec<Track>,
    /// Dated customer tickets.
    pub(crate) date_groups: Vec<DateGroup>,
    /// Customer tickets without a date.
    pub(crate) undated: usize,
    /// Blockers, most urgent first.
    pub(crate) blockers: Vec<Blocker>,
    /// Every item in the view but Test items.
    pub(crate) items: Vec<Ticket>,
    /// Test items (issue type Test), listed apart from the work.
    pub(crate) tests: Vec<Ticket>,
    /// Warnings about missing or partial data.
    pub(crate) notes: Vec<String>,
    /// PM status page shown for context.
    pub(crate) pm: Option<PmRef>,
    /// Build-health tile.
    pub(crate) health: Option<BuildHealth>,
    /// Future work cards.
    pub(crate) initiatives: Vec<InitiativeCard>,
    /// Whether committed dates can be edited here.
    pub(crate) editable_commitments: bool,
}

impl View {
    /// Every blocker shown in the view: its own list plus those on its cards.
    pub(crate) fn blocker_count(&self) -> usize {
        self.blockers.len()
            + self
                .initiatives
                .iter()
                .map(|c| c.blockers.len())
                .sum::<usize>()
    }
}

/// One top-level tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Tab {
    /// Stable id.
    pub(crate) id: String,
    /// Label.
    pub(crate) label: String,
    /// Worst verdict among its views.
    pub(crate) verdict: Verdict,
    /// Its sub-tabs.
    pub(crate) views: Vec<View>,
}

/// Whether a data source answered everything during the refresh.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceStatus {
    /// Source name.
    pub(crate) name: String,
    /// What went wrong; `None` when every read succeeded.
    pub(crate) error: Option<String>,
}

/// Everything the UI shows, as of one refresh.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    /// Layout version, `SNAPSHOT_FORMAT` when written.
    pub(crate) format: u32,
    /// When the refresh finished.
    pub(crate) generated_at: DateTime<Local>,
    /// The date verdicts were computed for.
    pub(crate) today: NaiveDate,
    /// How long the refresh took.
    pub(crate) duration_secs: f64,
    /// Data source health.
    pub(crate) sources: Vec<SourceStatus>,
    /// The tabs.
    pub(crate) tabs: Vec<Tab>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("DDN-7162", "DDN" ; "plain")]
    #[test_case("NVDCSE-246", "NVDCSE" ; "long project")]
    #[test_case("E7S-12", "E7S" ; "digit in project")]
    fn issue_key_accepts_jira_keys(text: &str, project: &str) {
        let key: IssueKey = text.parse().unwrap();

        assert_eq!(key.project(), project);
        assert_eq!(key.to_string(), text);
    }

    #[test_case("ddn-1" ; "lowercase")]
    #[test_case("DDN-" ; "no number")]
    #[test_case("DDN-01" ; "leading zero")]
    #[test_case("DDN7162" ; "no dash")]
    #[test_case("DDN-12a" ; "letters in number")]
    #[test_case("../etc-1" ; "path")]
    fn issue_key_rejects_other_text(text: &str) {
        let err = text.parse::<IssueKey>().unwrap_err();

        assert_eq!(err.to_string(), format!("{text:?} is not a Jira key"));
    }

    #[test]
    fn issue_key_round_trips_through_json() {
        let key: IssueKey = serde_json::from_str("\"EX-15945\"").unwrap();

        assert_eq!(serde_json::to_string(&key).unwrap(), "\"EX-15945\"");
        let err = serde_json::from_str::<IssueKey>("\"nope\"")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("\"nope\" is not a Jira key"), "{err}");
    }

    #[test_case("\"new\"", Category::New)]
    #[test_case("\"indeterminate\"", Category::Indeterminate)]
    #[test_case("\"done\"", Category::Done)]
    fn category_reads_jira_keys(json: &str, expected: Category) {
        assert_eq!(serde_json::from_str::<Category>(json).unwrap(), expected);
    }

    #[test]
    fn category_rejects_unknown_keys() {
        let err = serde_json::from_str::<Category>("\"undefined\"")
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("unknown variant `undefined`"), "{err}");
    }

    #[test]
    fn verdict_order_puts_the_worst_last() {
        let all = [
            Verdict::OffTrack,
            Verdict::NoDate,
            Verdict::AtRisk,
            Verdict::OnTrack,
            Verdict::NotStarted,
        ];

        assert_eq!(all.iter().max(), Some(&Verdict::OffTrack));
        assert_eq!(
            [Verdict::NoDate, Verdict::OnTrack].iter().max(),
            Some(&Verdict::OnTrack)
        );
    }

    #[test]
    fn instance_displays_as_it_serializes() {
        for instance in [Instance::Cloud, Instance::Whamcloud] {
            assert_eq!(
                serde_json::to_string(&instance).unwrap(),
                format!("\"{instance}\"")
            );
        }
    }

    #[test]
    fn build_health_lists_errors_by_part() {
        let failed = HealthPart {
            error: Some("timed out".into()),
            ..HealthPart::default()
        };
        let health = BuildHealth {
            url: Url::parse("https://localhost:9835/").unwrap(),
            days: 7,
            es6: Some(HealthPart {
                rate: Some(55.8),
                count: 500,
                excluded: 0,
                capped: true,
                error: None,
            }),
            es7: Some(failed),
            emf: None,
        };

        assert_eq!(health.errors(), ["es7: timed out"]);
    }

    #[test]
    fn view_ids_round_trip_through_json() {
        for id in ViewId::ALL {
            let json = serde_json::to_string(&id).unwrap();

            assert_eq!(json, format!("\"{}\"", id.as_str()));
            assert_eq!(serde_json::from_str::<ViewId>(&json).unwrap(), id);
        }
        assert!(serde_json::from_str::<ViewId>("\"dev-exa8\"").is_err());
    }

    #[test]
    fn a_severity_carries_its_label_into_the_snapshot() {
        let rated = Severity::from_priority(1, "Critical");

        assert_eq!(
            serde_json::to_value(&rated).unwrap(),
            serde_json::json!({"level": 1, "from_priority": "Critical", "label": "S1 (Critical)"})
        );
        assert_eq!(Severity::field(2).label(), "S2");
        assert!(rated.is_from_priority() && !Severity::field(2).is_from_priority());
    }
}
