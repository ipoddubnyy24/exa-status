//! Turns what one refresh read (`RawData`) into the snapshot the UI shows.
//! No I/O happens here, so every view can be tested from fixture data.

use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate};
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::config::Config;
use crate::confluence::PmPage;
use crate::fetch::{self, InitiativeData, KeysByVersion, Patches, RawData, Watched};
use crate::jira::Version;
use crate::model::{
    BuildHealth, Category, Committed, EpicRef, FixRef, FixState, InitiativeCard, IssueKey, Kpi,
    SNAPSHOT_FORMAT, Severity, Snapshot, SourceCount, SourceStatus, Tab, Ticket, Tone, Track,
    Verdict, View, ViewId, ViewKind,
};
use crate::rules::{self, BLOCKER, Bucket, CRITICAL, Classifier, TrackInput};

/// When the snapshot was made and how long the refresh took.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
    /// When the refresh finished.
    pub(crate) generated_at: DateTime<Local>,
    /// How long it took.
    pub(crate) duration_secs: f64,
}

/// What happened with Gerrit, which is read after the first snapshot is out.
#[derive(Clone, Copy, Debug)]
pub(crate) enum GerritState<'a> {
    /// Not read yet in this refresh.
    NotRead,
    /// Read; some searches may have failed.
    Read(&'a Patches),
    /// Could not be read at all.
    Failed(&'a str),
}

/// Builds the snapshot.
pub(crate) fn snapshot(
    cfg: &Config,
    raw: &RawData,
    gerrit: GerritState<'_>,
    timing: Timing,
) -> Result<Snapshot> {
    let empty = Patches::default();
    let patches = match gerrit {
        GerritState::Read(p) => p,
        GerritState::NotRead | GerritState::Failed(_) => &empty,
    };
    let ctx = Context {
        cfg,
        raw,
        patches,
        today: raw.today,
        versions: all_versions(cfg, raw),
    };
    let classifier = Classifier::new(cfg)?;
    let (mut others, mut gcp, mut nvidia) = (Vec::new(), Vec::new(), Vec::new());
    let rated = raw
        .customers
        .iter()
        .cloned()
        .map(|t| rules::with_priority_severity(cfg, t))
        .collect();
    for t in rules::dedup(rated, &cfg.mirror_projects) {
        match ctx.customer(&classifier, t) {
            (Bucket::Other, t) => others.push(t),
            (Bucket::Gcp, t) => gcp.push(t),
            (Bucket::Nvidia, t) => nvidia.push(t),
            (Bucket::Test, _) => {}
        }
    }
    let gcp_pm = raw.pm_pages.get(&cfg.pm_pages.gcp).map(|p| p.info.clone());
    // The groups' rows as they are, promises included; dates are set in the group views.
    let all = [&others, &gcp, &nvidia]
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    let customer_tab = tab(
        "customers",
        "Customers",
        vec![
            ctx.customer_view(ViewId::All, all, false),
            ctx.customer_view(ViewId::Others, others, false),
            View {
                pm: gcp_pm,
                ..ctx.customer_view(ViewId::Gcp, gcp, true)
            },
            ctx.customer_view(ViewId::Nvidia, nvidia, true),
        ],
    );
    let development_tab = tab("development", "Development", ctx.release_views());
    let future_tab = tab("future", "Future work", vec![ctx.future_view()]);

    let mut sources = raw.sources.clone();
    let gerrit_error = match gerrit {
        GerritState::NotRead => Some("not read yet".to_string()),
        GerritState::Read(p) => (!p.errors.is_empty()).then(|| p.errors.join("; ")),
        GerritState::Failed(e) => Some(e.to_string()),
    };
    sources.push(SourceStatus {
        name: "Gerrit".into(),
        error: gerrit_error,
    });
    Ok(Snapshot {
        format: SNAPSHOT_FORMAT,
        generated_at: timing.generated_at,
        today: raw.today,
        duration_secs: (timing.duration_secs * 10.0).round() / 10.0,
        sources,
        tabs: vec![customer_tab, development_tab, future_tab],
    })
}

/// The refresh read everything its verdicts depend on.
pub(crate) fn complete(raw: &RawData) -> bool {
    !raw.notes.iter().any(|n| n.incomplete)
}

/// The tickets whose Gerrit changes are shown: every NVIDIA and GCP ticket,
/// S1/S2 tickets of other customers, and every release and initiative blocker.
pub(crate) fn watch_list(cfg: &Config, snap: &Snapshot) -> Vec<Watched> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for view in snap.tabs.iter().flat_map(|t| &t.views) {
        let blockers: BTreeSet<&IssueKey> = view
            .blockers
            .iter()
            .chain(view.initiatives.iter().flat_map(|c| &c.blockers))
            .map(|b| &b.key)
            .collect();
        let watched_item = |t: &Ticket| match view.id {
            // Each of its tickets is watched, or not, by its own group's view.
            ViewId::All => false,
            ViewId::Gcp | ViewId::Nvidia => true,
            ViewId::Others => matches!(t.severity_level(), Some(1 | 2)),
            _ => blockers.contains(&t.key),
        };
        for t in view.items.iter().filter(|t| watched_item(t)) {
            if !seen.insert(t.key.clone()) {
                continue;
            }
            let mut search = vec![t.key.clone()];
            search.extend(t.also.iter().cloned());
            search.extend(
                t.links
                    .iter()
                    .map(|l| &l.key)
                    .filter(|k| cfg.gerrit_key_projects.iter().any(|p| p == k.project()))
                    .cloned(),
            );
            search.sort();
            search.dedup();
            out.push(Watched {
                key: t.key.clone(),
                search,
            });
        }
    }
    out
}

/// Every known fixVersion, by project and name: a name can exist in two projects.
fn all_versions<'a>(cfg: &'a Config, raw: &'a RawData) -> HashMap<(&'a str, &'a str), &'a Version> {
    let by_project = [
        (cfg.ex_project.as_str(), &raw.ex_versions),
        (cfg.exa_sd_project.as_str(), &raw.sd_versions),
    ]
    .into_iter();
    by_project
        .flat_map(|(project, versions)| {
            versions
                .iter()
                .map(move |v| ((project, v.name.as_str()), v))
        })
        .collect()
}

fn tab(id: &str, label: &str, views: Vec<View>) -> Tab {
    let verdict = views
        .iter()
        .map(|v| v.verdict)
        .max()
        .unwrap_or(Verdict::NoDate);
    Tab {
        id: id.into(),
        label: label.into(),
        verdict,
        views,
    }
}

fn kpi(label: &str, value: String, note: String, tone: Option<Tone>) -> Kpi {
    Kpi {
        label: label.into(),
        value,
        note,
        tone,
        by_source: Vec::new(),
    }
}

/// "1 ticket", "3 tickets".
fn counted(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// How far a ticket's linked fixes are, from the `read` ones of `total`.
fn fix_state(total: usize, read: &[&Ticket]) -> FixState {
    let done = read.iter().filter(|f| f.category == Category::Done).count();
    if read.is_empty() {
        FixState::NoFix
    } else if done == total {
        FixState::Delivered
    } else if read.iter().any(|f| f.category != Category::New) {
        FixState::UnderWay
    } else {
        FixState::NotStarted
    }
}

/// Unique by key, in input order.
fn unique<'a>(tickets: impl IntoIterator<Item = &'a Ticket>) -> Vec<&'a Ticket> {
    let mut seen = BTreeSet::new();
    tickets
        .into_iter()
        .filter(|t| seen.insert(&t.key))
        .collect()
}

/// One release or phase to judge.
struct TrackSpec<'s> {
    /// Display name.
    name: &'s str,
    /// The project whose fixVersions these are.
    project: &'s str,
    /// The fixVersions it covers.
    versions: &'s [String],
    /// Open items of its project, filtered to `versions`.
    open: &'s [Ticket],
    /// Closures of its project in the window.
    closed: &'s KeysByVersion,
    /// The id of its PM status page, if it has one.
    page: Option<u64>,
    /// The label of its date on that page ("Beta", "GA"); `None` takes the first date.
    label: Option<&'s str>,
}

/// A judged release or phase.
struct Judged<'s> {
    track: Track,
    items: Vec<&'s Ticket>,
    /// Why the PM page date could not be used, when there is a page.
    pm_note: Option<String>,
}

/// Closures and additions during the closing window.
#[derive(Clone, Copy)]
struct Window {
    closed: usize,
    /// `None` when the "added" query could not be read.
    added: Option<usize>,
}

/// Shared inputs of the view builders.
struct Context<'a> {
    cfg: &'a Config,
    raw: &'a RawData,
    patches: &'a Patches,
    today: NaiveDate,
    versions: HashMap<(&'a str, &'a str), &'a Version>,
}

impl<'a> Context<'a> {
    /// An item with its Gerrit changes, and what says its fix awaits verification.
    fn item(&self, mut t: Ticket) -> Ticket {
        t.patches = self.patches.map.get(&t.key).cloned().unwrap_or_default();
        t.verification = rules::verification(self.cfg, &t);
        t
    }

