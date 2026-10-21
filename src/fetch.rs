//! The refresh worker's reads. Everything that touches the network lives here
//! and lands in `RawData`; `build` turns that into the snapshot without I/O.

use anyhow::{Context, Result, anyhow};
use chrono::{Local, NaiveDate};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::warn;

use crate::commitments::{self, Commitments};
use crate::config::{Config, InitiativeConfig, TagFamily};
use crate::confluence::{self, PmPage, PmParser};
use crate::gerrit::{self, Change, Gerrit};
use crate::http;
use crate::jira::{self, Jira, Tickets, Version, str_at};
use crate::model::{
    BuildHealth, ChangeStatus, Instance, IssueKey, PatchRef, SourceStatus, Ticket, ViewId,
};
use crate::reporter;
use crate::rules;
use crate::store::{self, CachedComment};

/// Ticket keys per fixVersion.
pub(crate) type KeysByVersion = HashMap<String, BTreeSet<IssueKey>>;
/// Gerrit changes per watched ticket.
pub(crate) type PatchMap = HashMap<IssueKey, Vec<PatchRef>>;
/// Views a note or a read concerns.
pub(crate) type Views = BTreeSet<ViewId>;

/// Keys per Gerrit search: forty keys keep the query URL near 2 KB, and the
/// watch list (about 400 keys) then needs about ten searches.
const GERRIT_KEYS_PER_SEARCH: usize = 40;
/// Keys named in a note before it says "and N more".
const KEYS_NAMED: usize = 5;

/// A warning about missing or partial data.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Note {
    /// The views it is shown in: those whose data it concerns.
    pub(crate) views: Views,
    /// What it says.
    pub(crate) text: String,
    /// Data those views' verdicts depend on is missing, so they cannot read
    /// better than "at risk".
    pub(crate) incomplete: bool,
}

impl Note {
    /// A warning that leaves the verdicts as they are.
    pub(crate) fn info(views: impl IntoIterator<Item = ViewId>, text: String) -> Self {
        Self {
            views: views.into_iter().collect(),
            text,
            incomplete: false,
        }
    }

    /// Missing data the verdicts depend on.
    pub(crate) fn gap(views: impl IntoIterator<Item = ViewId>, text: String) -> Self {
        Self {
            views: views.into_iter().collect(),
            text,
            incomplete: true,
        }
    }
}

/// The open work, epics and closures of one Future work initiative.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InitiativeData {
    /// Its configuration.
    pub(crate) config: InitiativeConfig,
    /// Its epics.
    pub(crate) epics: Vec<Ticket>,
    /// Its open work items.
    pub(crate) open: Vec<Ticket>,
    /// Items closed during the closing window.
    pub(crate) closed_in_window: usize,
}

/// Everything one refresh read.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RawData {
    /// The date of the refresh.
    pub(crate) today: NaiveDate,
    /// EX fixVersions.
    pub(crate) ex_versions: Vec<Version>,
    /// EXA SD fixVersions.
    pub(crate) sd_versions: Vec<Version>,
    /// Open customer tickets from every source, in source order.
    pub(crate) customers: Vec<Ticket>,
    /// Open EX items in the tracked fixVersions.
    pub(crate) ex_open: Vec<Ticket>,
    /// Open EXA SD items.
    pub(crate) sd_open: Vec<Ticket>,
    /// EX items closed in the window, by fixVersion.
    pub(crate) ex_closed: KeysByVersion,
    /// EXA SD items closed in the window, by fixVersion.
    pub(crate) sd_closed: KeysByVersion,
    /// Items added to each release view in the window (created, or moved
    /// into one of its versions); `None` when not read.
    pub(crate) added: BTreeMap<ViewId, Option<usize>>,
    /// Open EX items with no scheduled EXA6 release; `None` when not read.
    pub(crate) unscheduled: Option<usize>,
    /// PM status pages by page id.
    pub(crate) pm_pages: HashMap<u64, PmPage>,
    /// Linked fixes of customer tickets, read on Jira cloud.
    pub(crate) fixes: HashMap<IssueKey, Ticket>,
    /// Promised dates.
    pub(crate) commitments: Commitments,
    /// Future work.
    pub(crate) initiatives: Vec<InitiativeData>,
    /// The build-health tile.
    pub(crate) health: Option<BuildHealth>,
    /// Source health.
    pub(crate) sources: Vec<SourceStatus>,
    /// Warnings.
    pub(crate) notes: Vec<Note>,
}

/// Runs `f` over `items` on up to `workers` threads, keeping input order.
pub(crate) fn par_map<T: Sync, R: Send>(
    items: &[T],
    workers: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|s| {
        for _ in 0..workers.clamp(1, items.len().max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    done.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((i, r));
                }
            });
        }
    });
    let mut results = done
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    results.sort_by_key(|(i, _)| *i);
    results.into_iter().map(|(_, r)| r).collect()
}

/// `"a", "b"` for a JQL `in (...)` list.
fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("\"{}\"", n.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Keys per fixVersion from issues that carry `fixVersions`.
pub(crate) fn keys_by_version(issues: &[Value]) -> KeysByVersion {
    let mut out = KeysByVersion::new();
    for issue in issues {
        let Some(key) = str_at(issue, "/key").and_then(|k| k.parse::<IssueKey>().ok()) else {
            warn!("skipping an issue without a valid key in a closure count");
            continue;
        };
        for name in issue
            .pointer("/fields/fixVersions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| str_at(v, "/name"))
        {
            out.entry(name.to_string()).or_default().insert(key.clone());
        }
    }
    out
}

/// The tracked EX fixVersions: dated, unreleased EXA6 ones, then the
/// configured Beta and GA ones that Jira has. A JQL that names a version Jira
/// does not have fails, so missing ones are left out (see `missing_versions`).
pub(crate) fn tracked_versions(cfg: &Config, ex_versions: &[Version]) -> Vec<String> {
    let mut exa6: Vec<&Version> = ex_versions
        .iter()
        .filter(|v| {
            v.name.starts_with(&cfg.exa6_prefix)
                && !v.released
                && !v.archived
                && v.release.is_some()
        })
        .collect();
    exa6.sort_by_key(|v| v.release);
    let known: HashSet<&str> = ex_versions.iter().map(|v| v.name.as_str()).collect();
    let mut names: Vec<String> = exa6.into_iter().map(|v| v.name.clone()).collect();
    names.extend(
        exa7_versions(cfg)
            .filter(|n| known.contains(n.as_str()))
            .cloned(),
    );
    names
}

/// The configured Beta and GA fixVersions that Jira does not have.
pub(crate) fn missing_versions(cfg: &Config, ex_versions: &[Version]) -> Vec<String> {
    let known: HashSet<&str> = ex_versions.iter().map(|v| v.name.as_str()).collect();
    exa7_versions(cfg)
        .filter(|n| !known.contains(n.as_str()))
        .cloned()
        .collect()
}

fn exa7_versions(cfg: &Config) -> impl Iterator<Item = &String> {
    std::iter::once(&cfg.exa7_beta_version).chain(&cfg.exa7_ga_versions)
}

/// The view a configured EXA7 fixVersion belongs to.
fn exa7_view(cfg: &Config, name: &str) -> ViewId {
    if name == cfg.exa7_beta_version {
        ViewId::Exa7Beta
    } else {
        ViewId::Exa7Ga
    }
}

/// The tracked fixVersions of each EX release view.
pub(crate) fn release_versions(cfg: &Config, tracked: &[String]) -> [(ViewId, Vec<String>); 3] {
    let pick = |wanted: &dyn Fn(&str) -> bool| -> Vec<String> {
        tracked.iter().filter(|n| wanted(n)).cloned().collect()
    };
    [
        (ViewId::Exa6, pick(&|n| n.starts_with(&cfg.exa6_prefix))),
        (ViewId::Exa7Beta, pick(&|n| n == cfg.exa7_beta_version)),
        (
            ViewId::Exa7Ga,
            pick(&|n| cfg.exa7_ga_versions.iter().any(|g| g == n)),
        ),
    ]
}

/// The unreleased EXA SD phases, earliest first.
pub(crate) fn phases<'a>(cfg: &Config, sd_versions: &'a [Version]) -> Vec<&'a Version> {
    let mut phases: Vec<&Version> = sd_versions
        .iter()
        .filter(|v| v.name.starts_with(&cfg.exa_sd_phase_prefix) && !v.released && !v.archived)
        .collect();
    phases.sort_by(|a, b| a.release.cmp(&b.release).then(a.name.cmp(&b.name)));
    phases
}

