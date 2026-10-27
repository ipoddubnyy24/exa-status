//! Built-in defaults, overridable by `config.local.json` in the working
//! directory (or the file named by `EXA_STATUS_CONFIG`). Only top-level keys
//! that change need to appear there, and a `status_weights` entry overrides
//! just that status. Unknown keys are rejected so a typo fails loudly, and
//! every value is checked at load, before any network I/O.

use anyhow::{Context, Result, bail, ensure};
use chrono::NaiveDate;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use url::Url;

use crate::model::{Category, Instance, IssueKey, ViewId, is_project_key};
use crate::rules::Classifier;
use crate::store;

/// Environment variable naming an alternative config file.
pub(crate) const CONFIG_ENV: &str = "EXA_STATUS_CONFIG";
/// Default config file, relative to the working directory.
pub(crate) const CONFIG_FILE: &str = "config.local.json";
/// Refresh intervals the UI offers, in minutes; 0 means by hand only.
pub(crate) const REFRESH_CHOICES: [u64; 5] = [0, 15, 30, 60, EVERY_8_HOURS];
/// The default refresh interval, in minutes.
const EVERY_8_HOURS: u64 = 8 * 60;
/// Key of the refresh interval in the config file.
pub(crate) const REFRESH_KEY: &str = "refresh_minutes";
/// EXA Reporter's address when the config does not name one.
const DEFAULT_REPORTER_URL: &str = "https://localhost:9835/";
/// Remaining-work weights: nothing done yet, under way, in review, being verified.
const NOT_STARTED_WEIGHT: f64 = 1.0;
const UNDER_WAY_WEIGHT: f64 = 0.6;
const REVIEW_WEIGHT: f64 = 0.3;
const VERIFY_WEIGHT: f64 = 0.15;
/// Statuses of work in review, and of work being verified.
const REVIEW_STATUSES: [&str; 2] = ["in review", "in code review"];
const VERIFY_STATUSES: [&str; 4] = [
    AWAITING_VERIFICATION,
    "test ready",
    "test in progress",
    "test",
];
/// The status of a fix that waits for someone to verify it.
const AWAITING_VERIFICATION: &str = "awaiting verification";
/// The label NVDCSE tickets carry while their fix waits for validation.
const AWAITING_VALIDATION_LABEL: &str = "awaiting_validation";
/// Release dates Ivan set on 22 Oct 2026, where Jira and the PM pages said
/// otherwise. Evaluated when compiled, so a wrong date fails the build.
const AGREED_DATES: [(&str, NaiveDate); 2] = [
    ("ES6.3.10", date(2027, 1, 27)),
    ("EXA7 GA", date(2027, 3, 31)),
];

/// A calendar date for a constant; one that does not exist stops the build.
const fn date(year: i32, month: u32, day: u32) -> NaiveDate {
    match NaiveDate::from_ymd_opt(year, month, day) {
        Some(date) => date,
        None => panic!("not a calendar date"),
    }
}

/// Jira projects whose open tickets feed the customer tabs.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustomerSource {
    /// Name shown in the "Open" breakdown, e.g. "DDN".
    pub(crate) name: String,
    /// Jira holding the tickets.
    pub(crate) instance: Instance,
    /// Its projects, e.g. `["GCP", "NVDCSE", "NCP"]`.
    pub(crate) projects: Vec<String>,
}

impl CustomerSource {
    /// The query for the open tickets.
    pub(crate) fn jql(&self) -> String {
        format!(
            "project in ({}) AND statusCategory != Done",
            self.projects.join(", ")
        )
    }
}

/// A Future work initiative.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct InitiativeConfig {
    /// Card title.
    pub(crate) name: String,
    /// Confluence id of its PM status page, if it has one.
    pub(crate) pm_page: Option<u64>,
    /// Its epics on Jira cloud.
    #[serde(default)]
    pub(crate) epics: Vec<IssueKey>,
    /// Query for its work items; defaults to the children of `epics`.
    pub(crate) jql: Option<String>,
    /// Also show it as a row in the EXA SD view.
    #[serde(default)]
    pub(crate) in_exa_sd: bool,
}

/// The weekly PM status pages (Confluence space PR) and what they describe.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct PmPages {
    /// EXAScaler 6.3.10 page id.
    pub(crate) exa6: u64,
    /// The ES6.3.* fixVersion that page describes.
    pub(crate) exa6_release: String,
    /// EXAScaler 7 page id, with a Beta and a GA date.
    pub(crate) exa7: u64,
    /// The label of the Beta date on that page ("Beta: 30 Nov 2026").
    pub(crate) exa7_beta_label: String,
    /// The label of the GA date on that page.
    pub(crate) exa7_ga_label: String,
    /// EXA SD page id.
    pub(crate) exa_sd: u64,
    /// EXAScaler GCP Lustre page id.
    pub(crate) gcp: u64,
}

impl Default for PmPages {
    fn default() -> Self {
        Self {
            exa6: 3_994_616_201,
            exa6_release: "ES6.3.10".into(),
            exa7: 3_994_616_067,
            exa7_beta_label: "Beta".into(),
            exa7_ga_label: "GA".into(),
            exa_sd: 3_495_624_961,
            gcp: 3_994_616_302,
        }
    }
}