    /// The fixVersion `name` of `project`.
    fn version(&self, project: &str, name: &str) -> Option<&'a Version> {
        self.versions.get(&(project, name)).copied()
    }

    /// A customer ticket with its bucket, fix, promise and patches. Link
    /// states come from the cloud copy of each fix, since links on Whamcloud
    /// tickets point at stale copies.
    fn customer(&self, classifier: &Classifier, t: Ticket) -> (Bucket, Ticket) {
        let mut t = self.item(t);
        let bucket = classifier.bucket(&t);
        for l in &mut t.links {
            if let Some(f) = self.raw.fixes.get(&l.key) {
                l.done = f.category == Category::Done;
            }
        }
        let fix_keys: Vec<&IssueKey> = rules::fix_keys(self.cfg, &t).collect();
        let linked: Vec<&Ticket> = fix_keys
            .iter()
            .filter_map(|k| self.raw.fixes.get(*k))
            .collect();
        t.fix_state = fix_state(fix_keys.len(), &linked);
        // Dates come from EX fixVersions, which are releases; EXR fixVersions
        // are planning quarters ("2025Q4"), so an EXR fix counts toward
        // "fix delivered" but gives no date.
        t.fix = linked
            .iter()
            .filter(|f| f.category != Category::Done && f.key.project() == self.cfg.ex_project)
            .flat_map(|f| {
                f.fix_versions.iter().filter_map(move |name| {
                    let v = self.version(f.key.project(), name)?;
                    Some((v.release.filter(|_| !v.released)?, *f, name))
                })
            })
            .min_by_key(|(date, _, _)| *date)
            .map(|(date, f, name)| FixRef {
                key: f.key.clone(),
                url: f.url.clone(),
                version: name.clone(),
                date,
                status: f.status.clone(),
                category: f.category,
            });
        if matches!(bucket, Bucket::Nvidia | Bucket::Gcp) {
            t.committed = std::iter::once(&t.key).chain(&t.also).find_map(|k| {
                self.raw.commitments.get(k).map(|c| Committed {
                    date: c.date,
                    note: c.note.clone(),
                    key: k.clone(),
                })
            });
        }
        (bucket, t)
    }

    /// The refresh notes about `view`'s data.
    fn notes(&self, view: ViewId) -> Vec<String> {
        self.raw
            .notes
            .iter()
            .filter(|n| n.views.contains(&view))
            .map(|n| n.text.clone())
            .collect()
    }

    /// A view whose verdict rests on data that was not all read: it reads no
    /// better than "at risk" and its summary says so.
    fn with_gaps(&self, mut view: View) -> View {
        if self
            .raw
            .notes
            .iter()
            .any(|n| n.incomplete && n.views.contains(&view.id))
        {
            view.verdict = view.verdict.max(Verdict::AtRisk);
            view.summary = format!("Incomplete data, see the notes. {}", view.summary);
        }
        view
    }

    /// Closures and additions of a view's `versions` in the window.
    fn window(&self, view: ViewId, closed: &KeysByVersion, versions: &[String]) -> Window {
        Window {
            closed: count_in(closed, versions),
            added: self.raw.added.get(&view).copied().flatten(),
        }
    }

    fn customer_view(&self, id: ViewId, mut items: Vec<Ticket>, editable: bool) -> View {
        let cfg = self.cfg;
        let refs: Vec<&Ticket> = items.iter().collect();
        let mut by_source: BTreeMap<&str, usize> = BTreeMap::new();
        for t in &refs {
            *by_source
                .entry(t.source.as_deref().unwrap_or("other"))
                .or_default() += 1;
        }
        let by_source: Vec<SourceCount> = by_source
            .into_iter()
            .map(|(source, count)| SourceCount {
                source: source.into(),
                count,
            })
            .collect();
        let source_note = by_source
            .iter()
            .map(|SourceCount { source, count }| format!("{source} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let s1 = refs
            .iter()
            .filter(|t| t.severity_level() == Some(1))
            .count();
        let s2 = refs
            .iter()
            .filter(|t| t.severity_level() == Some(2))
            .count();
        let unrated = refs.iter().filter(|t| t.severity.is_none()).count();
        let by_priority = refs
            .iter()
            .filter(|t| t.severity.as_ref().is_some_and(Severity::is_from_priority))
            .count();
        let idle = rules::severity_idle(cfg, &refs);
        let idle_count = refs
            .iter()
            .filter(|t| t.idle_days.is_some_and(|i| i > cfg.idle_alert_days))
            .count();
        let idle_month = refs
            .iter()
            .filter(|t| t.idle_days.is_some_and(|i| i > cfg.idle_offtrack_days))
            .count();
        let idle_unknown = refs.iter().filter(|t| t.idle_days.is_none()).count();
        let delivered = refs
            .iter()
            .filter(|t| t.fix_state == FixState::Delivered)
            .count();
        let committed = refs.iter().filter(|t| t.committed.is_some()).count();
        let dated = refs.iter().filter(|t| rules::target(t).is_some()).count();
        let groups = rules::date_groups(cfg, self.today, &refs);
        let date_verdict = groups
            .iter()
            .map(|g| g.verdict)
            .max()
            .unwrap_or(Verdict::NoDate);
        let date_summary = match groups
            .iter()
            .filter(|g| g.verdict == date_verdict)
            .min_by_key(|g| g.date)
        {
            Some(g) if date_verdict != Verdict::OnTrack => format!(
                "{} due {} ({} days): {}",
                counted(g.keys.len(), "ticket"),
                rules::fmt_date(g.date),
                g.days_left,
                g.reason.as_deref().unwrap_or("on track")
            ),
            Some(_) => "Every dated ticket is on track".into(),
            None => "No ticket here has a date yet".into(),
        };
        let summary = if idle.verdict >= Verdict::AtRisk {
            let mut parts = Vec::new();
            if idle.past_limit > 0 {
                parts.push(format!(
                    "{} past the idle limit, {} of them idle over {} days",
                    counted(idle.past_limit, "S1/S2 ticket"),
                    idle.off_track,
                    cfg.idle_offtrack_days
                ));
            }
            if idle.unknown > 0 {
                parts.push(format!(
                    "{} with unknown idle time",
                    counted(idle.unknown, "S1/S2 ticket")
                ));
            }
            let idle_summary = parts.join("; ");
            if date_verdict >= Verdict::AtRisk {
                format!("{idle_summary}. {date_summary}")
            } else {
                idle_summary
            }
        } else {
            date_summary
        };
        let mut notes = self.notes(id);
        if unrated > 0 {
            notes.push(format!(
                "No severity, so the S1/S2 idle limits do not apply: {}.",
                counted(unrated, "ticket")
            ));
        }
        if by_priority > 0 {
            let levels: Vec<String> = cfg
                .priority_severity
                .levels
                .iter()
                .map(|(p, l)| format!("{p} as S{l}"))
                .collect();
            notes.push(format!(
                "Rated from the priority ({}): {}.",
                levels.join(", "),
                counted(by_priority, "ticket")
            ));
        }
        let blockers = rules::customer_blockers(cfg, self.today, &refs);
        let open = items.len();
        // Most severe first, then most idle; an unknown idle time sorts as
        // the most idle, as the rules treat it.
        items.sort_by(|a, b| {
            a.severity_level()
                .unwrap_or(u8::MAX)
                .cmp(&b.severity_level().unwrap_or(u8::MAX))
                .then(
                    b.idle_days
                        .unwrap_or(i64::MAX)
                        .cmp(&a.idle_days.unwrap_or(i64::MAX)),
                )
        });
        let idle_note = match idle_unknown {
            0 => format!("{idle_month} over {} days", cfg.idle_offtrack_days),
            n => format!(
                "{idle_month} over {} days, {n} unknown",
                cfg.idle_offtrack_days
            ),
        };
        let view = View {
            id,
            label: id.label().into(),
            kind: ViewKind::Customers,
            verdict: date_verdict.max(idle.verdict),
            summary,
            kpis: vec![
                Kpi {
                    by_source,
                    ..kpi("Open", open.to_string(), source_note, None)
                },
                kpi(
                    "S1 / S2",
                    format!("{s1} / {s2}"),
                    format!("{} past the idle limit", idle.past_limit),
                    (idle.past_limit > 0).then_some(Tone::Danger),
                ),
                kpi(
                    &format!("Idle over {} days", cfg.idle_alert_days),
                    idle_count.to_string(),
                    idle_note,
                    None,
                ),
                kpi(
                    "Fix delivered",
                    delivered.to_string(),
                    "every linked fix done, ticket open".into(),
                    None,
                ),
                kpi(
                    "Dated",
                    dated.to_string(),
                    format!("{committed} committed, {} have no date", open - dated),
                    None,
                ),
            ],
            tracks: Vec::new(),
            undated: open - dated,
            date_groups: groups,
            blockers,
            items,
            tests: Vec::new(),
            notes,
            pm: None,
            health: None,
            initiatives: Vec::new(),
            editable_commitments: editable,
        };
        self.with_gaps(view)
    }

    /// The PM page with id `page`, and whether it was configured but not read.
    fn pm_page(&self, page: Option<u64>) -> (Option<&'a PmPage>, bool) {
        let read = page.and_then(|id| self.raw.pm_pages.get(&id));
        (read, page.is_some() && read.is_none())
    }

    /// One release or phase, judged from its open items. A release none of
    /// whose fixVersions Jira has cannot be judged.
    fn release_track<'s>(&self, spec: &TrackSpec<'s>) -> Judged<'s>
    where
        'a: 's,
    {
        let items: Vec<&Ticket> = spec
            .open
            .iter()
            .filter(|t| t.fix_versions.iter().any(|f| spec.versions.contains(f)))
            .collect();
        let dated = spec
            .versions
            .iter()
            .filter_map(|n| self.version(spec.project, n));
        let jira_date = dated.clone().filter_map(|v| v.release).max();
        let start_date = dated.filter_map(|v| v.start).min();
        let missing: Vec<&str> = spec
            .versions
            .iter()
            .filter(|n| self.version(spec.project, n).is_none())
            .map(String::as_str)
            .collect();
        let (page, pm_unread) = self.pm_page(spec.page);
        let pm_date = page.and_then(|p| p.target(spec.label));
        let pm_note = page
            .filter(|_| pm_date.is_none())
            .map(|p| pm_gap(p, spec.label));
        let mut track = rules::evaluate_track(
            self.cfg,
            self.today,
            TrackInput {
                name: spec.name.into(),
                items: items.clone(),
                closed_in_window: count_in(spec.closed, spec.versions),
                pm_date,
                pm: page.map(|p| p.info.clone()),
                pm_unread,
                jira_date,
                start_date,
            },
        );
        if !missing.is_empty() {
            let text = format!("Jira has no fixVersion {}", missing.join(", "));
            if missing.len() == spec.versions.len() {
                track.verdict = Verdict::NoDate;
                track.reason = text;
            } else {
                track.reason = format!("{}. {text}", track.reason);
            }
        }
        Judged {
            track,
            items,
            pm_note,
        }
    }

    /// An EX release to judge.
    fn ex_spec<'s>(
        &self,
        name: &'s str,
        versions: &'s [String],
        page: Option<u64>,
        label: Option<&'s str>,
    ) -> TrackSpec<'s>
    where
        'a: 's,
    {
        TrackSpec {
            name,
            project: &self.cfg.ex_project,
            versions,
            open: &self.raw.ex_open,
            closed: &self.raw.ex_closed,
            page,
            label,
        }
    }

    fn release_views(&self) -> Vec<View> {
        let (cfg, raw) = (self.cfg, self.raw);
        let pages = &cfg.pm_pages;

        let tracked = fetch::tracked_versions(cfg, &raw.ex_versions);
        let exa6: Vec<String> = fetch::release_versions(cfg, &tracked)
            .into_iter()
            .find(|(view, _)| *view == ViewId::Exa6)
            .map(|(_, names)| names)
            .unwrap_or_default();
        let mut exa6_tracks = Vec::new();
        let mut exa6_items = Vec::new();
        let mut exa6_notes = self.notes(ViewId::Exa6);
        for name in &exa6 {
            let page = (*name == pages.exa6_release).then_some(pages.exa6);
            let judged =
                self.release_track(&self.ex_spec(name, std::slice::from_ref(name), page, None));
            exa6_tracks.push(judged.track);
            exa6_items.extend(judged.items);
            exa6_notes.extend(judged.pm_note);
        }
        if !exa6.contains(&pages.exa6_release) {
            exa6_notes.push(format!(
                "No open release is called {}, so its PM page date is not used.",
                pages.exa6_release
            ));
        }
        exa6_notes.push(match raw.unscheduled {
            Some(n) => format!(
                "{n} open items sit in {} with no release date.",
                cfg.exa6_unscheduled
            ),
            None => format!(
                "Items in {} not counted: that query failed.",
                cfg.exa6_unscheduled
            ),
        });
        let mut exa6_view = self.release_view(
            ViewId::Exa6,
            exa6_tracks,
            &unique(exa6_items),
            self.window(ViewId::Exa6, &raw.ex_closed, &exa6),
        );
        exa6_view.health = self.health(true);
        exa6_view.notes = exa6_notes;

        let beta = std::slice::from_ref(&cfg.exa7_beta_version);
        let judged = self.release_track(&self.ex_spec(
            &cfg.exa7_beta_version,
            beta,
            Some(pages.exa7),
            Some(&pages.exa7_beta_label),
        ));
        let mut beta_view = self.release_view(
            ViewId::Exa7Beta,
            vec![judged.track],
            &judged.items,
            self.window(ViewId::Exa7Beta, &raw.ex_closed, beta),
        );
        beta_view.health = self.health(false);
        beta_view.notes = self.notes(ViewId::Exa7Beta);
        beta_view.notes.extend(judged.pm_note);

        let ga = &cfg.exa7_ga_versions;
        let ga_name = format!("EXA7 GA ({})", ga.join(" + "));
        let judged = self.release_track(&self.ex_spec(
            &ga_name,
            ga,
            Some(pages.exa7),
            Some(&pages.exa7_ga_label),
        ));
        let mut ga_view = self.release_view(
            ViewId::Exa7Ga,
            vec![judged.track],
            &judged.items,
            self.window(ViewId::Exa7Ga, &raw.ex_closed, ga),
        );
        ga_view.health = self.health(false);
        ga_view.notes = self.notes(ViewId::Exa7Ga);
        ga_view.notes.extend(judged.pm_note);

        let phases: Vec<String> = fetch::phases(cfg, &raw.sd_versions)
            .iter()
            .map(|v| v.name.clone())
            .collect();
        let mut sd_notes = self.notes(ViewId::ExaSd);
        let mut sd_tracks: Vec<Track> = Vec::new();
        for name in &phases {
            let page = (*name == cfg.exa_sd_pm_phase).then_some(pages.exa_sd);
            let spec = TrackSpec {
                name,
                project: &cfg.exa_sd_project,
                versions: std::slice::from_ref(name),
                open: &raw.sd_open,
                closed: &raw.sd_closed,
                page,
                label: None,
            };
            let judged = self.release_track(&spec);
            sd_tracks.push(judged.track);
            sd_notes.extend(judged.pm_note);
        }
        if !phases.contains(&cfg.exa_sd_pm_phase) {
            sd_notes.push(format!(
                "No open phase is called {}, so the EXA SD PM page date is not used.",
                cfg.exa_sd_pm_phase
            ));
        }
        sd_tracks.extend(
            raw.initiatives
                .iter()
                .filter(|i| i.config.in_exa_sd)
                .map(|i| self.initiative_card(i).0.track),
        );
        let tests = raw
            .sd_open
            .iter()
            .filter(|t| rules::is_test_case(t))
            .count();
        let mut sd_view = self.release_view(
            ViewId::ExaSd,
            sd_tracks,
            &raw.sd_open.iter().collect::<Vec<_>>(),
            self.window(ViewId::ExaSd, &raw.sd_closed, &phases),
        );
        // Open scope counts the open EXA SD items other than Test items and
        // says how many those are; closures and additions count only items
        // in the phases.
        if let Some(open) = sd_view.kpis.first_mut() {
            open.value = (raw.sd_open.len() - tests).to_string();
            open.note = if tests == 1 {
                "1 is a Test item".into()
            } else {
                format!("{tests} are Test items")
            };
        }
        if let Some(window) = sd_view.kpis.last_mut() {
            window.note = "closed / added in the phases (new or moved in)".into();
        }
        sd_view.notes = sd_notes;
        [exa6_view, beta_view, ga_view, sd_view]
            .into_iter()
            .map(|v| self.with_gaps(v))
            .collect()
    }

    /// The build-health tile for es6 (EXA6) or es7 (EXA7) views.
    fn health(&self, es6: bool) -> Option<BuildHealth> {
        self.raw.health.clone().map(|h| {
            if es6 {
                BuildHealth { es7: None, ..h }
            } else {
                BuildHealth { es6: None, ..h }
            }
        })
    }

    fn release_view(
        &self,
        id: ViewId,
        tracks: Vec<Track>,
        items: &[&Ticket],
        window: Window,
    ) -> View {
        let cfg = self.cfg;
        let today = self.today;
        let review = items.iter().filter(|t| cfg.in_review(&t.status)).count();
        let work: Vec<&&Ticket> = items.iter().filter(|t| !rules::is_test_case(t)).collect();
        let tests = items.len() - work.len();
        let blocker_priority = work
            .iter()
            .filter(|t| rules::has_priority(t, BLOCKER))
            .count();
        let blocked = work.iter().filter(|t| rules::is_blocked_status(t)).count();
        let critical = work
            .iter()
            .filter(|t| rules::has_priority(t, CRITICAL))
            .count();
        let tests_note = if tests > 0 {
            format!(", {tests} test cases not counted")
        } else {
            String::new()
        };
        let s1 = work
            .iter()
            .filter(|t| t.severity_level() == Some(1))
            .count();
        let s2 = work
            .iter()
            .filter(|t| t.severity_level() == Some(2))
            .count();
        let with_due = work.iter().filter(|t| t.due.is_some()).count();
        let past_due = work
            .iter()
            .filter(|t| t.due.is_some_and(|d| d < today))
            .count();
        let verdict = tracks
            .iter()
            .map(|t| t.verdict)
            .max()
            .unwrap_or(Verdict::NoDate);
        let summary = tracks.iter().find(|t| t.verdict == verdict).map_or_else(
            || "No dated release".to_string(),
            |t| format!("{}: {}", t.name, t.reason),
        );
        let blockers = rules::release_blockers(cfg, today, items);
        let added = window
            .added
            .map_or_else(|| "?".to_string(), |a| a.to_string());
        let mut sorted: Vec<Ticket> = items.iter().map(|t| self.item((*t).clone())).collect();
        sorted.sort_by(|a, b| {
            rules::priority_rank(a)
                .cmp(&rules::priority_rank(b))
                .then(a.key.cmp(&b.key))
        });
        let (test_items, work_items): (Vec<Ticket>, Vec<Ticket>) =
            sorted.into_iter().partition(rules::is_test_case);
        View {
            id,
            label: id.label().into(),
            kind: ViewKind::Release,
            verdict,
            summary,
            kpis: vec![
                kpi(
                    "Open scope",
                    items.len().to_string(),
                    format!("{review} in review or verification"),
                    None,
                ),
                kpi(
                    "Blocker priority",
                    blocker_priority.to_string(),
                    format!("+{blocked} blocked{tests_note}"),
                    (blocker_priority > 0).then_some(Tone::Danger),
                ),
                kpi(
                    "Critical",
                    critical.to_string(),
                    format!("S1 {s1}, S2 {s2}{tests_note}"),
                    None,
                ),
                kpi(
                    "Past due date",
                    past_due.to_string(),
                    format!("of {with_due} with a date{tests_note}"),
                    None,
                ),
                kpi(
                    &format!("Last {} days", cfg.closed_window_days),
                    format!("-{} / +{added}", window.closed),
                    "closed / added (new or moved in)".into(),
                    None,
                ),
            ],
            tracks,
            date_groups: Vec::new(),
            undated: 0,
            blockers,
            items: work_items,
            tests: test_items,
            notes: Vec::new(),
            pm: None,
            health: None,
            initiatives: Vec::new(),
            editable_commitments: false,
        }
    }

    /// One Future work card, and why its PM page date could not be used.
    fn initiative_card(&self, data: &InitiativeData) -> (InitiativeCard, Option<String>) {
        let (page, pm_unread) = self.pm_page(data.config.pm_page);
        // The epics' fixVersion dates, else their due dates.
        let release = data
            .epics
            .iter()
            .flat_map(|e| {
                e.fix_versions
                    .iter()
                    .filter_map(|n| self.version(e.key.project(), n).and_then(|v| v.release))
            })
            .max();
        let jira_date = release.or_else(|| data.epics.iter().filter_map(|e| e.due).max());
        let refs: Vec<&Ticket> = data.open.iter().collect();
        let pm_date = page.and_then(|p| p.target(None));
        let pm_note = page.filter(|_| pm_date.is_none()).map(|p| pm_gap(p, None));
        let track = rules::evaluate_track(
            self.cfg,
            self.today,
            TrackInput {
                name: data.config.name.clone(),
                items: refs.clone(),
                closed_in_window: data.closed_in_window,
                pm_date,
                pm: page.map(|p| p.info.clone()),
                pm_unread,
                jira_date,
                start_date: None,
            },
        );
        let card = InitiativeCard {
            track,
            epics: data
                .epics
                .iter()
                .map(|e| EpicRef {
                    key: e.key.clone(),
                    url: e.url.clone(),
                    summary: e.summary.clone(),
                    status: e.status.clone(),
                    owner: e.assignee.clone(),
                })
                .collect(),
            blockers: rules::release_blockers(self.cfg, self.today, &refs),
        };
        (card, pm_note)
    }

    fn future_view(&self) -> View {
        let mut notes = self.notes(ViewId::Future);
        let mut cards = Vec::new();
        for data in &self.raw.initiatives {
            let (card, pm_note) = self.initiative_card(data);
            cards.push(card);
            notes.extend(pm_note);
        }
        let items: Vec<Ticket> = unique(self.raw.initiatives.iter().flat_map(|i| &i.open))
            .into_iter()
            .map(|t| self.item(t.clone()))
            .collect();
        let verdict = cards
            .iter()
            .map(|c| c.track.verdict)
            .max()
            .unwrap_or(Verdict::NoDate);
        let behind = cards
            .iter()
            .filter(|c| c.track.verdict >= Verdict::AtRisk)
            .count();
        let blockers: usize = cards.iter().map(|c| c.blockers.len()).sum();
        let summary = cards
            .iter()
            .find(|c| c.track.verdict == verdict)
            .map_or_else(
                || "No initiatives configured".to_string(),
                |c| format!("{}: {}", c.track.name, c.track.reason),
            );
        let view = View {
            id: ViewId::Future,
            label: ViewId::Future.label().into(),
            kind: ViewKind::Initiatives,
            verdict,
            summary,
            kpis: vec![
                kpi(
                    "Initiatives",
                    cards.len().to_string(),
                    format!("{behind} at risk or off track"),
                    None,
                ),
                kpi(
                    "Open items",
                    items.len().to_string(),
                    "across every initiative".into(),
                    None,
                ),
                kpi(
                    "Blockers",
                    blockers.to_string(),
                    "test cases not counted".into(),
                    (blockers > 0).then_some(Tone::Danger),
                ),
            ],
            tracks: Vec::new(),
            date_groups: Vec::new(),
            undated: 0,
            blockers: Vec::new(),
            items,
            tests: Vec::new(),
            notes,
            pm: None,
            health: None,
            initiatives: cards,
            editable_commitments: false,
        };
        self.with_gaps(view)
    }
}