/// What a query's rows are for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum QueryId {
    /// Open tickets of `customer_sources[i]`.
    Customers(usize),
    ExOpen,
    ExClosed,
    /// Items added to one release view's fixVersions.
    Added(ViewId),
    ExUnscheduled,
    SdOpen,
    SdClosed,
}

impl QueryId {
    /// Without it the release verdicts would be wrong, not just incomplete,
    /// so its failure fails the refresh and the last snapshot stays.
    fn required(self) -> bool {
        matches!(
            self,
            Self::ExOpen | Self::ExClosed | Self::SdOpen | Self::SdClosed
        )
    }

    /// Its failure leaves its views' verdicts on partial data.
    fn feeds_verdicts(self) -> bool {
        matches!(self, Self::Customers(_))
    }
}

/// A query the refresh runs.
struct Query {
    id: QueryId,
    name: String,
    instance: Instance,
    jql: String,
    /// `None` reads tickets; `Some(fields)` reads raw issues with those fields.
    raw_fields: Option<&'static str>,
    /// The views its rows feed.
    views: Views,
}

/// What a PM page is read for.
#[derive(Clone, Debug, Default, PartialEq)]
struct PageUse {
    /// The views that show it.
    views: Views,
    /// A verdict is measured against its date.
    dated: bool,
}

/// The PM pages to read, with what each is read for.
fn page_uses(cfg: &Config) -> BTreeMap<u64, PageUse> {
    let pages = &cfg.pm_pages;
    let mut uses: BTreeMap<u64, PageUse> = BTreeMap::new();
    let mut add = |id: u64, views: &[ViewId], dated: bool| {
        let entry = uses.entry(id).or_default();
        entry.views.extend(views);
        entry.dated |= dated;
    };
    add(pages.exa6, &[ViewId::Exa6], true);
    add(pages.exa7, &[ViewId::Exa7Beta, ViewId::Exa7Ga], true);
    add(pages.exa_sd, &[ViewId::ExaSd], true);
    add(pages.gcp, &[ViewId::Gcp], false);
    for i in &cfg.initiatives {
        if let Some(id) = i.pm_page {
            add(id, &initiative_views(i), true);
        }
    }
    uses
}

/// The views that show an initiative.
fn initiative_views(config: &InitiativeConfig) -> Vec<ViewId> {
    if config.in_exa_sd {
        vec![ViewId::Future, ViewId::ExaSd]
    } else {
        vec![ViewId::Future]
    }
}

/// Either kind of query result.
enum Rows {
    Tickets(Tickets),
    Raw(Vec<Value>),
}

/// One unit of work for the shared Atlassian worker pool.
#[derive(Clone, Copy)]
enum Job<'a> {
    Query(&'a Query),
    Page(u64),
    Initiative(&'a InitiativeConfig),
}

/// A finished job and what it read.
enum Done<'a> {
    Query(&'a Query, Result<Rows>),
    Page(u64, Result<PmPage>),
    Initiative(&'a InitiativeConfig, Result<(InitiativeData, usize)>),
}

fn run<'a>(jira: &Jira, parser: &PmParser, cfg: &Config, job: Job<'a>) -> Done<'a> {
    match job {
        Job::Query(q) => Done::Query(
            q,
            match q.raw_fields {
                None => jira.tickets(q.instance, &q.jql).map(Rows::Tickets),
                Some(fields) => jira.search(q.instance, &q.jql, fields).map(Rows::Raw),
            },
        ),
        Job::Page(id) => Done::Page(id, confluence::fetch(jira, parser, id)),
        Job::Initiative(i) => Done::Initiative(i, initiative(jira, cfg, i)),
    }
}

/// What failed, per source, for the source dots.
#[derive(Debug, Default, PartialEq)]
struct SourceErrors {
    cloud: Vec<String>,
    whamcloud: Vec<String>,
    confluence: Vec<String>,
}

impl SourceErrors {
    fn jira(&mut self, instance: Instance, error: String) {
        match instance {
            Instance::Cloud => self.cloud.push(error),
            Instance::Whamcloud => self.whamcloud.push(error),
        }
    }
}

fn source(name: &str, errors: &[String]) -> SourceStatus {
    SourceStatus {
        name: name.into(),
        error: (!errors.is_empty()).then(|| errors.join("; ")),
    }
}