/// Severity for tickets that have only a priority: in `projects`, a ticket
/// without a severity takes the level its priority maps to.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct PrioritySeverity {
    /// Projects whose tickets are rated this way.
    pub(crate) projects: Vec<String>,
    /// Severity level (1 to 4) per priority name; case does not matter.
    /// Entries in the config file override the default for that priority only.
    #[serde(deserialize_with = "levels_over_defaults")]
    pub(crate) levels: BTreeMap<String, u8>,
}

impl Default for PrioritySeverity {
    fn default() -> Self {
        Self {
            // Decided on 21 Oct 2026: GCP, NVDCSE and NCP have no severity
            // field, so Blocker and Critical count as S1 and Major as S2. NP,
            // NCP's copy on Jira cloud, often carries DDN's Severity; an NP
            // ticket without one is rated the same way.
            projects: strings(&["GCP", "NVDCSE", "NCP", "NP"]),
            levels: BTreeMap::from([
                ("Blocker".to_string(), 1),
                ("Critical".to_string(), 1),
                ("Major".to_string(), 2),
            ]),
        }
    }
}

/// The configured levels laid over the defaults; a configured name replaces
/// a default that differs from it only in case.
fn levels_over_defaults<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, u8>, D::Error> {
    let mut levels = PrioritySeverity::default().levels;
    for (name, level) in BTreeMap::<String, u8>::deserialize(d)? {
        levels.retain(|k, _| !k.eq_ignore_ascii_case(&name));
        levels.insert(name, level);
    }
    Ok(levels)
}

/// A Lustre branch: the tag family whose first tag containing a change is
/// reported, and where its patches decide delivery.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TagFamily {
    /// Gerrit branch, e.g. `b_es6_0`.
    pub(crate) branch: String,
    /// Tag prefix, e.g. `2.14.0-ddn`.
    pub(crate) prefix: String,
    /// Customer group and release views whose tickets this branch's patches
    /// deliver, in place of Jira. The All view's tickets follow their group.
    pub(crate) views: Vec<ViewId>,
    /// The release expected to carry a customer ticket's patch still in
    /// review here, named as in `release_dates`.
    pub(crate) release: String,
}