/// Why a PM page gives a track no date.
fn pm_gap(page: &PmPage, label: Option<&str>) -> String {
    let wanted = label.map_or_else(|| "target".to_string(), |l| format!("\"{l}\""));
    let cell = page.info.target.as_deref().unwrap_or("empty");
    format!(
        "PM page \"{}\" gives no {wanted} date (Target Date: {cell}); the Jira date is used.",
        page.info.title
    )
}

/// Distinct keys across `names` in a per-fixVersion key map.
fn count_in(map: &KeysByVersion, names: &[String]) -> usize {
    names
        .iter()
        .flat_map(|n| map.get(n).into_iter().flatten())
        .collect::<BTreeSet<_>>()
        .len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitments::Commitment;
    use crate::config::InitiativeConfig;
    use crate::confluence::LabelledDate;
    use crate::fetch::Note;
    use crate::model::{ChangeStatus, HealthPart, Instance, LinkRef, PatchRef, PmRef};
    use crate::rules::tests::{d, idle, ticket, today};
    use url::Url;

    const ALL: ViewId = ViewId::All;
    const OTHERS: ViewId = ViewId::Others;
    const GCP: ViewId = ViewId::Gcp;
    const NVIDIA: ViewId = ViewId::Nvidia;
    const EXA6: ViewId = ViewId::Exa6;
    const EXA7_BETA: ViewId = ViewId::Exa7Beta;
    const EXA7_GA: ViewId = ViewId::Exa7Ga;
    const EXA_SD: ViewId = ViewId::ExaSd;
    const FUTURE: ViewId = ViewId::Future;

    fn version(name: &str, release: Option<NaiveDate>, start: Option<NaiveDate>) -> Version {
        Version {
            name: name.into(),
            release,
            start,
            released: false,
            archived: false,
        }
    }

    fn in_version(key: &str, version: &str, status: &str, category: Category) -> Ticket {
        let mut t = ticket(key, key);
        t.fix_versions = vec![version.into()];
        t.status = status.into();
        t.category = category;
        t
    }

    fn keys(entries: &[(&str, &[&str])]) -> KeysByVersion {
        entries
            .iter()
            .map(|(v, ks)| {
                (
                    v.to_string(),
                    ks.iter().map(|k| k.parse().unwrap()).collect(),
                )
            })
            .collect()
    }

    fn link(key: &str, relation: &str, done: bool) -> LinkRef {
        LinkRef {
            key: key.parse().unwrap(),
            relation: relation.into(),
            done,
        }
    }

    fn commitment(date: NaiveDate) -> Commitment {
        Commitment {
            date,
            note: None,
            source: "tracker".into(),
            updated: Local::now(),
        }
    }

    /// A small but complete refresh: one customer of each group, two EXA6
    /// releases, Beta, GA, one EXA SD phase and one initiative.
    fn raw() -> RawData {
        let cfg = Config::default();
        let mut nvidia = idle(
            ticket("DDN-7162", "Case 01349748 | NVIDIA | HCAs missing"),
            8,
        );
        (nvidia.severity, nvidia.source) = (Some(Severity::field(1)), Some("DDN".into()));
        nvidia.links = vec![link("EX-100", "is blocked by", false)];
        let mut gcp = idle(ticket("GCP-315", "Client hung in D-state"), 1);
        (gcp.instance, gcp.source) = (Instance::Whamcloud, Some("Whamcloud".into()));
        // The Whamcloud copy of EX-300 still says open; the cloud ticket is done.
        gcp.links = vec![link("EX-300", "is blocked by", false)];
        let mut other = idle(
            ticket("DDN-3571", "LustreError: ASSERTION in cl_page"),
            1041,
        );
        (other.severity, other.source) = (Some(Severity::field(1)), Some("DDN".into()));
        let mut delivered = idle(ticket("DDN-6000", "fixed already"), 1);
        delivered.source = Some("DDN".into());
        delivered.links = vec![link("EX-200", "relates to", true)];
        let mut half_read = idle(ticket("DDN-6001", "one fix not readable"), 1);
        half_read.source = Some("DDN".into());
        half_read.links = vec![
            link("EX-200", "relates to", true),
            link("EX-404", "relates to", true),
        ];
        let mut escalation = idle(ticket("DDN-6741", "escalation bookkeeping only"), 1);
        escalation.source = Some("DDN".into());
        escalation.links = vec![link("EX-500", "Occurred in Escalation", false)];
        let mut exr_fixed = idle(ticket("DDN-6900", "fixed in an EXR version"), 1);
        exr_fixed.source = Some("DDN".into());
        exr_fixed.links = vec![link("EXR-7", "is related to", false)];
        let test_case = ticket("DDN-5885", "GCP - P1 [TEST] Test Support Case");

        let fix_open = in_version("EX-100", "ES6.3.9_C1", "In Review", Category::Indeterminate);
        let fix_done = in_version("EX-200", "ES6.3.9", "Done", Category::Done);
        let fix_cloud_done = in_version("EX-300", "ES6.3.9", "Done", Category::Done);
        let escalation_record = in_version("EX-500", "ES6.3.9_C1", "To Do", Category::New);
        let exr_fix = in_version(
            "EXR-7",
            "ES6.3.9_C1",
            "In Progress",
            Category::Indeterminate,
        );

        let mut blocker = in_version("EX-1", "ES6.3.10", "To Do", Category::New);
        blocker.priority = Some("Blocker".into());
        let mut ex_open: Vec<Ticket> = (2..=cfg.small_release_max + 3)
            .map(|i| {
                in_version(
                    &format!("EX-{i}"),
                    "ES6.3.10",
                    "In Review",
                    Category::Indeterminate,
                )
            })
            .collect();
        ex_open.push(blocker);
        ex_open.push(idle(
            in_version("EX-50", "ES6.3.9_C1", "To Do", Category::New),
            1,
        ));
        ex_open.push(idle(
            in_version(
                "EX-60",
                "ES7.0.0_Beta",
                "In Progress",
                Category::Indeterminate,
            ),
            1,
        ));
        ex_open.push(idle(
            in_version("EX-70", "ES7.1.0", "To Do", Category::New),
            1,
        ));

        let mut sd_test = in_version("EXM-9", "NVMesh-P2", "New", Category::New);
        (sd_test.issue_type, sd_test.priority) = ("Test".into(), Some("Blocker".into()));
        let sd_open = vec![
            idle(
                in_version("EXM-1", "NVMesh-P2", "In Progress", Category::Indeterminate),
                1,
            ),
            sd_test,
        ];

        let mut epic = ticket("EXM-458", "New HA initiative: design and framework");
        epic.fix_versions = vec!["NVMesh-P2".into()];
        let initiative = InitiativeData {
            config: InitiativeConfig {
                name: "New HA (EAM)".into(),
                pm_page: None,
                epics: vec!["EXM-458".parse().unwrap()],
                jql: None,
                in_exa_sd: true,
            },
            epics: vec![epic],
            open: vec![idle(
                in_version("EXM-500", "NVMesh-P3", "New", Category::New),
                1,
            )],
            closed_in_window: 4,
        };

        let mut commitments = crate::commitments::Commitments::new();
        commitments.insert("NP-131".parse().unwrap(), commitment(d(2026, 11, 1)));
        commitments.insert("DDN-3571".parse().unwrap(), commitment(d(2026, 11, 2)));
        let mut ncp = idle(ticket("NCP-163", "OCI-IAD evictions"), 1);
        ncp.source = Some("Whamcloud".into());
        let mut np = idle(ticket("NP-131", "OCI-IAD evictions"), 1);
        np.source = Some("NP".into());

        RawData {
            today: today(),
            ex_versions: vec![
                version("ES6.3.9_C1", Some(d(2026, 11, 6)), Some(d(2026, 10, 14))),
                version("ES6.3.10", Some(d(2026, 12, 15)), Some(d(2026, 9, 1))),
                version("ES7.0.0_Beta", Some(d(2026, 11, 30)), None),
                version("ES7.0.0", None, None),
                version("ES7.1.0", Some(d(2027, 1, 29)), Some(d(2026, 10, 30))),
            ],
            sd_versions: vec![
                version("NVMesh-P2", Some(d(2026, 11, 30)), None),
                version("NVMesh-P3", Some(d(2027, 1, 29)), None),
            ],
            customers: vec![
                gcp, nvidia, other, delivered, half_read, escalation, exr_fixed, test_case, ncp, np,
            ],
            ex_open,
            sd_open,
            ex_closed: keys(&[
                ("ES6.3.10", &["EX-90", "EX-91", "EX-92", "EX-93"]),
                ("ES7.0.0_Beta", &["EX-94"]),
                ("ES7.0.0", &["EX-95"]),
            ]),
            sd_closed: keys(&[("NVMesh-P2", &["EXM-90", "EXM-91"])]),
            added: BTreeMap::from([
                (EXA6, Some(2)),
                (EXA7_BETA, Some(0)),
                (EXA7_GA, Some(0)),
                (EXA_SD, None),
            ]),
            unscheduled: Some(395),
            pm_pages: HashMap::new(),
            fixes: [
                fix_open,
                fix_done,
                fix_cloud_done,
                escalation_record,
                exr_fix,
            ]
            .into_iter()
            .map(|t| (t.key.clone(), t))
            .collect(),
            commitments,
            initiatives: vec![initiative],
            health: None,
            sources: vec![SourceStatus {
                name: "Jira cloud".into(),
                error: None,
            }],
            notes: vec![Note::info(
                [EXA_SD],
                "EXA SD added: not read (timed out)".into(),
            )],
        }
    }

    fn timing() -> Timing {
        Timing {
            generated_at: Local::now(),
            duration_secs: 12.34,
        }
    }

    fn view(snap: &Snapshot, id: ViewId) -> &View {
        snap.tabs
            .iter()
            .flat_map(|t| &t.views)
            .find(|v| v.id == id)
            .unwrap()
    }

    fn item<'a>(snap: &'a Snapshot, id: ViewId, key: &str) -> &'a Ticket {
        view(snap, id)
            .items
            .iter()
            .find(|t| t.key.as_str() == key)
            .unwrap()
    }

    fn build(raw: &RawData) -> Snapshot {
        snapshot(&Config::default(), raw, GerritState::NotRead, timing()).unwrap()
    }

    #[test]
    fn customers_are_grouped_dated_and_judged() {
        let snap = build(&raw());
        let summary = |id: ViewId| {
            let v = view(&snap, id);
            (
                v.items.iter().map(|t| t.key.as_str()).collect::<Vec<_>>(),
                v.verdict,
                v.summary.clone(),
            )
        };

        assert_eq!(
            summary(OTHERS),
            (
                vec!["DDN-3571", "DDN-6000", "DDN-6001", "DDN-6741", "DDN-6900"],
                Verdict::OffTrack,
                "1 S1/S2 ticket past the idle limit, 1 of them idle over 30 days".to_string()
            )
        );
        assert_eq!(summary(GCP).0, ["GCP-315"]);
        assert_eq!(summary(NVIDIA).1, Verdict::AtRisk);
        assert_eq!(
            snap.tabs.first().map(|t| t.verdict),
            Some(Verdict::OffTrack)
        );
        assert_eq!(snap.format, SNAPSHOT_FORMAT);
    }

    #[test]
    fn a_promise_filed_under_the_mirror_copy_shows_on_the_row_and_names_its_key() {
        let snap = build(&raw());

        let row = item(&snap, NVIDIA, "NCP-163");
        assert_eq!(
            row.committed.as_ref().map(|c| (c.date, c.key.as_str())),
            Some((d(2026, 11, 1), "NP-131"))
        );
        assert_eq!(
            item(&snap, OTHERS, "DDN-3571").committed,
            None,
            "All others has no committed dates"
        );
        assert!(view(&snap, GCP).editable_commitments && !view(&snap, OTHERS).editable_commitments);
    }

    #[test]
    fn the_all_view_shows_every_customer_row_as_its_group_does() {
        let mut raw = raw();
        // Due the same day as NVIDIA's NCP-163.
        let mut due = idle(ticket("GCP-400", "Client crash"), 1);
        (due.instance, due.source) = (Instance::Whamcloud, Some("Whamcloud".into()));
        raw.customers.push(due);
        raw.commitments
            .insert("GCP-400".parse().unwrap(), commitment(d(2026, 11, 1)));

        let snap = build(&raw);
        let groups = [OTHERS, GCP, NVIDIA];
        let rows: Vec<&Ticket> = groups
            .iter()
            .flat_map(|id| &view(&snap, *id).items)
            .collect();
        let due_nov_1 = |id: ViewId| {
            view(&snap, id)
                .date_groups
                .iter()
                .find(|g| g.date == d(2026, 11, 1))
                .map(|g| g.keys.len())
        };

        assert_eq!(view(&snap, ALL).items.len(), rows.len());
        for row in rows {
            assert_eq!(item(&snap, ALL, row.key.as_str()), row);
        }
        assert_eq!(
            (due_nov_1(ALL), due_nov_1(GCP), due_nov_1(NVIDIA)),
            (Some(2), Some(1), Some(1)),
            "one date group across the customers"
        );
        assert_eq!(
            Some(view(&snap, ALL).verdict),
            groups.iter().map(|id| view(&snap, *id).verdict).max()
        );
        assert!(
            !view(&snap, ALL).editable_commitments,
            "dates are set in the group views"
        );
        assert_eq!(
            snap.tabs
                .first()
                .map(|t| t.views.iter().map(|v| v.id).collect::<Vec<_>>()),
            Some(vec![ALL, OTHERS, GCP, NVIDIA])
        );
    }

    #[test]
    fn tickets_awaiting_verification_say_so_in_the_tables_and_the_blocker_lists() {
        let mut raw = raw();
        if let Some(t) = raw.ex_open.iter_mut().find(|t| t.key.as_str() == "EX-1") {
            t.status = "Awaiting Verification".into();
        }
        if let Some(t) = raw
            .customers
            .iter_mut()
            .find(|t| t.key.as_str() == "GCP-315")
        {
            t.labels = vec!["awaiting_validation".into()];
        }

        let snap = build(&raw);
        let verification = |id: ViewId, key: &str| item(&snap, id, key).verification.clone();
        let blocker = view(&snap, EXA6)
            .blockers
            .iter()
            .find(|b| b.key.as_str() == "EX-1")
            .unwrap();

        assert_eq!(
            verification(EXA6, "EX-1").as_deref(),
            Some("Awaiting Verification")
        );
        assert_eq!(
            blocker.verification.as_deref(),
            Some("Awaiting Verification")
        );
        assert_eq!(
            verification(GCP, "GCP-315").as_deref(),
            Some("awaiting_validation")
        );
        assert_eq!(
            verification(ALL, "GCP-315").as_deref(),
            Some("awaiting_validation")
        );
        assert_eq!(verification(NVIDIA, "DDN-7162"), None);
    }

    #[test]
    fn the_open_tile_counts_tickets_by_source_for_the_page_to_filter() {
        let snap = build(&raw());
        let by_source = |id: ViewId| {
            view(&snap, id)
                .kpis
                .iter()
                .filter(|k| !k.by_source.is_empty())
                .map(|k| {
                    let counts = k
                        .by_source
                        .iter()
                        .map(|s| format!("{} {}", s.source, s.count));
                    (
                        k.label.as_str(),
                        k.note.as_str(),
                        counts.collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            by_source(ALL),
            [(
                "Open",
                "DDN 6, Whamcloud 2",
                vec!["DDN 6".to_string(), "Whamcloud 2".to_string()]
            )]
        );
        assert_eq!(
            by_source(OTHERS),
            [("Open", "DDN 5", vec!["DDN 5".to_string()])]
        );
        assert_eq!(by_source(EXA6), [], "release tiles have no sources");
    }

    #[test]
    fn the_all_view_adds_nothing_to_the_watch_list() {
        let cfg = Config::default();
        let mut raw = raw();
        // A blocker in All others that is not S1/S2, so that view does not watch it.
        let mut blocker = idle(ticket("DDN-8000", "Mount hangs"), 1);
        (blocker.priority, blocker.source) = (Some("Blocker".into()), Some("DDN".into()));
        raw.customers.push(blocker);

        let snap = build(&raw);

        assert!(
            view(&snap, ALL)
                .blockers
                .iter()
                .any(|b| b.key.as_str() == "DDN-8000")
        );
        assert!(
            watch_list(&cfg, &snap)
                .iter()
                .all(|w| w.key.as_str() != "DDN-8000")
        );
    }

    #[test]
    fn only_ex_fixes_give_dates() {
        let snap = build(&raw());
        let fix = |id: ViewId, key: &str| {
            item(&snap, id, key)
                .fix
                .as_ref()
                .map(|f| (f.key.to_string(), f.date))
        };

        assert_eq!(
            fix(NVIDIA, "DDN-7162"),
            Some(("EX-100".into(), d(2026, 11, 6)))
        );
        assert_eq!(
            fix(OTHERS, "DDN-6900"),
            None,
            "EXR fixVersions are planning quarters, not releases"
        );
    }

    #[test]
    fn an_escalation_link_is_not_a_fix() {
        let snap = build(&raw());
        let row = item(&snap, OTHERS, "DDN-6741");

        assert_eq!((row.fix.as_ref(), row.fix_state), (None, FixState::NoFix));
    }

    #[test]
    fn fix_states_follow_the_linked_fixes() {
        let snap = build(&raw());
        let state = |id: ViewId, key: &str| item(&snap, id, key).fix_state;

        assert_eq!(state(OTHERS, "DDN-6000"), FixState::Delivered);
        assert_eq!(
            state(OTHERS, "DDN-6001"),
            FixState::UnderWay,
            "one fix is done, EX-404 was not read"
        );
        assert_eq!(state(NVIDIA, "DDN-7162"), FixState::UnderWay);
        assert_eq!(
            state(OTHERS, "DDN-6900"),
            FixState::UnderWay,
            "an EXR fix counts, though it gives no date"
        );
        assert_eq!(state(OTHERS, "DDN-3571"), FixState::NoFix);
        assert_eq!(fix_state(1, &[]), FixState::NoFix);
        let new = ticket("EX-9", "x");
        assert_eq!(fix_state(1, &[&new]), FixState::NotStarted);
    }

    #[test]
    fn link_states_come_from_the_cloud_copy_of_the_fix() {
        let snap = build(&raw());

        assert!(item(&snap, GCP, "GCP-315").links.iter().all(|l| l.done));
        assert!(
            view(&snap, GCP)
                .blockers
                .iter()
                .all(|b| b.reason != "Blocked by EX-300")
        );
        assert_eq!(item(&snap, GCP, "GCP-315").fix_state, FixState::Delivered);
    }

    #[test]
    fn gcp_and_nvidia_tickets_are_rated_from_their_priority() {
        let mut raw = raw();
        let mut critical = idle(ticket("GCP-400", "Client crash"), 8);
        (critical.priority, critical.source) = (Some("Critical".into()), Some("Whamcloud".into()));
        let mut major = idle(ticket("NVDCSE-250", "Slow recovery"), 2);
        (major.priority, major.source) = (Some("major".into()), Some("Whamcloud".into()));
        let mut blocker = idle(ticket("NCP-170", "Mount hangs"), 1);
        (blocker.priority, blocker.source) = (Some("Blocker".into()), Some("Whamcloud".into()));
        let mut ddn = idle(ticket("DDN-9000", "No severity set"), 8);
        (ddn.priority, ddn.source) = (Some("Critical".into()), Some("DDN".into()));
        raw.customers.extend([critical, major, blocker, ddn]);

        let snap = build(&raw);
        let rating = |id: ViewId, key: &str| {
            let t = item(&snap, id, key);
            (
                t.severity_level(),
                t.severity.as_ref().is_some_and(Severity::is_from_priority),
            )
        };

        assert_eq!(rating(GCP, "GCP-400"), (Some(1), true));
        assert_eq!(rating(NVIDIA, "NVDCSE-250"), (Some(2), true));
        assert_eq!(rating(NVIDIA, "NCP-170"), (Some(1), true));
        assert_eq!(rating(GCP, "GCP-315"), (None, false), "Medium has no level");
        assert_eq!(
            rating(OTHERS, "DDN-9000"),
            (None, false),
            "DDN tickets keep their own severity field"
        );
        assert_eq!(view(&snap, GCP).verdict, Verdict::AtRisk);
        assert_eq!(
            view(&snap, GCP)
                .blockers
                .first()
                .map(|b| (b.key.as_str(), b.reason.as_str())),
            Some(("GCP-400", "S1 (Critical), idle 8d"))
        );
        assert!(view(&snap, GCP).notes.iter().any(|n| n
            == "Rated from the priority (Blocker as S1, Critical as S1, Major as S2): 1 ticket."));
    }

    #[test]
    fn rows_without_a_severity_are_called_out() {
        let snap = build(&raw());

        assert!(
            view(&snap, GCP)
                .notes
                .iter()
                .any(|n| n == "No severity, so the S1/S2 idle limits do not apply: 1 ticket.")
        );
    }

    #[test]
    fn a_failed_customer_source_caps_the_verdict_and_says_so() {
        let mut raw = raw();
        raw.notes.push(Note::gap(
            [ALL, GCP, NVIDIA],
            "Whamcloud: not read (HTTP 503)".into(),
        ));

        let snap = build(&raw);
        let gcp = view(&snap, GCP);

        assert_eq!(gcp.verdict, Verdict::AtRisk);
        assert!(
            gcp.summary.starts_with("Incomplete data, see the notes. "),
            "{}",
            gcp.summary
        );
        assert!(
            gcp.notes
                .iter()
                .any(|n| n == "Whamcloud: not read (HTTP 503)")
        );
        assert_eq!(
            capped(&snap),
            [ALL, GCP, NVIDIA],
            "views the source does not feed are not affected"
        );
        assert!(
            view(&snap, ALL)
                .notes
                .iter()
                .any(|n| n == "Whamcloud: not read (HTTP 503)")
        );
        assert!(!complete(&raw));
    }

    /// The views that read "Incomplete data".
    fn capped(snap: &Snapshot) -> Vec<ViewId> {
        snap.tabs
            .iter()
            .flat_map(|t| &t.views)
            .filter(|v| v.summary.starts_with("Incomplete data"))
            .map(|v| v.id)
            .collect()
    }

    #[test]
    fn unread_committed_dates_cap_only_the_views_that_carry_them() {
        let mut raw = raw();
        raw.notes.push(Note::gap(
            [ALL, GCP, NVIDIA],
            "Committed dates not read: x".into(),
        ));

        assert_eq!(capped(&build(&raw)), [ALL, GCP, NVIDIA]);
    }

    #[test]
    fn an_unread_pm_page_caps_the_views_that_use_its_date_and_says_so_on_the_track() {
        let mut raw = raw();
        raw.notes.push(Note::gap(
            [EXA7_BETA, EXA7_GA],
            "PM page 3994616067 could not be read, so the Jira date is used.".into(),
        ));

        let snap = build(&raw);
        let beta = view(&snap, EXA7_BETA);
        let track = beta.tracks.first().unwrap();

        assert_eq!(capped(&snap), [EXA7_BETA, EXA7_GA]);
        assert_eq!(beta.verdict, Verdict::AtRisk);
        assert!(track.pm_unread && track.pm.is_none());
        assert!(
            track.reason.ends_with(". PM page not read"),
            "{}",
            track.reason
        );
    }

    #[test]
    fn a_release_jira_lacks_is_not_judged_and_caps_only_its_view() {
        let mut raw = raw();
        raw.ex_versions
            .retain(|v| v.name != "ES7.0.0_Beta" && v.name != "ES7.1.0");
        raw.notes.extend([
            Note::gap(
                [EXA7_BETA],
                "Jira has no fixVersion ES7.0.0_Beta; fix its name in the config.".into(),
            ),
            Note::gap(
                [EXA7_GA],
                "Jira has no fixVersion ES7.1.0; fix its name in the config.".into(),
            ),
        ]);

        let snap = build(&raw);
        let track = |id: ViewId| {
            let t = view(&snap, id).tracks.first().unwrap();
            (t.verdict, t.reason.clone())
        };

        assert_eq!(
            track(EXA7_BETA),
            (
                Verdict::NoDate,
                "Jira has no fixVersion ES7.0.0_Beta".to_string()
            )
        );
        assert!(
            track(EXA7_GA)
                .1
                .ends_with(". Jira has no fixVersion ES7.1.0")
        );
        assert_eq!(capped(&snap), [EXA7_BETA, EXA7_GA]);
        assert_eq!(view(&snap, EXA7_BETA).verdict, Verdict::AtRisk);
    }

    #[test]
    fn a_pm_release_or_phase_that_is_not_open_is_called_out() {
        let cfg = Config {
            pm_pages: crate::config::PmPages {
                exa6_release: "ES6.3.11".into(),
                ..Config::default().pm_pages
            },
            exa_sd_pm_phase: "NVMesh-P9".into(),
            ..Config::default()
        };

        let snap = snapshot(&cfg, &raw(), GerritState::NotRead, timing()).unwrap();

        assert!(
            view(&snap, EXA6).notes.iter().any(
                |n| n == "No open release is called ES6.3.11, so its PM page date is not used."
            )
        );
        assert!(
            view(&snap, EXA_SD).notes.iter().any(|n| n
                == "No open phase is called NVMesh-P9, so the EXA SD PM page date is not used.")
        );
    }

    #[test]
    fn releases_are_projected_against_their_dates() {
        let snap = build(&raw());
        let tracks = |id: ViewId| -> Vec<(String, usize, Verdict)> {
            view(&snap, id)
                .tracks
                .iter()
                .map(|t| (t.name.clone(), t.open, t.verdict))
                .collect()
        };

        assert_eq!(
            tracks(EXA6),
            [
                ("ES6.3.9_C1".into(), 1, Verdict::OnTrack),
                (
                    "ES6.3.10".into(),
                    Config::default().small_release_max + 3,
                    Verdict::OnTrack
                )
            ]
        );
        assert_eq!(
            tracks(EXA7_BETA),
            [("ES7.0.0_Beta".into(), 1, Verdict::OnTrack)]
        );
        assert_eq!(
            tracks(EXA7_GA),
            [("EXA7 GA (ES7.0.0 + ES7.1.0)".into(), 1, Verdict::OnTrack)]
        );
        assert_eq!(
            tracks(EXA_SD),
            [
                ("NVMesh-P2".into(), 2, Verdict::OnTrack),
                ("NVMesh-P3".into(), 0, Verdict::OnTrack),
                ("New HA (EAM)".into(), 1, Verdict::OnTrack)
            ]
        );
        let exa6 = view(&snap, EXA6);
        assert_eq!(exa6.blockers.first().map(|b| b.key.as_str()), Some("EX-1"));
        assert!(
            view(&snap, EXA_SD)
                .notes
                .iter()
                .any(|n| n == "EXA SD added: not read (timed out)")
        );
        assert!(
            !exa6.notes.iter().any(|n| n.starts_with("EXA SD added")),
            "a note stays in the view it concerns"
        );
        assert!(exa6.notes.iter().any(|n| n.starts_with("395 open items")));
        assert!(
            view(&snap, EXA_SD).blockers.is_empty(),
            "the Blocker test case is not a blocker"
        );
    }

    #[test]
    fn exa_sd_lists_test_items_apart_and_leaves_them_out_of_open_scope() {
        let mut raw = raw();
        let scope = |raw: &RawData| {
            let snap = build(raw);
            let v = view(&snap, EXA_SD);
            let open = v.kpis.first().unwrap();
            let keys = |list: &[Ticket]| list.iter().map(|t| t.key.to_string()).collect::<Vec<_>>();
            (
                open.value.clone(),
                open.note.clone(),
                keys(&v.items),
                keys(&v.tests),
            )
        };

        assert_eq!(
            scope(&raw),
            (
                "1".to_string(),
                "1 is a Test item".to_string(),
                vec!["EXM-1".to_string()],
                vec!["EXM-9".to_string()]
            )
        );
        let mut test = in_version("EXM-10", "NVMesh-P2", "New", Category::New);
        test.issue_type = "Test".into();
        raw.sd_open.push(test);
        assert_eq!(
            scope(&raw),
            (
                "1".to_string(),
                "2 are Test items".to_string(),
                vec!["EXM-1".to_string()],
                vec!["EXM-9".to_string(), "EXM-10".to_string()]
            ),
            "Test items sort as the work does: Blocker priority first"
        );
    }

    #[test]
    fn an_unread_count_shows_as_unknown_not_zero() {
        let mut raw = raw();
        raw.unscheduled = None;
        let snap = build(&raw);
        let window_kpi = |id: ViewId| view(&snap, id).kpis.last().map(|k| k.value.clone());

        assert_eq!(window_kpi(EXA6), Some("-4 / +2".into()));
        assert_eq!(window_kpi(EXA_SD), Some("-2 / +?".into()));
        assert!(
            view(&snap, EXA6)
                .notes
                .iter()
                .any(|n| n == "Items in ES6.3.x (TBD) not counted: that query failed.")
        );
    }

    fn pm_page(target: &str, dates: Vec<LabelledDate>) -> PmPage {
        PmPage {
            info: PmRef {
                title: "EXAScaler 7".into(),
                url: Url::parse("https://x/").unwrap(),
                status: Some("ON TRACK".into()),
                target: Some(target.into()),
                updated: None,
            },
            dates,
        }
    }

    #[test]
    fn a_pm_page_without_the_wanted_date_is_called_out() {
        let mut raw = raw();
        let pages = Config::default().pm_pages;
        raw.pm_pages.insert(
            pages.exa7,
            pm_page(
                "Beta: 30 Nov 2026, GA: TBD",
                vec![LabelledDate {
                    label: Some("Beta".into()),
                    date: d(2026, 11, 30),
                }],
            ),
        );

        let snap = build(&raw);

        assert_eq!(
            view(&snap, EXA7_BETA)
                .tracks
                .first()
                .and_then(|t| t.pm_date),
            Some(d(2026, 11, 30))
        );
        assert!(
            view(&snap, EXA7_GA)
                .notes
                .iter()
                .any(|n| n == "PM page \"EXAScaler 7\" gives no \"GA\" date (Target Date: Beta: 30 Nov 2026, GA: TBD); the Jira date is used."),
            "{:?}",
            view(&snap, EXA7_GA).notes
        );
    }

    #[test]
    fn initiatives_get_cards_with_epics_and_the_phase_date() {
        let snap = build(&raw());
        let future = view(&snap, FUTURE);
        let card = future.initiatives.first().unwrap();

        assert_eq!(
            (
                card.track.name.as_str(),
                card.track.jira_date,
                card.track.open
            ),
            ("New HA (EAM)", Some(d(2026, 11, 30)), 1)
        );
        assert_eq!(
            card.epics
                .iter()
                .map(|e| e.key.as_str())
                .collect::<Vec<_>>(),
            ["EXM-458"]
        );
        assert_eq!(future.items.len(), 1);
    }

    #[test]
    fn gerrit_results_reach_the_items_and_the_sources() {
        let raw = raw();
        let patch = PatchRef {
            number: 66674,
            url: Url::parse("https://review.whamcloud.com/c/ex/lustre-release/+/66674").unwrap(),
            branch: "b_es6_0".into(),
            status: ChangeStatus::Merged,
            first_tag: Some("2.14.0-ddn258".into()),
        };
        let patches = Patches {
            map: HashMap::from([
                ("DDN-7162".parse().unwrap(), vec![patch.clone()]),
                ("EX-1".parse().unwrap(), vec![patch.clone()]),
            ]),
            errors: vec!["change search: timed out".into()],
        };
        let gerrit_error = |snap: &Snapshot| {
            snap.sources
                .last()
                .map(|s| (s.name.clone(), s.error.clone()))
        };

        let snap = snapshot(
            &Config::default(),
            &raw,
            GerritState::Read(&patches),
            timing(),
        )
        .unwrap();

        assert_eq!(item(&snap, NVIDIA, "DDN-7162").patches, vec![patch.clone()]);
        assert_eq!(item(&snap, EXA6, "EX-1").patches, vec![patch]);
        assert_eq!(
            gerrit_error(&snap),
            Some(("Gerrit".into(), Some("change search: timed out".into())))
        );
        let failed = snapshot(
            &Config::default(),
            &raw,
            GerritState::Failed("no credentials"),
            timing(),
        )
        .unwrap();
        assert_eq!(
            gerrit_error(&failed),
            Some(("Gerrit".into(), Some("no credentials".into())))
        );
        assert_eq!(
            gerrit_error(&build(&raw)),
            Some(("Gerrit".into(), Some("not read yet".into())))
        );
    }

    #[test]
    fn the_watch_list_covers_nvidia_gcp_s1_s2_and_blockers() {
        let cfg = Config::default();
        let snap = build(&raw());

        let watched: Vec<(String, Vec<String>)> = watch_list(&cfg, &snap)
            .into_iter()
            .map(|w| {
                (
                    w.key.to_string(),
                    w.search.iter().map(ToString::to_string).collect(),
                )
            })
            .collect();

        assert_eq!(
            watched,
            [
                ("DDN-3571".to_string(), vec!["DDN-3571".to_string()]),
                (
                    "GCP-315".to_string(),
                    vec!["EX-300".to_string(), "GCP-315".to_string()]
                ),
                (
                    "DDN-7162".to_string(),
                    vec!["DDN-7162".to_string(), "EX-100".to_string()]
                ),
                (
                    "NCP-163".to_string(),
                    vec!["NCP-163".to_string(), "NP-131".to_string()]
                ),
                ("EX-1".to_string(), vec!["EX-1".to_string()]),
            ]
        );
    }

    #[test]
    fn health_is_split_by_branch_with_its_own_errors() {
        let mut raw = raw();
        raw.health = Some(BuildHealth {
            url: Url::parse("https://localhost:9835/").unwrap(),
            days: 7,
            es6: Some(HealthPart {
                rate: Some(55.8),
                count: 500,
                excluded: 0,
                capped: true,
                error: None,
            }),
            es7: Some(HealthPart {
                error: Some("timed out".into()),
                ..HealthPart::default()
            }),
            emf: None,
        });

        let snap = build(&raw);

        let exa6 = view(&snap, EXA6).health.clone().unwrap();
        let beta = view(&snap, EXA7_BETA).health.clone().unwrap();
        assert_eq!(
            (exa6.es6.is_some(), exa6.es7.is_some(), exa6.errors().len()),
            (true, false, 0)
        );
        assert_eq!(
            (beta.es6.is_some(), beta.errors()),
            (false, vec!["es7: timed out".to_string()])
        );
    }

    /// Every field path of a JSON value; array elements are merged.
    fn paths(v: &serde_json::Value, prefix: &str, out: &mut BTreeSet<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    let path = format!("{prefix}.{k}");
                    out.insert(path.clone());
                    paths(child, &path, out);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    paths(item, &format!("{prefix}[]"), out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn the_snapshot_layout_is_pinned_to_its_format() {
        let cfg = Config::default();
        let mut raw = raw();
        raw.health = Some(BuildHealth {
            url: Url::parse("https://localhost:9835/").unwrap(),
            days: 7,
            es6: Some(HealthPart {
                rate: Some(55.8),
                count: 500,
                excluded: 0,
                capped: true,
                error: None,
            }),
            es7: Some(HealthPart::default()),
            emf: Some(HealthPart::default()),
        });
        let page = pm_page(
            "Beta: 30 Nov 2026",
            vec![LabelledDate {
                label: Some("Beta".into()),
                date: d(2026, 11, 30),
            }],
        );
        raw.pm_pages.insert(cfg.pm_pages.gcp, page.clone());
        raw.pm_pages.insert(cfg.pm_pages.exa7, page);
        if let Some(first) = raw.initiatives.first_mut().and_then(|i| i.open.first_mut()) {
            first.priority = Some("Blocker".into());
        }
        if let Some(t) = raw
            .customers
            .iter_mut()
            .find(|t| t.key.as_str() == "DDN-7162")
        {
            t.last_comment = Some(crate::model::Comment {
                date: d(2026, 10, 13),
                author: Some("Jason".into()),
            });
        }
        let patch = PatchRef {
            number: 1,
            url: Url::parse("https://review.whamcloud.com/c/ex/lustre-release/+/1").unwrap(),
            branch: "b_es6_0".into(),
            status: ChangeStatus::Merged,
            first_tag: Some("2.14.0-ddn258".into()),
        };
        let patches = Patches {
            map: HashMap::from([("DDN-7162".parse().unwrap(), vec![patch])]),
            errors: vec![],
        };
        let snap = snapshot(&cfg, &raw, GerritState::Read(&patches), timing()).unwrap();

        let mut found = BTreeSet::new();
        paths(&serde_json::to_value(&snap).unwrap(), "", &mut found);
        let actual = found.into_iter().collect::<Vec<_>>().join("\n");
        // The file names the format its layout belongs to, so pasting a new
        // layout without bumping SNAPSHOT_FORMAT still fails.
        let (first, layout) = include_str!("snapshot_paths.txt")
            .split_once('\n')
            .unwrap_or_default();
        let pinned: u32 = first
            .strip_prefix("format ")
            .and_then(|n| n.parse().ok())
            .unwrap_or(0);
        let wanted = if pinned < SNAPSHOT_FORMAT {
            SNAPSHOT_FORMAT
        } else {
            SNAPSHOT_FORMAT + 1
        };
        let bump = if wanted > SNAPSHOT_FORMAT {
            format!("bump SNAPSHOT_FORMAT to {wanted} and ")
        } else {
            String::new()
        };
        assert!(
            pinned == SNAPSHOT_FORMAT && actual == layout.trim(),
            "the snapshot layout changed: {bump}put this in src/snapshot_paths.txt:\nformat {wanted}\n{actual}\nEND"
        );
    }
}