/// "EX-1, EX-2 and 3 more".
fn named(keys: &[&IssueKey]) -> String {
    let shown: Vec<String> = keys
        .iter()
        .take(KEYS_NAMED)
        .map(ToString::to_string)
        .collect();
    match keys.len().saturating_sub(KEYS_NAMED) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

/// Reads everything one refresh needs.
pub(crate) fn gather(cfg: &Config) -> Result<RawData> {
    let today = Local::now().date_naive();
    let jira = Jira::from_tool_config(&cfg.jira_config)?;
    let parser = PmParser::new()?;
    let ex_versions = jira
        .versions(&cfg.ex_project)
        .with_context(|| format!("reading {} versions", cfg.ex_project))?;
    let sd_versions = jira
        .versions(&cfg.exa_sd_project)
        .context("reading EXA SD versions")?;
    let mut errors = SourceErrors::default();
    let mut notes = Vec::new();
    let tracked = tracked_versions(cfg, &ex_versions);
    for name in missing_versions(cfg, &ex_versions) {
        notes.push(Note::gap(
            [exa7_view(cfg, &name)],
            format!("Jira has no fixVersion {name}; fix its name in the config."),
        ));
    }
    let phase_names: Vec<String> = phases(cfg, &sd_versions)
        .iter()
        .map(|v| v.name.clone())
        .collect();
    let queries = queries(cfg, &tracked, &phase_names);
    let pages = page_uses(cfg);

    let mut jobs: Vec<Job<'_>> = queries.iter().map(Job::Query).collect();
    jobs.extend(pages.keys().copied().map(Job::Page));
    jobs.extend(cfg.initiatives.iter().map(Job::Initiative));
    // Queries, pages and initiatives share one pool, so `workers` is the most
    // requests in flight to the Atlassian site; EXA Reporter is another host.
    let (done, health) = std::thread::scope(|s| {
        let health = s.spawn(|| reporter::fetch(cfg));
        let done = par_map(&jobs, cfg.workers, |job| run(&jira, &parser, cfg, *job));
        (done, health.join())
    });
    let Collected {
        mut rows,
        pm_pages,
        mut initiatives,
        lists_complete,
    } = collect(done, &pages, &mut errors, &mut notes)?;

    let health = match health.map_err(|_| anyhow!("the EXA Reporter thread panicked"))? {
        Ok(h) => Some(h),
        Err(e) => {
            warn!("EXA Reporter: {e:#}");
            notes.push(Note::info(ViewId::RELEASES, format!("EXA Reporter: {e:#}")));
            None
        }
    };

    // Only GCP and NVIDIA tickets carry committed dates.
    let commitments = commitments::load(&cfg.commitments_file).unwrap_or_else(|e| {
        warn!("{e:#}");
        notes.push(Note::gap(
            [ViewId::Gcp, ViewId::Nvidia],
            format!("Committed dates not read: {e:#}"),
        ));
        Commitments::new()
    });

    let mut customers: Vec<Ticket> = Vec::new();
    for (i, s) in cfg.customer_sources.iter().enumerate() {
        if let Some(Rows::Tickets(t)) = rows.remove(&QueryId::Customers(i)) {
            customers.extend(t.items.into_iter().map(|t| Ticket {
                source: Some(s.name.clone()),
                ..t
            }));
        }
    }
    let (mut ex_open, ex_closed) = if tracked.is_empty() {
        notes.push(Note::gap(
            ViewId::RELEASES,
            "No EX release is tracked: Jira has none of the configured versions.".into(),
        ));
        (Vec::new(), KeysByVersion::new())
    } else {
        (
            take_tickets(&mut rows, QueryId::ExOpen)?,
            keys_by_version(&take_raw(&mut rows, QueryId::ExClosed)?),
        )
    };
    let unscheduled = take_raw(&mut rows, QueryId::ExUnscheduled)
        .ok()
        .map(|r| r.len());
    let mut sd_open = take_tickets(&mut rows, QueryId::SdOpen)?;
    let sd_closed = if phase_names.is_empty() {
        KeysByVersion::new()
    } else {
        keys_by_version(&take_raw(&mut rows, QueryId::SdClosed)?)
    };
    // A view with no version to read has nothing added; a failed read is unknown.
    let added = release_versions(cfg, &tracked)
        .into_iter()
        .chain([(ViewId::ExaSd, phase_names.clone())])
        .map(|(view, versions)| {
            let count = if versions.is_empty() {
                Some(0)
            } else {
                take_raw(&mut rows, QueryId::Added(view))
                    .ok()
                    .map(|r| r.len())
            };
            (view, count)
        })
        .collect();

    let fixes = linked_fixes(&jira, cfg, &customers);
    if !fixes.unread.is_empty() {
        let unread: Vec<&IssueKey> = fixes.unread.iter().collect();
        notes.push(Note::info(
            ViewId::CUSTOMERS,
            format!("Linked fixes not read: {}. Tickets linking to them are not counted as fix delivered.", named(&unread)),
        ));
    }
    if !fixes.errors.is_empty() {
        notes.push(Note::gap(
            ViewId::CUSTOMERS,
            "Some linked fixes could not be read; fix dates may be missing.".into(),
        ));
    }
    errors.cloud.extend(fixes.errors);

    // Idle time for every customer ticket, and for the release items a rule reads it for.
    let small = small_release_keys(
        cfg,
        &[
            Scoped {
                items: &ex_open,
                versions: &tracked,
            },
            Scoped {
                items: &sd_open,
                versions: &phase_names,
            },
        ],
        &initiatives,
    );
    let customer_keys: BTreeSet<IssueKey> = customers.iter().map(|t| t.key.clone()).collect();
    let mut wanted: Vec<&mut Ticket> = customers.iter_mut().collect();
    let release_items = ex_open
        .iter_mut()
        .chain(sd_open.iter_mut())
        .chain(initiatives.iter_mut().flat_map(|d| d.open.iter_mut()));
    wanted.extend(release_items.filter(|t| rules::needs_idle(t, today) || small.contains(&t.key)));
    let failed = attach_comments(&jira, cfg, today, &mut wanted, lists_complete);
    report_comment_failures(&failed, &customer_keys, &mut errors, &mut notes);

    let reporter_status = SourceStatus {
        name: "EXA Reporter".into(),
        error: match &health {
            Some(h) => Some(h.errors().join("; ")).filter(|e| !e.is_empty()),
            None => Some("not read".into()),
        },
    };
    let sources = vec![
        source("Jira cloud", &errors.cloud),
        source("Jira Whamcloud", &errors.whamcloud),
        source("Confluence", &errors.confluence),
        reporter_status,
    ];

    Ok(RawData {
        today,
        ex_versions,
        sd_versions,
        customers,
        ex_open,
        sd_open,
        ex_closed,
        sd_closed,
        added,
        unscheduled,
        pm_pages,
        fixes: fixes.found,
        commitments,
        initiatives,
        health,
        sources,
        notes,
    })
}

/// What the shared pool read, sorted by kind.
struct Collected {
    rows: HashMap<QueryId, Rows>,
    pm_pages: HashMap<u64, PmPage>,
    initiatives: Vec<InitiativeData>,
    /// Every ticket list was read in full, so a ticket missing from them
    /// this time is gone, not just unread.
    lists_complete: bool,
}

/// Sorts finished jobs, turning failures and unreadable issues into notes and
/// source errors. A failed required query fails the refresh. Data a verdict
/// rests on that is missing (a list, an unreadable issue in it, a dated PM
/// page) is a gap in the views it feeds.
fn collect(
    done: Vec<Done<'_>>,
    pages: &BTreeMap<u64, PageUse>,
    errors: &mut SourceErrors,
    notes: &mut Vec<Note>,
) -> Result<Collected> {
    let mut found = Collected {
        rows: HashMap::new(),
        pm_pages: HashMap::new(),
        initiatives: Vec::new(),
        lists_complete: true,
    };
    let skipped_note =
        |name: &str, n: usize| format!("{name}: {n} issues could not be read and are left out");
    for finished in done {
        match finished {
            Done::Query(q, Ok(rows)) => {
                if let Rows::Tickets(t) = &rows
                    && t.skipped > 0
                {
                    found.lists_complete = false;
                    let text = skipped_note(&q.name, t.skipped);
                    notes.push(if q.id.required() || q.id.feeds_verdicts() {
                        Note::gap(q.views.clone(), text)
                    } else {
                        Note::info(q.views.clone(), text)
                    });
                }
                found.rows.insert(q.id, rows);
            }
            Done::Query(q, Err(e)) => {
                if q.id.required() {
                    return Err(e.context(format!("Jira query '{}'", q.name)));
                }
                warn!("query '{}' failed: {e:#}", q.name);
                errors.jira(q.instance, format!("{}: {e:#}", q.name));
                if q.raw_fields.is_none() {
                    found.lists_complete = false;
                }
                let text = format!("{}: not read ({e:#})", q.name);
                notes.push(if q.id.feeds_verdicts() {
                    Note::gap(q.views.clone(), text)
                } else {
                    Note::info(q.views.clone(), text)
                });
            }
            Done::Page(id, Ok(page)) => {
                if let Some(used) = pages.get(&id).filter(|u| u.dated)
                    && page.info.target.is_none()
                {
                    notes.push(Note::info(
                        used.views.clone(),
                        format!(
                            "PM page \"{}\" has no Target Date row; its dates fall back to Jira.",
                            page.info.title
                        ),
                    ));
                }
                found.pm_pages.insert(id, page);
            }
            Done::Page(id, Err(e)) => {
                warn!("PM page {id}: {e:#}");
                errors.confluence.push(format!("page {id}: {e:#}"));
                let used = pages.get(&id).cloned().unwrap_or_default();
                notes.push(if used.dated {
                    Note::gap(
                        used.views,
                        format!("PM page {id} could not be read, so the Jira date is used."),
                    )
                } else {
                    Note::info(used.views, format!("PM page {id} could not be read."))
                });
            }
            Done::Initiative(_, Ok((data, skipped))) => {
                if skipped > 0 {
                    found.lists_complete = false;
                    notes.push(Note::gap(
                        initiative_views(&data.config),
                        skipped_note(&data.config.name, skipped),
                    ));
                }
                found.initiatives.push(data);
            }
            Done::Initiative(config, Err(e)) => {
                warn!("initiative '{}': {e:#}", config.name);
                errors.cloud.push(format!("{}: {e:#}", config.name));
                found.lists_complete = false;
                notes.push(Note::gap(
                    initiative_views(config),
                    format!("{}: not read ({e:#})", config.name),
                ));
            }
        }
    }
    Ok(found)
}

/// Comment reads that failed: a source error per Jira, and a note when
/// customer tickets are among them (their idle time drives the verdict).
fn report_comment_failures(
    failed: &[(Instance, IssueKey)],
    customer_keys: &BTreeSet<IssueKey>,
    errors: &mut SourceErrors,
    notes: &mut Vec<Note>,
) {
    for instance in [Instance::Cloud, Instance::Whamcloud] {
        let keys: Vec<&IssueKey> = failed
            .iter()
            .filter(|(i, _)| *i == instance)
            .map(|(_, k)| k)
            .collect();
        if !keys.is_empty() {
            errors.jira(
                instance,
                format!("newest comment not read for {}", named(&keys)),
            );
        }
    }
    let customers_failed: Vec<&IssueKey> = failed
        .iter()
        .map(|(_, k)| k)
        .filter(|k| customer_keys.contains(*k))
        .collect();
    if !customers_failed.is_empty() {
        notes.push(Note::info(
            ViewId::CUSTOMERS,
            format!("Newest comment not read for {}; their idle time is unknown or from an earlier refresh.", named(&customers_failed)),
        ));
    }
}

fn take_tickets(rows: &mut HashMap<QueryId, Rows>, id: QueryId) -> Result<Vec<Ticket>> {
    match rows.remove(&id) {
        Some(Rows::Tickets(t)) => Ok(t.items),
        _ => Err(anyhow!("no tickets for {id:?}")),
    }
}

fn take_raw(rows: &mut HashMap<QueryId, Rows>, id: QueryId) -> Result<Vec<Value>> {
    match rows.remove(&id) {
        Some(Rows::Raw(r)) => Ok(r),
        _ => Err(anyhow!("no issues for {id:?}")),
    }
}

/// Items of `project` in `versions` added in the last `days`: created, or
/// moved into one of `versions` from elsewhere. Only these versions are
/// named, so an item that has long been in one of them and was just added to
/// another view's version does not count here.
fn added_jql(project: &str, versions: &[String], days: i64) -> String {
    let moved_in = versions.iter().map(|v| {
        format!(
            "fixVersion CHANGED TO {} AFTER -{days}d",
            quoted(std::slice::from_ref(v))
        )
    });
    let clause = std::iter::once(format!("created >= -{days}d"))
        .chain(moved_in)
        .collect::<Vec<_>>()
        .join(" OR ");
    format!(
        "project = {project} AND fixVersion in ({}) AND ({clause})",
        quoted(versions)
    )
}

fn queries(cfg: &Config, tracked: &[String], phase_names: &[String]) -> Vec<Query> {
    let w = cfg.closed_window_days;
    let (ex, sd) = (&cfg.ex_project, &cfg.exa_sd_project);
    let releases = Views::from(ViewId::RELEASES);
    let query = |id: QueryId,
                 name: &str,
                 instance: Instance,
                 jql: String,
                 raw_fields: Option<&'static str>,
                 views: Views| Query {
        id,
        name: name.to_string(),
        instance,
        jql,
        raw_fields,
        views,
    };
    let mut list: Vec<Query> = cfg
        .customer_sources
        .iter()
        .enumerate()
        .map(|(i, s)| {
            query(
                QueryId::Customers(i),
                &s.name,
                s.instance,
                s.jql(),
                None,
                cfg.source_views(s),
            )
        })
        .collect();
    if !tracked.is_empty() {
        let fv = quoted(tracked);
        list.extend([
            query(
                QueryId::ExOpen,
                "EX open",
                Instance::Cloud,
                format!("project = {ex} AND fixVersion in ({fv}) AND statusCategory != Done"),
                None,
                releases.clone(),
            ),
            query(
                QueryId::ExClosed,
                "EX closed",
                Instance::Cloud,
                format!("project = {ex} AND fixVersion in ({fv}) AND statusCategory = Done AND resolved >= -{w}d"),
                Some("fixVersions"),
                releases,
            ),
        ]);
    }
    for (view, versions) in release_versions(cfg, tracked) {
        if !versions.is_empty() {
            list.push(query(
                QueryId::Added(view),
                &format!("{} added", view.label()),
                Instance::Cloud,
                added_jql(ex, &versions, w),
                Some("status"),
                Views::from([view]),
            ));
        }
    }
    let sd_views = Views::from([ViewId::ExaSd]);
    list.extend([
        query(
            QueryId::ExUnscheduled,
            "EX unscheduled",
            Instance::Cloud,
            format!(
                "project = {ex} AND fixVersion = {} AND statusCategory != Done",
                quoted(std::slice::from_ref(&cfg.exa6_unscheduled))
            ),
            Some("status"),
            Views::from([ViewId::Exa6]),
        ),
        query(
            QueryId::SdOpen,
            "EXA SD open",
            Instance::Cloud,
            format!("project = {sd} AND statusCategory != Done"),
            None,
            sd_views.clone(),
        ),
    ]);
    if !phase_names.is_empty() {
        let ph = quoted(phase_names);
        list.push(query(
            QueryId::SdClosed,
            "EXA SD closed",
            Instance::Cloud,
            format!("project = {sd} AND fixVersion in ({ph}) AND statusCategory = Done AND resolved >= -{w}d"),
            Some("fixVersions"),
            sd_views.clone(),
        ));
        list.push(query(
            QueryId::Added(ViewId::ExaSd),
            "EXA SD added",
            Instance::Cloud,
            added_jql(sd, phase_names, w),
            Some("status"),
            sd_views,
        ));
    }
    list
}

/// Reads one initiative: its epics, its open items and its recent closures.
/// Also returns how many issues could not be read.
fn initiative(
    jira: &Jira,
    cfg: &Config,
    config: &InitiativeConfig,
) -> Result<(InitiativeData, usize)> {
    let epic_list = config
        .epics
        .iter()
        .map(IssueKey::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let scope = match (&config.jql, config.epics.is_empty()) {
        (Some(jql), _) => jql.clone(),
        (None, false) => format!("parent in ({epic_list})"),
        (None, true) => return Err(anyhow!("no epics and no query")),
    };
    let epic_tickets = if config.epics.is_empty() {
        Tickets::default()
    } else {
        jira.tickets(Instance::Cloud, &format!("key in ({epic_list})"))?
    };
    let open = jira.tickets(
        Instance::Cloud,
        &format!("({scope}) AND statusCategory != Done"),
    )?;
    let closed = jira.search(
        Instance::Cloud,
        &format!(
            "({scope}) AND statusCategory = Done AND resolved >= -{}d",
            cfg.closed_window_days
        ),
        "status",
    )?;
    let data = InitiativeData {
        config: config.clone(),
        epics: epic_tickets.items,
        open: open.items,
        closed_in_window: closed.len(),
    };
    Ok((data, epic_tickets.skipped + open.skipped))
}

/// Open items of a project, and the fixVersions that split them into
/// releases or phases.
struct Scoped<'a> {
    items: &'a [Ticket],
    versions: &'a [String],
}

/// Keys of items in releases, phases and initiatives small enough to be
/// judged per item, which reads their idle time.
fn small_release_keys(
    cfg: &Config,
    releases: &[Scoped<'_>],
    initiatives: &[InitiativeData],
) -> BTreeSet<IssueKey> {
    let by_version = releases.iter().flat_map(|scoped| {
        scoped.versions.iter().flat_map(move |v| {
            let in_v: Vec<&Ticket> = scoped
                .items
                .iter()
                .filter(|t| t.fix_versions.contains(v))
                .collect();
            if in_v.len() <= cfg.small_release_max {
                in_v
            } else {
                Vec::new()
            }
        })
    });
    let small_initiatives = initiatives
        .iter()
        .filter(|d| d.open.len() <= cfg.small_release_max)
        .flat_map(|d| &d.open);
    by_version
        .chain(small_initiatives)
        .map(|t| t.key.clone())
        .collect()
}

/// Linked fixes as read.
#[derive(Debug, Default, PartialEq)]
struct Fixes {
    found: HashMap<IssueKey, Ticket>,
    /// Links that could not be read: unknown, hidden or failed.
    unread: BTreeSet<IssueKey>,
    /// Failures other than an unknown key.
    errors: Vec<String>,
}

/// Reads every linked fix of the customer tickets on Jira cloud. Links on
/// Whamcloud tickets point at stale copies, so only the key is used.
fn linked_fixes(jira: &Jira, cfg: &Config, customers: &[Ticket]) -> Fixes {
    let keys: BTreeSet<&IssueKey> = customers
        .iter()
        .flat_map(|t| rules::fix_keys(cfg, t))
        .collect();
    let keys: Vec<&IssueKey> = keys.into_iter().collect();
    let chunks: Vec<&[&IssueKey]> = keys.chunks(jira::PAGE_SIZE).collect();
    let mut fixes = Fixes::default();
    for chunk in par_map(&chunks, cfg.workers, |chunk| {
        read_fixes(chunk, |list| {
            jira.tickets(Instance::Cloud, &format!("key in ({list})"))
        })
    }) {
        fixes.found.extend(chunk.found);
        fixes.unread.extend(chunk.unread);
        fixes.errors.extend(chunk.errors);
    }
    fixes
}

/// One chunk of linked fixes; `read` runs `key in (<list>)`. Jira refuses the
/// whole query with HTTP 400 when one key is unknown or hidden; only then is
/// each key read on its own. Any other failure leaves the chunk unread.
fn read_fixes(chunk: &[&IssueKey], read: impl Fn(&str) -> Result<Tickets>) -> Fixes {
    let mut fixes = Fixes::default();
    let all = chunk
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    match read(&all) {
        Ok(t) => fixes
            .found
            .extend(t.items.into_iter().map(|t| (t.key.clone(), t))),
        Err(e) if http::status_of(&e) == Some(400) => {
            for key in chunk {
                match read(key.as_str()) {
                    Ok(t) => fixes
                        .found
                        .extend(t.items.into_iter().map(|t| (t.key.clone(), t))),
                    Err(e) if http::status_of(&e) == Some(400) => {
                        warn!("linked fix {key} is unknown or hidden")
                    }
                    Err(e) => fixes.errors.push(format!("linked fix {key}: {e:#}")),
                }
            }
        }
        Err(e) => fixes.errors.push(format!("linked fixes: {e:#}")),
    }
    fixes.unread = chunk
        .iter()
        .filter(|k| !fixes.found.contains_key(**k))
        .map(|k| (*k).clone())
        .collect();
    fixes
}

/// Comment-cache key: the same key can exist on both Jira instances.
fn cache_key(t: &Ticket) -> String {
    format!("{}:{}", t.instance, t.key)
}

/// A ticket whose newest comment must be read again.
struct StaleComment {
    instance: Instance,
    key: IssueKey,
    updated: String,
    cache_key: String,
}

/// Tickets the cache cannot answer (new, or changed since), once each.
fn stale_comments(
    cache: &HashMap<String, CachedComment>,
    tickets: &[&mut Ticket],
) -> Vec<StaleComment> {
    let mut seen = BTreeSet::new();
    let mut stale = Vec::new();
    for t in tickets {
        let k = cache_key(t);
        if cache.get(&k).is_none_or(|c| c.updated != t.updated) && seen.insert(k.clone()) {
            stale.push(StaleComment {
                instance: t.instance,
                key: t.key.clone(),
                updated: t.updated.clone(),
                cache_key: k,
            });
        }
    }
    stale
}

/// The comment cache to keep: the entries still in use, or all of them when
/// a ticket list was not read in full (its tickets are unread, not gone).
fn kept_comments(
    cache: HashMap<String, CachedComment>,
    in_use: HashMap<String, CachedComment>,
    lists_complete: bool,
) -> HashMap<String, CachedComment> {
    if lists_complete { in_use } else { cache }
}

/// Gives every ticket the cached newest comment and its idle time. A ticket
/// listed twice (a release item that is also an initiative item) gets the
/// same answer both times. Returns the entries still in use.
fn apply_comments(
    cache: &HashMap<String, CachedComment>,
    tickets: &mut [&mut Ticket],
    today: NaiveDate,
) -> HashMap<String, CachedComment> {
    let mut kept = HashMap::new();
    for t in tickets.iter_mut() {
        let k = cache_key(t);
        if let Some(c) = cache.get(&k) {
            t.last_comment.clone_from(&c.comment);
            t.idle_days = rules::idle_days(t, today);
            kept.insert(k, c.clone());
        }
    }
    kept
}

/// Fills in the newest human comment and idle time, reusing the cache while a
/// ticket is unchanged. A failed read keeps the earlier answer, if any.
/// Returns the tickets whose comments could not be read.
fn attach_comments(
    jira: &Jira,
    cfg: &Config,
    today: NaiveDate,
    tickets: &mut [&mut Ticket],
    lists_complete: bool,
) -> Vec<(Instance, IssueKey)> {
    let path = cfg.data_file(store::COMMENTS_FILE);
    let mut cache = store::load_comments(&path, &cfg.bot_authors);
    let stale = stale_comments(&cache, tickets);
    let read = par_map(&stale, cfg.workers, |s| {
        jira.latest_human_comment(s.instance, &s.key, &cfg.bot_authors)
    });
    let mut failed = Vec::new();
    for (s, r) in stale.into_iter().zip(read) {
        match r {
            Ok(comment) => {
                cache.insert(
                    s.cache_key,
                    CachedComment {
                        updated: s.updated,
                        comment,
                    },
                );
            }
            Err(e) => {
                warn!("newest comment of {}: {e:#}", s.key);
                failed.push((s.instance, s.key));
            }
        }
    }
    let in_use = apply_comments(&cache, tickets, today);
    store::save_comments(
        &path,
        &cfg.bot_authors,
        kept_comments(cache, in_use, lists_complete),
    );
    failed
}

/// A ticket whose Gerrit changes are looked up.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Watched {
    /// The ticket.
    pub(crate) key: IssueKey,
    /// Keys searched for in commit messages: the ticket, its copies and its
    /// linked tickets in `gerrit_key_projects`.
    pub(crate) search: Vec<IssueKey>,
}

/// Gerrit changes found for the watched tickets.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Patches {
    /// Changes per ticket.
    pub(crate) map: PatchMap,
    /// Searches and tag lookups that failed.
    pub(crate) errors: Vec<String>,
}