/// All settings.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct Config {
    /// Address the dashboard listens on: 127.0.0.1 or ::1. Only loopback,
    /// because the Host and Origin checks keep out browsers, not other
    /// machines; only these two, because the certificate and the Host check
    /// name "localhost" and "127.0.0.1".
    pub(crate) host: IpAddr,
    /// Port the dashboard listens on.
    pub(crate) port: u16,
    /// Minutes between automatic refreshes; 0 means by hand only.
    pub(crate) refresh_minutes: u64,
    /// Snapshot, history and caches.
    pub(crate) data_dir: PathBuf,
    /// The dashboard's self-signed certificate and key.
    pub(crate) cert_dir: PathBuf,
    /// Dates promised to customers.
    pub(crate) commitments_file: PathBuf,
    /// The llm_jira tool's Jira credentials.
    pub(crate) jira_config: PathBuf,
    /// The gerrit-cli tool's Gerrit credentials.
    pub(crate) gerrit_env: PathBuf,
    /// EXA Reporter's address; `None` means `https://localhost:9835/`.
    pub(crate) reporter_url: Option<Url>,
    /// EXA Reporter's self-signed certificate, pinned for its HTTPS calls.
    pub(crate) reporter_cert: PathBuf,
    /// Days of build history the health tile covers.
    pub(crate) reporter_days: u32,
    /// Parallel Atlassian requests (Jira and Confluence) during a refresh.
    pub(crate) workers: usize,
    /// Parallel Gerrit requests during a refresh.
    pub(crate) gerrit_workers: usize,
    /// An S1 ticket idle longer than this is a blocker.
    pub(crate) idle_s1_days: i64,
    /// An S2 ticket idle longer than this is a blocker.
    pub(crate) idle_s2_days: i64,
    /// An S1/S2 ticket idle longer than this puts its customer view off track.
    pub(crate) idle_offtrack_days: i64,
    /// Idle time counted in the "Idle" number and in per-item judgements.
    pub(crate) idle_alert_days: i64,
    /// A dated item is at risk inside this window when work has not started or it is idle.
    pub(crate) risk_window_days: i64,
    /// Up to this many days late is "at risk"; more is "off track".
    pub(crate) late_tolerance_days: i64,
    /// PM page and Jira dates further apart than this are flagged.
    pub(crate) date_gap_days: i64,
    /// Releases and initiatives with this many open items or fewer are judged per item.
    pub(crate) small_release_max: usize,
    /// Days of closures the projection rate is taken from.
    pub(crate) closed_window_days: i64,
    /// Remaining-work weight per status name, 0 to 1. Entries in the config
    /// file override the default for that status only; case does not matter.
    #[serde(deserialize_with = "weights_over_defaults")]
    pub(crate) status_weights: BTreeMap<String, f64>,
    /// Statuses counted as "in review or verification"; case does not
    /// matter. Kept apart from `status_weights`, so retuning a weight does not
    /// change what this number counts.
    pub(crate) review_statuses: Vec<String>,
    /// Statuses of a ticket whose fix awaits verification, which the page
    /// marks; case does not matter.
    pub(crate) verification_statuses: Vec<String>,
    /// Labels that mark a ticket the same way; case does not matter.
    pub(crate) verification_labels: Vec<String>,
    /// Comment authors that are bots, not people.
    pub(crate) bot_authors: Vec<String>,
    /// "Site Affected" values that mean NVIDIA.
    pub(crate) nvidia_site: String,
    /// Summary text that names NVIDIA as the customer.
    pub(crate) nvidia_summary: String,
    /// Projects whose tickets are all NVIDIA's.
    pub(crate) nvidia_projects: Vec<String>,
    /// Site or summary text that means GCP.
    pub(crate) gcp_pattern: String,
    /// Projects whose tickets are all GCP's.
    pub(crate) gcp_projects: Vec<String>,
    /// Summary text of test tickets, left out of customer views.
    pub(crate) test_pattern: String,
    /// Customer ticket queries.
    pub(crate) customer_sources: Vec<CustomerSource>,
    /// Project pairs that hold copies of the same tickets; a ticket and its
    /// copy (same summary) are shown as one row.
    pub(crate) mirror_projects: Vec<(String, String)>,
    /// How tickets with a priority but no severity get one, for the S1/S2 idle limits.
    pub(crate) priority_severity: PrioritySeverity,
    /// The EXAScaler engineering project on Jira cloud.
    pub(crate) ex_project: String,
    /// Projects on Jira cloud whose linked tickets are fixes for a customer
    /// ticket; fixes are read there only. They must include `ex_project`, the
    /// only one whose fixes give a date: EXR fixVersions are planning quarters.
    pub(crate) fix_projects: Vec<String>,
    /// Link relations (from the customer ticket's side, any case) that do not
    /// make the linked ticket its fix. Any other relation does: DDN links
    /// tickets to their fixes mostly with "is related to".
    pub(crate) non_fix_relations: Vec<String>,
    /// EXA6 releases are unreleased, dated fixVersions with this prefix.
    pub(crate) exa6_prefix: String,
    /// The EXA6 fixVersion holding unscheduled work.
    pub(crate) exa6_unscheduled: String,
    /// The EXA7 Beta fixVersion.
    pub(crate) exa7_beta_version: String,
    /// The EXA7 GA fixVersions.
    pub(crate) exa7_ga_versions: Vec<String>,
    /// Tracked EX fixVersions whose work is a saved Jira filter, by name: the
    /// release counts the filter's open issues and its closures in the window
    /// instead of every item in the version.
    pub(crate) scope_filters: BTreeMap<String, u64>,
    /// Release dates agreed outside Jira and the PM pages, by release: an
    /// EXA6 fixVersion ("ES6.3.10"), "EXA7 Beta", "EXA7 GA", an EXA SD phase or
    /// a Future work initiative. Its verdict is measured against this date
    /// before the PM page's and Jira's.
    pub(crate) release_dates: BTreeMap<String, NaiveDate>,
    /// EXA SD's Jira project.
    pub(crate) exa_sd_project: String,
    /// EXA SD phase fixVersions start with this.
    pub(crate) exa_sd_phase_prefix: String,
    /// The phase the EXA SD PM page date applies to.
    pub(crate) exa_sd_pm_phase: String,
    /// PM status pages.
    pub(crate) pm_pages: PmPages,
    /// Future work initiatives.
    pub(crate) initiatives: Vec<InitiativeConfig>,
    /// Gerrit project holding the EXAScaler Lustre branches.
    pub(crate) gerrit_project: String,
    /// Projects whose keys appear in Lustre commit messages; linked tickets of
    /// these projects are searched for along with the watched ticket.
    pub(crate) gerrit_key_projects: Vec<String>,
    /// Tag families, one per branch.
    pub(crate) tag_families: Vec<TagFamily>,
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn source(name: &str, instance: Instance, projects: &[&str]) -> CustomerSource {
    CustomerSource {
        name: name.into(),
        instance,
        projects: strings(projects),
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(ToString::to_string).collect()
}

/// Keys written in this file; `the_default_keys_all_parse` checks none is dropped.
fn keys(items: &[&str]) -> Vec<IssueKey> {
    items.iter().filter_map(|k| k.parse().ok()).collect()
}

/// Weights per lowercase status name, as agreed on 21 Oct 2026.
fn default_weights() -> BTreeMap<String, f64> {
    let not_started = [
        "to do",
        "open",
        "new",
        "backlog",
        "in backlog",
        "reopened",
        "selected for development",
        "need information",
        "info needed",
        "blocked",
        "blocked external",
    ];
    let weighted = |statuses: &[&str], weight: f64| {
        statuses
            .iter()
            .map(|s| (s.to_string(), weight))
            .collect::<Vec<_>>()
    };
    weighted(&not_started, NOT_STARTED_WEIGHT)
        .into_iter()
        .chain(weighted(&["in progress"], UNDER_WAY_WEIGHT))
        .chain(weighted(&REVIEW_STATUSES, REVIEW_WEIGHT))
        .chain(weighted(&VERIFY_STATUSES, VERIFY_WEIGHT))
        .collect()
}

/// The configured weights laid over the defaults, keys lowercased.
fn weights_over_defaults<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, f64>, D::Error> {
    let mut weights = default_weights();
    weights.extend(
        BTreeMap::<String, f64>::deserialize(d)?
            .into_iter()
            .map(|(k, v)| (k.to_lowercase(), v)),
    );
    Ok(weights)
}

impl Default for Config {
    fn default() -> Self {
        let home = home();
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            // Next to EXA Reporter's 9835.
            port: 9836,
            // The 30-60 minutes asked for; the page offers 15 to 60.
            refresh_minutes: EVERY_8_HOURS,
            data_dir: PathBuf::from("data"),
            cert_dir: PathBuf::from("certs"),
            commitments_file: PathBuf::from("commitments.json"),
            jira_config: home.join(".jira-tool.json"),
            gerrit_env: home.join(".config/gerrit-cli/.env"),
            reporter_url: None,
            reporter_cert: home.join("work/src/lustre_reporter/certs/cert.pem"),
            // EXA Reporter's own default window.
            reporter_days: 7,
            // Eight parallel searches ran without rate limiting on 21 Oct 2026.
            workers: 8,
            // review.whamcloud.com is the shared community server; keep the load light.
            gerrit_workers: 4,
            // Agreed on 21 Oct 2026: S1 3 days, S2 7 days, off track past 30.
            idle_s1_days: 3,
            idle_s2_days: 7,
            idle_offtrack_days: 30,
            // A working week without a word from a person.
            idle_alert_days: 7,
            // Inside two weeks, a ticket nobody has started will not make its date.
            risk_window_days: 14,
            // Two weeks late is one sprint of slip, still recoverable; more is not.
            late_tolerance_days: 14,
            // Agreed on 21 Oct 2026.
            date_gap_days: 7,
            // Agreed on 21 Oct 2026: with five or fewer items left, a closing
            // rate says little, so each item is judged instead.
            small_release_max: 5,
            // Four weeks, so one holiday week does not swing the rate.
            closed_window_days: 28,
            status_weights: default_weights(),
            review_statuses: strings(&REVIEW_STATUSES).into_iter().chain(strings(&VERIFY_STATUSES)).collect(),
            verification_statuses: strings(&[AWAITING_VERIFICATION]),
            verification_labels: strings(&[AWAITING_VALIDATION_LABEL]),
            bot_authors: strings(&[
                "Gerrit",
                "Gerrit Updater",
                "Maloo",
                "Autotest",
                "Autotest System",
                "Automation for Jira",
                "Jira Automation",
                "Jenkins",
            ]),
            nvidia_site: r"(?i)nvidia".into(),
            // Customer names in case titles, not technology names such as "Nvidia GPUDirect".
            nvidia_summary: r"(?i)(\|\s*nvidia\b|nvidia\s*[-|]|\bnvidia aurora\b|\bNVDA\b|\bDGXC\b|nvidia security)".into(),
            nvidia_projects: strings(&["NVDCSE", "NCP", "NP"]),
            gcp_pattern: r"(?i)\b(gcp|google)\b".into(),
            gcp_projects: strings(&["GCP"]),
            test_pattern: r"(?i)\[test\]|test support case".into(),
            customer_sources: vec![
                // Whamcloud first, so NCP keeps the row when NP holds a copy.
                source("Whamcloud", Instance::Whamcloud, &["GCP", "NVDCSE", "NCP"]),
                source("DDN", Instance::Cloud, &["DDN"]),
                source("SFAFE", Instance::Cloud, &["SFAFE"]),
                source("NP", Instance::Cloud, &["NP"]),
            ],
            // NP on Jira cloud copies NCP tickets from Whamcloud.
            mirror_projects: vec![("NCP".into(), "NP".into())],
            priority_severity: PrioritySeverity::default(),
            ex_project: "EX".into(),
            fix_projects: strings(&["EX", "EXR"]),
            // Escalation bookkeeping, not the fix itself.
            non_fix_relations: strings(&["has action item", "action item from", "occurred in escalation", "causes"]),
            exa6_prefix: "ES6.3.".into(),
            exa6_unscheduled: "ES6.3.x (TBD)".into(),
            exa7_beta_version: "ES7.0.0_Beta".into(),
            exa7_ga_versions: strings(&["ES7.0.0", "ES7.1.0"]),
            // "EX 6.3.10 scope", Manish Agrawal's filter, chosen on 22 Oct 2026.
            scope_filters: BTreeMap::from([("ES6.3.10".into(), 49208)]),
            release_dates: AGREED_DATES
                .iter()
                .map(|(name, date)| ((*name).to_string(), *date))
                .collect(),
            exa_sd_project: "EXM".into(),
            exa_sd_phase_prefix: "NVMesh-P".into(),
            exa_sd_pm_phase: "NVMesh-P2".into(),
            pm_pages: PmPages::default(),
            initiatives: vec![
                InitiativeConfig {
                    name: "New HA (EAM)".into(),
                    pm_page: None,
                    epics: keys(&["EXM-458", "EXM-462", "EXM-503"]),
                    jql: None,
                    in_exa_sd: true,
                },
                InitiativeConfig {
                    name: "NDU".into(),
                    pm_page: Some(3_994_550_624),
                    epics: keys(&["EX-15467", "EX-15385", "EX-15032", "EX-14671", "EX-11062"]),
                    jql: None,
                    in_exa_sd: false,
                },
                InitiativeConfig {
                    name: "EXA UI".into(),
                    pm_page: Some(3_994_550_754),
                    epics: Vec::new(),
                    jql: Some("project = EXAUI AND issuetype != Epic".into()),
                    in_exa_sd: false,
                },
                InitiativeConfig {
                    name: "Security and compliance".into(),
                    pm_page: Some(3_995_860_993),
                    epics: keys(&["EXR-213", "EX-15288", "EX-15928", "EX-14170"]),
                    jql: None,
                    in_exa_sd: false,
                },
                InitiativeConfig {
                    name: "Embedded Orchestrator".into(),
                    pm_page: None,
                    epics: keys(&["EXM-450", "EXM-688"]),
                    jql: None,
                    in_exa_sd: false,
                },
                InitiativeConfig {
                    name: "Pluggable HA".into(),
                    pm_page: None,
                    epics: keys(&["EXM-720"]),
                    jql: None,
                    in_exa_sd: false,
                },
            ],
            gerrit_project: "ex/lustre-release".into(),
            gerrit_key_projects: strings(&["EX", "LU"]),
            // GCP runs both lines, every other customer EXA6.
            tag_families: vec![
                TagFamily {
                    branch: "b_es6_0".into(),
                    prefix: "2.14.0-ddn".into(),
                    views: vec![ViewId::Others, ViewId::Gcp, ViewId::Nvidia, ViewId::Exa6],
                    release: "ES6.3.10".into(),
                },
                TagFamily {
                    branch: "b_es7_0".into(),
                    prefix: "2.16.0-ddn".into(),
                    views: vec![ViewId::Gcp, ViewId::Exa7Beta, ViewId::Exa7Ga],
                    release: ViewId::Exa7Beta.label().into(),
                },
            ],
        }
    }
}