/// Gerrit changes for the watched tickets. Changes are searched on every
/// refresh (a linked ticket's change can merge without the watched ticket
/// changing); only the first tag of a merged revision, which never changes
/// once set, is cached.
pub(crate) fn patches(cfg: &Config, watched: &[Watched]) -> Result<Patches> {
    let gerrit = Gerrit::from_env_file(&cfg.gerrit_env)?;
    let branches: Vec<String> = cfg.tag_families.iter().map(|f| f.branch.clone()).collect();
    let keys: Vec<IssueKey> = watched
        .iter()
        .flat_map(|w| w.search.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let chunks: Vec<&[IssueKey]> = keys.chunks(GERRIT_KEYS_PER_SEARCH).collect();
    let mut errors = Vec::new();
    let mut changes: BTreeMap<u64, Change> = BTreeMap::new();
    for r in par_map(&chunks, cfg.gerrit_workers, |chunk| {
        gerrit.changes_mentioning(chunk, &cfg.gerrit_project, &branches)
    }) {
        match r {
            Ok(found) => changes.extend(found.into_iter().map(|c| (c.number, c))),
            Err(e) => errors.push(format!("change search: {e:#}")),
        }
    }

    let path = cfg.data_file(store::TAGS_FILE);
    let mut tags = store::load_tags(&path);
    let untagged: Vec<(&str, &TagFamily)> = changes
        .values()
        .filter(|c| c.status == ChangeStatus::Merged)
        .filter_map(|c| {
            Some((
                c.revision.as_deref()?,
                cfg.tag_families.iter().find(|f| f.branch == c.branch)?,
            ))
        })
        .filter(|(revision, _)| !tags.contains_key(*revision))
        .collect();
    let looked_up = par_map(&untagged, cfg.gerrit_workers, |(revision, family)| {
        gerrit
            .tags_containing(&cfg.gerrit_project, revision)
            .map(|t| gerrit::first_tag(&t, &family.prefix))
    });
    for ((revision, _), r) in untagged.iter().zip(looked_up) {
        match r {
            Ok(Some(tag)) => {
                tags.insert((*revision).to_string(), tag);
            }
            Ok(None) => {}
            Err(e) => errors.push(format!("tags of {revision}: {e:#}")),
        }
    }
    let in_use: HashSet<&str> = changes
        .values()
        .filter_map(|c| c.revision.as_deref())
        .collect();
    let tags = pruned_tags(tags, &in_use, errors.is_empty());
    store::save_tags(&path, &tags);

    let found = changes
        .into_values()
        .map(|c| {
            let first_tag = c.revision.as_ref().and_then(|r| tags.get(r)).cloned();
            let url = gerrit.change_url(&cfg.gerrit_project, c.number)?;
            Ok((
                c.keys,
                PatchRef {
                    number: c.number,
                    url,
                    branch: c.branch,
                    status: c.status,
                    first_tag,
                },
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Patches {
        map: patches_by_ticket(watched, &found),
        errors,
    })
}

/// The tag cache to keep: entries for revisions still in use, or all of them
/// when a search failed (its changes are missing this time, not gone).
fn pruned_tags(
    mut tags: HashMap<String, String>,
    in_use: &HashSet<&str>,
    complete: bool,
) -> HashMap<String, String> {
    if complete {
        tags.retain(|revision, _| in_use.contains(revision.as_str()));
    }
    tags
}

/// Each watched ticket's changes: those whose commit message names one of its keys.
fn patches_by_ticket(watched: &[Watched], changes: &[(BTreeSet<IssueKey>, PatchRef)]) -> PatchMap {
    watched
        .iter()
        .map(|w| {
            let mine = changes
                .iter()
                .filter(|(keys, _)| w.search.iter().any(|k| keys.contains(k)))
                .map(|(_, p)| p.clone())
                .collect();
            (w.key.clone(), mine)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Comment, PmRef};
    use crate::rules::tests::{d, ticket, today};
    use serde_json::json;
    use test_case::test_case;
    use url::Url;

    #[test]
    fn par_map_keeps_input_order() {
        let input: Vec<u64> = (0..50).collect();

        let out = par_map(&input, 8, |n| n * 2);

        assert_eq!(out, input.iter().map(|n| n * 2).collect::<Vec<_>>());
        assert!(par_map(&Vec::<u64>::new(), 8, |n| *n).is_empty());
    }

    #[test]
    fn keys_by_version_groups_closures() {
        let issues = vec![
            json!({"key": "EX-1", "fields": {"fixVersions": [{"name": "ES6.3.10"}, {"name": "ES7.0.0"}]}}),
            json!({"key": "EX-2", "fields": {"fixVersions": [{"name": "ES6.3.10"}]}}),
            json!({"key": "bad", "fields": {"fixVersions": [{"name": "ES6.3.10"}]}}),
        ];

        let map = keys_by_version(&issues);

        assert_eq!(map.get("ES6.3.10").map(BTreeSet::len), Some(2));
        assert_eq!(map.get("ES7.0.0").map(BTreeSet::len), Some(1));
    }

    fn version(name: &str, release: Option<NaiveDate>, released: bool) -> Version {
        Version {
            name: name.into(),
            release,
            start: None,
            released,
            archived: false,
        }
    }

    fn ex_versions() -> Vec<Version> {
        vec![
            version("ES6.3.10", Some(d(2026, 12, 15)), false),
            version("ES6.3.9_C1", Some(d(2026, 11, 6)), false),
            version("ES6.3.9", Some(d(2026, 9, 14)), true),
            version("ES6.3.5", None, false),
            version("ES6.3.x (TBD)", None, false),
            version("ES7.0.0_Beta", Some(d(2026, 11, 30)), false),
            version("ES7.0.0", None, false),
            version("ES7.1.0", Some(d(2027, 1, 29)), false),
        ]
    }

    #[test]
    fn tracked_versions_take_dated_unreleased_exa6_then_exa7() {
        assert_eq!(
            tracked_versions(&Config::default(), &ex_versions()),
            [
                "ES6.3.9_C1",
                "ES6.3.10",
                "ES7.0.0_Beta",
                "ES7.0.0",
                "ES7.1.0"
            ]
        );
        assert!(missing_versions(&Config::default(), &ex_versions()).is_empty());
    }

    #[test]
    fn a_configured_version_jira_lacks_is_left_out_and_reported() {
        let renamed: Vec<Version> = ex_versions()
            .into_iter()
            .filter(|v| v.name != "ES7.1.0")
            .collect();

        assert_eq!(
            tracked_versions(&Config::default(), &renamed),
            ["ES6.3.9_C1", "ES6.3.10", "ES7.0.0_Beta", "ES7.0.0"]
        );
        assert_eq!(missing_versions(&Config::default(), &renamed), ["ES7.1.0"]);
    }

    #[test]
    fn phases_are_unreleased_and_in_date_order() {
        let versions = vec![
            version("NVMesh-P3", Some(d(2027, 1, 29)), false),
            version("NVMesh-P1", Some(d(2026, 7, 31)), true),
            version("NVMesh-P2", Some(d(2026, 11, 30)), false),
            version("3.5.0", None, false),
        ];

        let names: Vec<&str> = phases(&Config::default(), &versions)
            .iter()
            .map(|v| v.name.as_str())
            .collect();

        assert_eq!(names, ["NVMesh-P2", "NVMesh-P3"]);
    }

    #[test]
    fn quoted_escapes_quotes() {
        assert_eq!(
            quoted(&["ES6.3.x (TBD)".to_string(), "a\"b".to_string()]),
            "\"ES6.3.x (TBD)\", \"a\\\"b\""
        );
    }

    #[test]
    fn added_counts_new_items_and_items_moved_into_these_versions() {
        assert_eq!(
            added_jql(
                "EX",
                &["ES6.3.10".to_string(), "ES6.3.9_C1".to_string()],
                28
            ),
            "project = EX AND fixVersion in (\"ES6.3.10\", \"ES6.3.9_C1\") AND (created >= -28d OR fixVersion CHANGED TO \"ES6.3.10\" AFTER -28d OR fixVersion CHANGED TO \"ES6.3.9_C1\" AFTER -28d)"
        );
    }

    #[test]
    fn each_release_view_counts_only_what_was_added_to_its_own_versions() {
        let cfg = Config::default();
        let tracked: Vec<String> = ["ES6.3.10", "ES7.0.0_Beta", "ES7.1.0"]
            .map(String::from)
            .to_vec();

        let added: Vec<(String, Vec<ViewId>, String)> = queries(&cfg, &tracked, &[])
            .into_iter()
            .filter(|q| matches!(q.id, QueryId::Added(_)))
            .map(|q| (q.name, q.views.into_iter().collect(), q.jql))
            .collect();

        assert_eq!(
            added,
            [
                (
                    "EXA6 added".to_string(),
                    vec![ViewId::Exa6],
                    added_jql("EX", &tracked[..1], 28)
                ),
                (
                    "EXA7 Beta added".to_string(),
                    vec![ViewId::Exa7Beta],
                    added_jql("EX", &tracked[1..2], 28)
                ),
                (
                    "EXA7 GA added".to_string(),
                    vec![ViewId::Exa7Ga],
                    added_jql("EX", &tracked[2..], 28)
                ),
            ]
        );
    }

    #[test]
    fn queries_cover_every_source_and_release_and_closures_are_required() {
        let cfg = Config::default();

        let list = queries(&cfg, &["ES6.3.10".to_string()], &["NVMesh-P2".to_string()]);

        let names: Vec<&str> = list.iter().map(|q| q.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Whamcloud",
                "DDN",
                "SFAFE",
                "NP",
                "EX open",
                "EX closed",
                "EXA6 added",
                "EX unscheduled",
                "EXA SD open",
                "EXA SD closed",
                "EXA SD added"
            ]
        );
        let required: Vec<&str> = list
            .iter()
            .filter(|q| q.id.required())
            .map(|q| q.name.as_str())
            .collect();
        assert_eq!(
            required,
            ["EX open", "EX closed", "EXA SD open", "EXA SD closed"]
        );
        let without: Vec<String> = queries(&cfg, &[], &[])
            .into_iter()
            .map(|q| q.name)
            .collect();
        assert_eq!(
            without,
            [
                "Whamcloud",
                "DDN",
                "SFAFE",
                "NP",
                "EX unscheduled",
                "EXA SD open"
            ],
            "nothing tracked, no release queries"
        );
        let views = |name: &str| {
            list.iter()
                .find(|q| q.name == name)
                .map(|q| q.views.iter().copied().collect::<Vec<_>>())
        };
        assert_eq!(views("Whamcloud"), Some(vec![ViewId::Gcp, ViewId::Nvidia]));
        assert_eq!(views("EX unscheduled"), Some(vec![ViewId::Exa6]));
        assert_eq!(views("EXA SD added"), Some(vec![ViewId::ExaSd]));
    }

    #[test]
    fn named_lists_a_few_keys() {
        let keys: Vec<IssueKey> = (1..=KEYS_NAMED + 2)
            .map(|i| format!("EX-{i}").parse().unwrap())
            .collect();
        let refs: Vec<&IssueKey> = keys.iter().collect();

        assert_eq!(named(&refs), "EX-1, EX-2, EX-3, EX-4, EX-5 and 2 more");
        assert_eq!(named(refs.get(..2).unwrap()), "EX-1, EX-2");
    }

    #[test]
    fn small_initiatives_are_judged_per_item() {
        let cfg = Config::default();
        let small = InitiativeData {
            config: cfg.initiatives.first().unwrap().clone(),
            epics: vec![],
            open: vec![ticket("EXM-500", "a")],
            closed_in_window: 0,
        };
        let large = InitiativeData {
            open: (1..=cfg.small_release_max + 1)
                .map(|i| ticket(&format!("EXM-{}", 600 + i), "b"))
                .collect(),
            ..small.clone()
        };

        let keys = small_release_keys(&cfg, &[], &[small, large]);

        assert_eq!(
            keys.into_iter().map(|k| k.to_string()).collect::<Vec<_>>(),
            ["EXM-500"]
        );
    }

    fn query(id: QueryId, name: &str, views: &[ViewId]) -> Query {
        Query {
            id,
            name: name.into(),
            instance: Instance::Cloud,
            jql: String::new(),
            raw_fields: None,
            views: views.iter().copied().collect(),
        }
    }

    fn page(target: Option<&str>) -> PmPage {
        PmPage {
            info: PmRef {
                title: "EXAScaler 7".into(),
                url: Url::parse("https://x/").unwrap(),
                status: None,
                target: target.map(String::from),
                updated: None,
            },
            dates: vec![],
        }
    }

    #[test]
    fn a_failed_required_query_fails_the_refresh() {
        let open = query(QueryId::ExOpen, "EX open", &ViewId::RELEASES);

        let err = collect(
            vec![Done::Query(&open, Err(anyhow!("timed out")))],
            &BTreeMap::new(),
            &mut SourceErrors::default(),
            &mut Vec::new(),
        )
        .err()
        .unwrap();

        assert_eq!(format!("{err:#}"), "Jira query 'EX open': timed out");
    }

    fn page_use(views: &[ViewId], dated: bool) -> PageUse {
        PageUse {
            views: views.iter().copied().collect(),
            dated,
        }
    }

    #[test]
    fn pm_pages_are_read_for_the_views_that_use_them() {
        let cfg = Config::default();
        let uses = page_uses(&cfg);

        assert_eq!(
            uses.get(&cfg.pm_pages.exa7),
            Some(&page_use(&[ViewId::Exa7Beta, ViewId::Exa7Ga], true))
        );
        assert_eq!(
            uses.get(&cfg.pm_pages.gcp),
            Some(&page_use(&[ViewId::Gcp], false))
        );
        let ndu = cfg
            .initiatives
            .iter()
            .find(|i| i.name == "NDU")
            .and_then(|i| i.pm_page)
            .unwrap();
        assert_eq!(uses.get(&ndu), Some(&page_use(&[ViewId::Future], true)));
    }

    #[test]
    fn other_failures_become_notes_in_the_views_they_feed() {
        let ddn = query(QueryId::Customers(1), "DDN", &ViewId::CUSTOMERS);
        let whamcloud = query(
            QueryId::Customers(0),
            "Whamcloud",
            &[ViewId::Gcp, ViewId::Nvidia],
        );
        let added = Query {
            raw_fields: Some("status"),
            ..query(QueryId::Added(ViewId::Exa6), "EXA6 added", &[ViewId::Exa6])
        };
        let cfg = Config::default();
        let new_ha = cfg.initiatives.iter().find(|i| i.in_exa_sd).unwrap();
        let pages = BTreeMap::from([
            (1, page_use(&[ViewId::Exa7Beta, ViewId::Exa7Ga], true)),
            (3, page_use(&[ViewId::Exa6], true)),
            (4, page_use(&[ViewId::Gcp], false)),
        ]);
        let mut errors = SourceErrors::default();
        let mut notes = Vec::new();

        let found = collect(
            vec![
                Done::Query(&ddn, Err(anyhow!("HTTP 503"))),
                Done::Query(
                    &whamcloud,
                    Ok(Rows::Tickets(Tickets {
                        items: vec![],
                        skipped: 2,
                    })),
                ),
                Done::Query(&added, Err(anyhow!("timed out"))),
                Done::Page(1, Ok(page(None))),
                Done::Page(2, Ok(page(None))),
                Done::Page(3, Err(anyhow!("HTTP 404"))),
                Done::Page(4, Err(anyhow!("HTTP 404"))),
                Done::Initiative(new_ha, Err(anyhow!("HTTP 400"))),
            ],
            &pages,
            &mut errors,
            &mut notes,
        )
        .unwrap();

        assert_eq!(found.pm_pages.len(), 2);
        assert!(!found.lists_complete);
        let summary: Vec<(Vec<ViewId>, &str, bool)> = notes
            .iter()
            .map(|n| {
                (
                    n.views.iter().copied().collect(),
                    n.text.as_str(),
                    n.incomplete,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (ViewId::CUSTOMERS.to_vec(), "DDN: not read (HTTP 503)", true),
                (
                    vec![ViewId::Gcp, ViewId::Nvidia],
                    "Whamcloud: 2 issues could not be read and are left out",
                    true
                ),
                (
                    vec![ViewId::Exa6],
                    "EXA6 added: not read (timed out)",
                    false
                ),
                (
                    vec![ViewId::Exa7Beta, ViewId::Exa7Ga],
                    "PM page \"EXAScaler 7\" has no Target Date row; its dates fall back to Jira.",
                    false
                ),
                (
                    vec![ViewId::Exa6],
                    "PM page 3 could not be read, so the Jira date is used.",
                    true
                ),
                (vec![ViewId::Gcp], "PM page 4 could not be read.", false),
                (
                    vec![ViewId::ExaSd, ViewId::Future],
                    "New HA (EAM): not read (HTTP 400)",
                    true
                ),
            ]
        );
        assert_eq!(
            errors,
            SourceErrors {
                cloud: vec![
                    "DDN: HTTP 503".into(),
                    "EXA6 added: timed out".into(),
                    "New HA (EAM): HTTP 400".into()
                ],
                whamcloud: vec![],
                confluence: vec!["page 3: HTTP 404".into(), "page 4: HTTP 404".into()],
            }
        );
    }

    #[test_case(true, &["cloud:EX-1"] ; "complete lists drop entries no ticket uses")]
    #[test_case(false, &["cloud:EX-1", "cloud:EX-2"] ; "an incomplete list keeps every entry")]
    fn comment_cache_pruning(complete: bool, expected: &[&str]) {
        let cache = HashMap::from([
            ("cloud:EX-1".to_string(), cached("a", None)),
            ("cloud:EX-2".to_string(), cached("b", None)),
        ]);
        let in_use = HashMap::from([("cloud:EX-1".to_string(), cached("a", None))]);

        let kept = kept_comments(cache, in_use, complete);

        let mut keys: Vec<&str> = kept.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, expected);
    }

    #[test]
    fn comment_failures_name_the_tickets_per_jira_and_note_customer_ones() {
        let key = |k: &str| k.parse::<IssueKey>().unwrap();
        let failed = vec![
            (Instance::Whamcloud, key("GCP-315")),
            (Instance::Cloud, key("EX-1")),
        ];
        let mut errors = SourceErrors::default();
        let mut notes = Vec::new();

        report_comment_failures(
            &failed,
            &BTreeSet::from([key("GCP-315")]),
            &mut errors,
            &mut notes,
        );

        assert_eq!(
            (errors.cloud, errors.whamcloud),
            (
                vec!["newest comment not read for EX-1".to_string()],
                vec!["newest comment not read for GCP-315".to_string()]
            )
        );
        assert_eq!(
            notes,
            [Note::info(ViewId::CUSTOMERS, "Newest comment not read for GCP-315; their idle time is unknown or from an earlier refresh.".into())]
        );
    }

    fn status_error(code: u16) -> anyhow::Error {
        http::StatusError {
            path: "/rest/api/3/search/jql".into(),
            code,
            excerpt: String::new(),
        }
        .into()
    }

    #[test]
    fn an_unknown_key_reads_the_chunk_key_by_key() {
        let key = |k: &str| k.parse::<IssueKey>().unwrap();
        let keys = [key("EX-1"), key("EX-404"), key("EX-5")];
        let chunk: Vec<&IssueKey> = keys.iter().collect();

        let fixes = read_fixes(&chunk, |list| match list {
            "EX-1" => Ok(Tickets {
                items: vec![ticket("EX-1", "fix")],
                skipped: 0,
            }),
            "EX-5" => Err(status_error(503)),
            _ => Err(status_error(400)),
        });

        assert_eq!(
            fixes.found.keys().map(IssueKey::as_str).collect::<Vec<_>>(),
            ["EX-1"]
        );
        assert_eq!(fixes.unread, BTreeSet::from([key("EX-404"), key("EX-5")]));
        assert_eq!(
            fixes.errors.len(),
            1,
            "only the failure that is not an unknown key is an error"
        );
    }

    #[test]
    fn any_other_failure_leaves_the_chunk_unread() {
        let key: IssueKey = "EX-1".parse().unwrap();
        let calls = std::cell::Cell::new(0);

        let fixes = read_fixes(&[&key], |_| {
            calls.set(calls.get() + 1);
            Err(status_error(401))
        });

        assert_eq!(
            (fixes.unread.len(), fixes.errors.len(), calls.get()),
            (1, 1, 1)
        );
    }

    fn cached(updated: &str, date: Option<NaiveDate>) -> CachedComment {
        CachedComment {
            updated: updated.into(),
            comment: date.map(|date| Comment {
                date,
                author: Some("Jason".into()),
            }),
        }
    }

    #[test]
    fn a_ticket_listed_twice_gets_its_comment_both_times_and_is_read_once() {
        let mut release = ticket("EXM-458", "x");
        let mut initiative = release.clone();
        let mut fresh = ticket("EX-2", "y");
        fresh.updated = "changed".into();
        let cache = HashMap::from([
            (
                cache_key(&release),
                cached(&release.updated, Some(d(2026, 10, 19))),
            ),
            ("cloud:EX-2".to_string(), cached("before", None)),
            ("cloud:EX-404".to_string(), cached("gone", None)),
        ]);
        let mut tickets = vec![&mut release, &mut initiative, &mut fresh];

        let stale: Vec<String> = stale_comments(&cache, &tickets)
            .into_iter()
            .map(|s| s.cache_key)
            .collect();
        let kept = apply_comments(&cache, &mut tickets, today());

        assert_eq!(stale, ["cloud:EX-2"]);
        assert_eq!(
            (release.idle_days, initiative.idle_days),
            (Some(2), Some(2))
        );
        assert_eq!(
            fresh.idle_days,
            Some(20),
            "an earlier answer is used until a new one is read"
        );
        assert!(
            !kept.contains_key("cloud:EX-404"),
            "entries no ticket uses are dropped"
        );
    }

    #[test]
    fn a_ticket_without_a_cached_comment_has_unknown_idle_time() {
        let mut t = ticket("EX-1", "x");

        apply_comments(&HashMap::new(), &mut [&mut t], today());

        assert_eq!(t.idle_days, None);
    }

    #[test]
    fn stale_comments_are_listed_once_per_ticket() {
        let mut a = ticket("EX-1", "x");
        let mut b = a.clone();

        let stale = stale_comments(&HashMap::new(), &[&mut a, &mut b]);

        assert_eq!(stale.len(), 1);
    }

    #[test_case(true, &["rev-a"] ; "a complete search drops tags no change uses")]
    #[test_case(false, &["rev-a", "rev-b"] ; "a failed search keeps every tag")]
    fn tag_pruning(complete: bool, expected: &[&str]) {
        let tags = HashMap::from([
            ("rev-a".to_string(), "2.14.0-ddn258".to_string()),
            ("rev-b".to_string(), "2.14.0-ddn259".to_string()),
        ]);

        let kept = pruned_tags(tags, &HashSet::from(["rev-a"]), complete);

        let mut revisions: Vec<&str> = kept.keys().map(String::as_str).collect();
        revisions.sort_unstable();
        assert_eq!(revisions, expected);
    }

    fn patch(number: u64, status: ChangeStatus) -> PatchRef {
        PatchRef {
            number,
            url: Url::parse(&format!(
                "https://review.whamcloud.com/c/ex/lustre-release/+/{number}"
            ))
            .unwrap(),
            branch: "b_es6_0".into(),
            status,
            first_tag: None,
        }
    }

    #[test]
    fn changes_reach_every_ticket_whose_keys_they_name() {
        let key = |k: &str| k.parse::<IssueKey>().unwrap();
        let watched = vec![
            Watched {
                key: key("DDN-7162"),
                search: vec![key("DDN-7162"), key("EX-100")],
            },
            Watched {
                key: key("GCP-315"),
                search: vec![key("GCP-315")],
            },
            Watched {
                key: key("EX-100"),
                search: vec![key("EX-100")],
            },
        ];
        let keys = |list: &[&str]| list.iter().map(|k| key(k)).collect::<BTreeSet<IssueKey>>();
        let changes = vec![
            (keys(&["EX-100", "LU-1"]), patch(1, ChangeStatus::Merged)),
            (keys(&["EX-9"]), patch(2, ChangeStatus::New)),
        ];

        let map = patches_by_ticket(&watched, &changes);

        let numbers = |k: &str| {
            map.get(&key(k))
                .map(|ps| ps.iter().map(|p| p.number).collect::<Vec<_>>())
        };
        assert_eq!(
            (numbers("DDN-7162"), numbers("GCP-315"), numbers("EX-100")),
            (Some(vec![1]), Some(vec![]), Some(vec![1]))
        );
    }
}