impl Config {
    /// EXA Reporter's address, ending in `/` so API paths join under it.
    pub(crate) fn reporter_url(&self) -> Result<Url> {
        let mut url = match &self.reporter_url {
            Some(url) => url.clone(),
            None => Url::parse(DEFAULT_REPORTER_URL)
                .context("parsing the default EXA Reporter address")?,
        };
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        Ok(url)
    }

    /// The config file in use: `$EXA_STATUS_CONFIG`, else `config.local.json`,
    /// and whether it was named explicitly.
    pub(crate) fn path() -> (PathBuf, bool) {
        match std::env::var_os(CONFIG_ENV) {
            Some(p) => (PathBuf::from(p), true),
            None => (PathBuf::from(CONFIG_FILE), false),
        }
    }

    /// Defaults merged with the config file in use.
    pub(crate) fn load() -> Result<Self> {
        let (path, named) = Self::path();
        Self::load_from(&path, named)
    }

    /// Defaults merged with the file at `path`. A missing file means
    /// defaults, unless it was `named` explicitly, which makes it an error.
    pub(crate) fn load_from(path: &Path, named: bool) -> Result<Self> {
        match store::read_optional(path)? {
            Some(raw) => Self::parse(&raw).with_context(|| format!("parsing {}", path.display())),
            None if named => bail!(
                "{CONFIG_ENV} names {}, which does not exist",
                path.display()
            ),
            None => Self::parse("{}"),
        }
    }

    /// Defaults merged with the JSON in `raw`, checked.
    pub(crate) fn parse(raw: &str) -> Result<Self> {
        let cfg: Self = serde_json::from_str(raw)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Checks what the types cannot: addresses, ranges and patterns.
    fn validate(&self) -> Result<()> {
        ensure!(
            self.host == IpAddr::V4(Ipv4Addr::LOCALHOST)
                || self.host == IpAddr::V6(Ipv6Addr::LOCALHOST),
            "host must be 127.0.0.1 or ::1, not {}",
            self.host
        );
        ensure!(
            REFRESH_CHOICES.contains(&self.refresh_minutes),
            "{REFRESH_KEY} must be one of {REFRESH_CHOICES:?}, not {}",
            self.refresh_minutes
        );
        ensure!(
            self.closed_window_days > 0,
            "closed_window_days must be at least 1"
        );
        ensure!(
            self.workers > 0 && self.gerrit_workers > 0,
            "workers and gerrit_workers must be at least 1"
        );
        ensure!(self.reporter_days > 0, "reporter_days must be at least 1");
        for (name, days) in [
            ("idle_s1_days", self.idle_s1_days),
            ("idle_s2_days", self.idle_s2_days),
            ("idle_offtrack_days", self.idle_offtrack_days),
            ("idle_alert_days", self.idle_alert_days),
            ("risk_window_days", self.risk_window_days),
            ("late_tolerance_days", self.late_tolerance_days),
            ("date_gap_days", self.date_gap_days),
        ] {
            ensure!(days >= 0, "{name} must not be negative");
        }
        if let Some((status, w)) = self
            .status_weights
            .iter()
            .find(|(_, w)| !(0.0..=1.0).contains(*w))
        {
            bail!("status_weights: {status:?} has weight {w}; weights run from 0 to 1");
        }
        if let Some((priority, level)) = self
            .priority_severity
            .levels
            .iter()
            .find(|(_, l)| !(1..=4).contains(*l))
        {
            bail!("priority_severity: {priority:?} maps to {level}; severities run from 1 to 4");
        }
        Classifier::new(self)?;
        let mut names = BTreeSet::new();
        for s in &self.customer_sources {
            ensure!(
                names.insert(&s.name),
                "two customer_sources are named {:?}",
                s.name
            );
            ensure!(
                !s.projects.is_empty() && s.projects.iter().all(|p| is_project_key(p)),
                "customer source {:?} needs projects named like DDN or NVDCSE",
                s.name
            );
        }
        ensure!(
            self.fix_projects.contains(&self.ex_project),
            "fix_projects must include {}: only its fixes give dates",
            self.ex_project
        );
        for i in &self.initiatives {
            ensure!(
                !i.epics.is_empty() || i.jql.is_some(),
                "initiative {:?} needs epics or jql",
                i.name
            );
        }
        // EXA SD and Future work tracks are judged from items without patches.
        let deliverable = |v: &ViewId| {
            *v != ViewId::All && (ViewId::CUSTOMERS.contains(v) || ViewId::RELEASES.contains(v))
        };
        for f in &self.tag_families {
            if let Some(v) = f.views.iter().find(|v| !deliverable(v)) {
                bail!(
                    "tag_families: {} names {}; patches decide delivery only in the customer group and EX release views",
                    f.branch,
                    v.as_str()
                );
            }
        }
        self.reporter_url()?;
        Ok(())
    }

    /// Remaining-work weight of one open item: by status name, else by category.
    pub(crate) fn weight(&self, status: &str, category: Category) -> f64 {
        match self.status_weights.get(&status.to_lowercase()) {
            Some(w) => *w,
            None if category == Category::Indeterminate => UNDER_WAY_WEIGHT,
            None => NOT_STARTED_WEIGHT,
        }
    }

    /// Remaining-work weight of an item whose patch is in review: that of the
    /// "in review" status.
    pub(crate) fn review_weight(&self) -> f64 {
        self.weight(REVIEW_STATUSES[0], Category::Indeterminate)
    }

    /// The status counts as "in review or verification".
    pub(crate) fn in_review(&self, status: &str) -> bool {
        self.review_statuses
            .iter()
            .any(|s| s.eq_ignore_ascii_case(status))
    }

    /// A link with this relation makes the linked ticket the customer ticket's fix.
    pub(crate) fn is_fix_relation(&self, relation: &str) -> bool {
        let relation = relation.trim();
        !self
            .non_fix_relations
            .iter()
            .any(|r| r.trim().eq_ignore_ascii_case(relation))
    }

    /// Path of a file in the data directory.
    pub(crate) fn data_file(&self, name: &str) -> PathBuf {
        self.data_dir.join(name)
    }

    /// The customer views a source's tickets can land in: a project that is
    /// all NVIDIA's or all GCP's feeds that view and All; any other project
    /// feeds every customer view, since its tickets are sorted by site and title.
    pub(crate) fn source_views(&self, source: &CustomerSource) -> BTreeSet<ViewId> {
        source
            .projects
            .iter()
            .flat_map(|p| {
                if self.nvidia_projects.contains(p) {
                    vec![ViewId::All, ViewId::Nvidia]
                } else if self.gcp_projects.contains(p) {
                    vec![ViewId::All, ViewId::Gcp]
                } else {
                    ViewId::CUSTOMERS.to_vec()
                }
            })
            .collect()
    }
}

/// The config file text with `refresh_minutes` set and every other key kept
/// in place. `existing` is the current file content, or `None` when there is
/// no file. Fails rather than replace a file that is not a valid config.
pub(crate) fn with_refresh_minutes(existing: Option<&str>, minutes: u64) -> Result<String> {
    ensure!(
        REFRESH_CHOICES.contains(&minutes),
        "{REFRESH_KEY} must be one of {REFRESH_CHOICES:?}, not {minutes}"
    );
    let mut object = match existing {
        None => Map::new(),
        Some(text) => match serde_json::from_str::<Value>(text)
            .context("the config file is not valid JSON")?
        {
            Value::Object(o) => o,
            _ => bail!("the config file is not a JSON object"),
        },
    };
    object.insert(REFRESH_KEY.into(), Value::from(minutes));
    let text = serde_json::to_string_pretty(&Value::Object(object))?;
    Config::parse(&text).context("the updated config would not load")?;
    Ok(text + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test]
    fn an_empty_file_gives_the_defaults_and_they_are_valid() {
        assert_eq!(Config::parse("{}").unwrap(), Config::default());
        assert_eq!(Config::default().refresh_minutes, 480, "every 8 hours");
    }

    #[test]
    fn the_default_keys_all_parse() {
        let epics: Vec<usize> = Config::default()
            .initiatives
            .iter()
            .map(|i| i.epics.len())
            .collect();

        assert_eq!(epics, [3, 5, 0, 4, 2, 1]);
    }

    #[test]
    fn a_typo_fails_loudly() {
        let err = Config::parse(r#"{"refresh_minute": 15}"#).unwrap_err();

        assert!(
            err.to_string().contains("unknown field `refresh_minute`"),
            "{err}"
        );
    }

    #[test_case(r#"{"refresh_minutes": 7}"#, "refresh_minutes must be one of [0, 15, 30, 60, 480], not 7" ; "unoffered interval")]
    #[test_case(r#"{"host": "0.0.0.0"}"#, "host must be 127.0.0.1 or ::1, not 0.0.0.0" ; "network-wide listener")]
    #[test_case(r#"{"host": "127.0.0.2"}"#, "host must be 127.0.0.1 or ::1, not 127.0.0.2" ; "loopback the certificate does not name")]
    #[test_case(r#"{"closed_window_days": 0}"#, "closed_window_days must be at least 1" ; "empty closing window")]
    #[test_case(r#"{"workers": 0}"#, "workers and gerrit_workers must be at least 1" ; "no workers")]
    #[test_case(r#"{"idle_s1_days": -1}"#, "idle_s1_days must not be negative" ; "negative days")]
    #[test_case(r#"{"status_weights": {"Parked": 2.0}}"#, "status_weights: \"parked\" has weight 2; weights run from 0 to 1" ; "weight out of range")]
    #[test_case(r#"{"gcp_pattern": "(unclosed"}"#, "gcp_pattern" ; "bad pattern")]
    #[test_case(r#"{"priority_severity": {"levels": {"Blocker": 0}}}"#, "priority_severity: \"Blocker\" maps to 0; severities run from 1 to 4" ; "severity out of range")]
    #[test_case(r#"{"initiatives": [{"name": "X"}]}"#, "initiative \"X\" needs epics or jql" ; "empty initiative")]
    #[test_case(
        r#"{"customer_sources": [{"name": "DDN", "instance": "cloud", "projects": ["DDN"]}, {"name": "DDN", "instance": "cloud", "projects": ["SFAFE"]}]}"#,
        "two customer_sources are named \"DDN\"" ;
        "duplicate source"
    )]
    #[test_case(r#"{"customer_sources": [{"name": "DDN", "instance": "cloud", "projects": []}]}"#, "customer source \"DDN\" needs projects named like DDN or NVDCSE" ; "source without projects")]
    #[test_case(r#"{"customer_sources": [{"name": "DDN", "instance": "cloud", "projects": ["DDN) OR (1=1"]}]}"#, "customer source \"DDN\" needs projects named like DDN or NVDCSE" ; "project that is not a key")]
    #[test_case(r#"{"fix_projects": ["EXR"]}"#, "fix_projects must include EX: only its fixes give dates" ; "fixes without EX")]
    #[test_case(
        r#"{"tag_families": [{"branch": "b_es6_0", "prefix": "2.14.0-ddn", "views": ["customers-all"], "release": "ES6.3.10"}]}"#,
        "tag_families: b_es6_0 names customers-all; patches decide delivery only in the customer group and EX release views" ;
        "the All view, whose tickets follow their group"
    )]
    #[test_case(
        r#"{"tag_families": [{"branch": "b_es7_0", "prefix": "2.16.0-ddn", "views": ["dev-exa-sd"], "release": "EXA7 GA"}]}"#,
        "tag_families: b_es7_0 names dev-exa-sd; patches decide delivery only in the customer group and EX release views" ;
        "EXA SD, judged from items without patches"
    )]
    fn bad_values_are_rejected_at_load(json: &str, message: &str) {
        let err = Config::parse(json).unwrap_err();

        assert_eq!(err.to_string(), message);
    }

    #[test_case(r#"{"pm_pages": {"exa6": "12a"}}"#, "invalid type: string \"12a\", expected u64" ; "page id that is not a number")]
    #[test_case(r#"{"initiatives": [{"name": "X", "epics": ["ex-1"]}]}"#, "\"ex-1\" is not a Jira key" ; "epic that is not a key")]
    fn badly_typed_values_are_rejected_at_load(json: &str, message: &str) {
        let err = Config::parse(json).unwrap_err().to_string();

        assert!(err.contains(message), "{err}");
    }

    #[test]
    fn the_defaults_point_at_the_live_sources() {
        let cfg = Config::default();

        assert_eq!(
            cfg.reporter_url().unwrap().as_str(),
            "https://localhost:9835/"
        );
        assert_eq!(cfg.host, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_eq!(cfg.customer_sources.len(), 4);
        assert_eq!(cfg.initiatives.iter().filter(|i| i.in_exa_sd).count(), 1);
    }

    #[test]
    fn a_reporter_address_without_a_trailing_slash_keeps_its_path() {
        let cfg = Config::parse(r#"{"reporter_url": "https://localhost:9835/reporter"}"#).unwrap();

        let url = cfg.reporter_url().unwrap();

        assert_eq!(
            url.join("api/stability").unwrap().as_str(),
            "https://localhost:9835/reporter/api/stability"
        );
    }

    #[test_case("In Review", Category::Indeterminate, REVIEW_WEIGHT ; "named status")]
    #[test_case("IN PROGRESS", Category::Indeterminate, UNDER_WAY_WEIGHT ; "case does not matter")]
    #[test_case("Waiting for Godot", Category::Indeterminate, UNDER_WAY_WEIGHT ; "unknown, under way")]
    #[test_case("Parked", Category::New, NOT_STARTED_WEIGHT ; "unknown, not started")]
    fn weight_by_status_then_category(status: &str, category: Category, expected: f64) {
        assert_eq!(Config::default().weight(status, category), expected);
    }

    #[test]
    fn a_weight_override_keeps_the_other_defaults_and_ignores_case() {
        let cfg = Config::parse(r#"{"status_weights": {"In Review": 0.5}}"#).unwrap();

        assert_eq!(cfg.weight("in review", Category::Indeterminate), 0.5);
        assert_eq!(
            cfg.review_weight(),
            0.5,
            "a patch in review weighs as In Review"
        );
        assert_eq!(Config::default().review_weight(), REVIEW_WEIGHT);
        assert_eq!(
            cfg.weight("Awaiting Verification", Category::Indeterminate),
            VERIFY_WEIGHT
        );
        assert_eq!(cfg.status_weights.len(), default_weights().len());
    }

    #[test]
    fn a_priority_level_override_keeps_the_other_defaults_and_ignores_case() {
        let cfg = Config::parse(r#"{"priority_severity": {"levels": {"MAJOR": 3, "Minor": 4}}}"#)
            .unwrap();

        assert_eq!(
            cfg.priority_severity.levels,
            BTreeMap::from([
                ("Blocker".to_string(), 1),
                ("Critical".to_string(), 1),
                ("MAJOR".to_string(), 3),
                ("Minor".to_string(), 4)
            ])
        );
        assert_eq!(
            cfg.priority_severity.projects,
            PrioritySeverity::default().projects
        );
    }

    #[test]
    fn release_dates_are_set_in_the_config_file_as_iso_dates() {
        let date = |y, m, d| NaiveDate::from_ymd_opt(y, m, d).unwrap();

        assert_eq!(
            Config::default().release_dates,
            BTreeMap::from([
                ("ES6.3.10".to_string(), date(2027, 1, 27)),
                ("EXA7 GA".to_string(), date(2027, 3, 31))
            ])
        );
        let set = Config::parse(r#"{"release_dates": {"ES6.3.10": "2027-02-01"}}"#).unwrap();
        assert_eq!(
            set.release_dates,
            BTreeMap::from([("ES6.3.10".to_string(), date(2027, 2, 1))])
        );
        assert!(Config::parse(r#"{"release_dates": {"ES6.3.10": "27 Jan 2027"}}"#).is_err());
    }

    #[test]
    fn customer_sources_query_their_projects_and_feed_the_views_their_tickets_reach() {
        let cfg = Config::default();
        let views = |name: &str| {
            let source = cfg
                .customer_sources
                .iter()
                .find(|s| s.name == name)
                .unwrap();
            cfg.source_views(source).into_iter().collect::<Vec<_>>()
        };

        assert_eq!(
            cfg.customer_sources.first().map(CustomerSource::jql),
            Some("project in (GCP, NVDCSE, NCP) AND statusCategory != Done".to_string())
        );
        assert_eq!(
            views("Whamcloud"),
            [ViewId::All, ViewId::Gcp, ViewId::Nvidia]
        );
        assert_eq!(views("NP"), [ViewId::All, ViewId::Nvidia]);
        assert_eq!(views("DDN"), ViewId::CUSTOMERS);
    }

    #[test]
    fn review_statuses_match_without_case() {
        let cfg = Config::default();

        assert!(cfg.in_review("In Code Review"));
        assert!(!cfg.in_review("In Progress"));
    }

    #[test_case("is related to", true ; "the usual fix link")]
    #[test_case("is related to ", true ; "trailing space in the link type")]
    #[test_case("Has Action Item", false ; "escalation bookkeeping, any case")]
    #[test_case("occurred in escalation", false ; "escalation record")]
    fn fix_relations(relation: &str, fix: bool) {
        assert_eq!(Config::default().is_fix_relation(relation), fix);
    }

    #[test]
    fn setting_the_interval_keeps_other_keys_in_their_order() {
        let text = with_refresh_minutes(
            Some(r#"{"port": 9999, "refresh_minutes": 30, "data_dir": "d"}"#),
            15,
        )
        .unwrap();

        assert_eq!(
            text,
            "{\n  \"port\": 9999,\n  \"refresh_minutes\": 15,\n  \"data_dir\": \"d\"\n}\n"
        );
    }

    #[test]
    fn setting_the_interval_creates_the_file_when_missing() {
        let text = with_refresh_minutes(None, 60).unwrap();

        assert_eq!(Config::parse(&text).unwrap().refresh_minutes, 60);
    }

    #[test_case(Some("not json"), 15, "the config file is not valid JSON" ; "corrupt file is kept")]
    #[test_case(Some("[1, 2]"), 15, "the config file is not a JSON object" ; "array is kept")]
    #[test_case(Some(r#"{"host": "0.0.0.0"}"#), 15, "the updated config would not load" ; "invalid file is kept")]
    #[test_case(None, 45, "refresh_minutes must be one of [0, 15, 30, 60, 480], not 45" ; "unoffered value")]
    fn setting_the_interval_refuses_to_clobber(
        existing: Option<&str>,
        minutes: u64,
        message: &str,
    ) {
        let err = with_refresh_minutes(existing, minutes).unwrap_err();

        assert_eq!(err.to_string(), message);
    }

    #[test]
    fn a_missing_file_means_defaults_unless_it_was_named() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent.json");
        let present = dir.path().join("present.json");
        std::fs::write(&present, r#"{"refresh_minutes": 60}"#).unwrap();

        assert_eq!(
            Config::load_from(&missing, false).unwrap(),
            Config::default()
        );
        assert_eq!(
            Config::load_from(&missing, true).unwrap_err().to_string(),
            format!(
                "{CONFIG_ENV} names {}, which does not exist",
                missing.display()
            )
        );
        assert_eq!(
            Config::load_from(&present, true).unwrap().refresh_minutes,
            60
        );
    }
}
