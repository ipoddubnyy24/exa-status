//! Turns what one refresh read (`RawData`) into the snapshot the UI shows.
//! No I/O happens here, so every view can be tested from fixture data.

use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use url::Url;

use crate::config::{Config, TagFamily};
use crate::confluence::PmPage;
use crate::fetch::{self, InitiativeData, KeysByVersion, Patches, RawData, Watched};
use crate::jira::Version;
use crate::model::{
    BuildHealth, Category, Committed, Delivery, EpicRef, FixRef, FixState, InitiativeCard,
    IssueKey, Kpi, SNAPSHOT_FORMAT, Severity, Snapshot, SourceCount, SourceStatus, Tab, Ticket,
    TilePart, Tone, Track, Verdict, View, ViewId, ViewKind,
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
    let gerrit_error = match gerrit {
        GerritState::NotRead => Some("not read yet".to_string()),
        GerritState::Read(p) => (!p.errors.is_empty()).then(|| p.errors.join("; ")),
        GerritState::Failed(e) => Some(e.to_string()),
    };
    let ctx = Context {
        cfg,
        raw,
        patches,
        patch_gap: gerrit_error.clone(),
        releases: ReleaseDates::new(),
        today: raw.today,
        versions: all_versions(cfg, raw),
    };
    let classifier = Classifier::new(cfg)?;
    // Releases first: they date the customer tickets whose patches are in review.
    let (release_views, releases) = ctx.release_views();
    let ctx = Context { releases, ..ctx };
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
    let development_tab = tab("development", "Development", release_views);
    let future_tab = tab("future", "Future work", vec![ctx.future_view()]);

    let mut sources = raw.sources.clone();
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

/// The refresh read everything its verdicts depend on: every source, and
/// Gerrit where patches decide delivery.
pub(crate) fn complete(cfg: &Config, raw: &RawData, gerrit: GerritState<'_>) -> bool {
    let patches_read = matches!(gerrit, GerritState::Read(p) if p.errors.is_empty());
    !raw.notes.iter().any(|n| n.incomplete)
        && (patches_read || cfg.tag_families.iter().all(|f| f.views.is_empty()))
}

/// The tickets whose Gerrit changes are looked up: every row of every view.
/// Commit messages never name a customer ticket, so it is searched for by
/// the EX and LU tickets linked as its fixes too (any relation but
/// `non_fix_relations`); any other ticket only by its own key, so that a
/// linked ticket's change is not taken for its own.
pub(crate) fn watch_list(cfg: &Config, snap: &Snapshot) -> Vec<Watched> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let rows = snap.tabs.iter().flat_map(|t| &t.views).flat_map(|v| {
        let customers = v.kind == ViewKind::Customers;
        v.items.iter().chain(&v.tests).map(move |t| (customers, t))
    });
    for (customers, t) in rows {
        if !seen.insert(t.key.clone()) {
            continue;
        }
        let mut search = vec![t.key.clone()];
        search.extend(t.also.iter().cloned());
        search.extend(
            t.links
                .iter()
                .filter(|l| customers && cfg.is_fix_relation(&l.relation))
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
        value_parts: Vec::new(),
        note_parts: Vec::new(),
    }
}

/// A tile whose value and note are made of pieces, some of them counts.
fn tile(label: &str, value: Vec<TilePart>, note: Vec<TilePart>, tone: Option<Tone>) -> Kpi {
    Kpi {
        value_parts: value.clone(),
        note_parts: note.clone(),
        ..kpi(label, text_of(&value), text_of(&note), tone)
    }
}

/// What a tile shows of `parts`.
fn text_of(parts: &[TilePart]) -> String {
    parts.iter().map(|p| p.text.as_str()).collect()
}

/// Words on a tile.
fn words(text: impl Into<String>) -> TilePart {
    TilePart {
        text: text.into(),
        filter: None,
        keys: Vec::new(),
        url: None,
    }
}

/// A count on a tile of tickets no table here shows, linked to their Jira search.
fn linked(text: String, url: Option<Url>) -> TilePart {
    TilePart { url, ..words(text) }
}

/// The tickets of `tickets` that `keep` keeps.
fn subset<'t>(tickets: &[&'t Ticket], keep: impl Fn(&Ticket) -> bool) -> Vec<&'t Ticket> {
    tickets.iter().copied().filter(|t| keep(t)).collect()
}

/// A count of the tickets `keys` on a tile, shown as `text`; the table's
/// filter calls it `filter`.
fn count_of(text: String, filter: &str, keys: Vec<IssueKey>) -> TilePart {
    TilePart {
        text,
        filter: Some(filter.into()),
        keys,
        url: None,
    }
}

/// The keys of `tickets`.
fn keys_of(tickets: &[&Ticket]) -> Vec<IssueKey> {
    tickets.iter().map(|t| t.key.clone()).collect()
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
    /// The name its agreed release date is kept under (`release_dates`).
    date_key: &'s str,
}

/// A judged release or phase.
struct Judged<'s> {
    track: Track,
    items: Vec<&'s Ticket>,
    /// For its view's notes: why the PM page date could not be used, when
    /// there is a page, and what a scope filter counts.
    notes: Vec<String>,
    /// The name its date is kept under (`TrackSpec::date_key`).
    date_key: &'s str,
}

/// Release dates by the name `release_dates` keeps them under ("ES6.3.10",
/// "EXA7 Beta"); a release without a date is missing.
type ReleaseDates = BTreeMap<String, NaiveDate>;

/// Keeps a judged release's date for the customer tickets it is expected to carry.
fn keep_date(releases: &mut ReleaseDates, judged: &Judged<'_>) {
    if let Some(d) = judged.track.ref_date {
        releases.insert(judged.date_key.to_string(), d);
    }
}

/// Closures and additions during the closing window.
struct Window {
    /// The items closed.
    closed: Vec<IssueKey>,
    /// The items added; `None` when the "added" query could not be read.
    added: Option<Vec<IssueKey>>,
}

/// Shared inputs of the view builders.
struct Context<'a> {
    cfg: &'a Config,
    raw: &'a RawData,
    patches: &'a Patches,
    /// Why the patches were not all read, when they were not.
    patch_gap: Option<String>,
    /// The judged releases' dates; empty until the release views are built.
    releases: ReleaseDates,
    today: NaiveDate,
    versions: HashMap<(&'a str, &'a str), &'a Version>,
}

impl<'a> Context<'a> {
    /// An item with its Gerrit changes, what they say about its delivery in
    /// `view`, and what says its fix awaits verification.
    fn item(&self, mut t: Ticket, view: ViewId) -> Ticket {
        t.patches = self.patches.map.get(&t.key).cloned().unwrap_or_default();
        t.delivery = rules::delivery(&t.patches, &self.deciding(view));
        t.verification = rules::verification(self.cfg, &t);
        t
    }

    /// `tickets` as `view` shows them.
    fn items(&self, tickets: &[Ticket], view: ViewId) -> Vec<Ticket> {
        tickets.iter().map(|t| self.item(t.clone(), view)).collect()
    }

    /// The branches whose patches decide delivery in `view`.
    fn deciding(&self, view: ViewId) -> Vec<&'a TagFamily> {
        self.cfg
            .tag_families
            .iter()
            .filter(|f| match view {
                // The All view's tickets follow their own group's branches.
                ViewId::All => f.views.iter().any(|v| ViewId::CUSTOMERS.contains(v)),
                _ => f.views.contains(&view),
            })
            .collect()
    }

    /// The release expected to carry a customer ticket's patches in review,
    /// and its date: of their branches' releases, the one dated last, an
    /// undated one counting as last. `None` once every patch is merged.
    fn expected_release(&self, d: &Delivery) -> Option<(String, Option<NaiveDate>)> {
        d.branches
            .iter()
            .filter(|b| !b.merged)
            .filter_map(|b| self.cfg.tag_families.iter().find(|f| f.branch == b.branch))
            .map(|f| (f.release.clone(), self.releases.get(&f.release).copied()))
            .max_by_key(|(_, date)| date.unwrap_or(NaiveDate::MAX))
    }

    /// For `view`'s notes: how many of `items` patches decide, and which
    /// releases cannot date those in review. Empty where no branch decides.
    fn patch_notes(&self, view: ViewId, items: &[&Ticket], noun: &str) -> Vec<String> {
        let families = self.deciding(view);
        if families.is_empty() {
            return Vec::new();
        }
        let branches: Vec<&str> = families.iter().map(|f| f.branch.as_str()).collect();
        let decided: Vec<&Delivery> = items.iter().filter_map(|t| t.delivery.as_ref()).collect();
        let merged = decided.iter().filter(|d| d.merged).count();
        let mut notes = vec![format!(
            "Patches on {} decide the delivery of {}: {merged} merged, {} in review.",
            branches.join(" or "),
            counted(decided.len(), noun),
            decided.len() - merged
        )];
        if ViewId::CUSTOMERS.contains(&view) {
            notes.extend(
                families
                    .iter()
                    .filter(|f| !self.releases.contains_key(&f.release))
                    .map(|f| {
                        format!(
                            "{} has no date, so patches in review on {} give none.",
                            f.release, f.branch
                        )
                    }),
            );
        }
        notes
    }

    /// The fixVersion `name` of `project`.
    fn version(&self, project: &str, name: &str) -> Option<&'a Version> {
        self.versions.get(&(project, name)).copied()
    }

    /// A customer ticket with its bucket, fix, promise and patches. Link
    /// states come from the cloud copy of each fix, since links on Whamcloud
    /// tickets point at stale copies. Patches on its group's branches stand
    /// in for its linked fixes.
    fn customer(&self, classifier: &Classifier, t: Ticket) -> (Bucket, Ticket) {
        let bucket = classifier.bucket(&t);
        let Some(view) = bucket.view() else {
            return (bucket, t);
        };
        let mut t = self.item(t, view);
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
        if let Some(d) = t.delivery.as_mut() {
            t.fix = None;
            t.fix_state = if d.merged {
                FixState::Delivered
            } else {
                FixState::UnderWay
            };
            if let Some((release, date)) = self.expected_release(d) {
                (d.release, d.date) = (Some(release), date);
            }
        }
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
        let gap = self.patch_gap(view).map(|gap| {
            format!(
                "Gerrit not fully read ({gap}): where patches are missing, Jira decides delivery."
            )
        });
        self.raw
            .notes
            .iter()
            .filter(|n| n.views.contains(&view))
            .map(|n| n.text.clone())
            .chain(gap)
            .collect()
    }

    /// Why patches that decide delivery in `view` were not all read, when they were not.
    fn patch_gap(&self, view: ViewId) -> Option<&str> {
        self.patch_gap
            .as_deref()
            .filter(|_| !self.deciding(view).is_empty())
    }

    /// A view whose verdict rests on data that was not all read: it reads no
    /// better than "at risk" and its summary says so.
    fn with_gaps(&self, mut view: View) -> View {
        if self.patch_gap(view.id).is_some()
            || self
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
            closed: keys_in(closed, versions),
            added: self.raw.added.get(&view).cloned().flatten(),
        }
    }

    /// A Jira search for the tickets `keys`; none for none.
    fn jira_search(&self, keys: &[IssueKey]) -> Option<Url> {
        if keys.is_empty() {
            return None;
        }
        let mut url = self.raw.jira_cloud.join("issues/").ok()?;
        let list: Vec<&str> = keys.iter().map(IssueKey::as_str).collect();
        url.query_pairs_mut()
            .append_pair("jql", &format!("key in ({}) ORDER BY key", list.join(", ")));
        Some(url)
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
        let rated = |level: u8| -> Vec<&Ticket> {
            refs.iter()
                .copied()
                .filter(|t| t.severity_level() == Some(level))
                .collect()
        };
        let (s1, s2) = (rated(1), rated(2));
        let unrated = refs.iter().filter(|t| t.severity.is_none()).count();
        let by_priority = refs
            .iter()
            .filter(|t| t.severity.as_ref().is_some_and(Severity::is_from_priority))
            .count();
        let idle = rules::severity_idle(cfg, &refs);
        // A complete ticket waits for its tag, so its idle time is no stall.
        let (complete, waiting): (Vec<&Ticket>, Vec<&Ticket>) =
            refs.iter().partition(|t| rules::is_complete(t));
        let idle_over = |days: i64| -> Vec<&Ticket> {
            waiting
                .iter()
                .copied()
                .filter(|t| t.idle_days.is_some_and(|i| i > days))
                .collect()
        };
        let (idle_long, idle_month) = (
            idle_over(cfg.idle_alert_days),
            idle_over(cfg.idle_offtrack_days),
        );
        let idle_unknown: Vec<&Ticket> = waiting
            .iter()
            .copied()
            .filter(|t| t.idle_days.is_none())
            .collect();
        let delivered: Vec<&Ticket> = refs
            .iter()
            .copied()
            .filter(|t| t.fix_state == FixState::Delivered)
            .collect();
        let next_tag: Vec<&Ticket> = complete
            .iter()
            .copied()
            .filter(|t| {
                t.delivery
                    .as_ref()
                    .is_some_and(|d| d.branches.iter().any(|b| b.tag.is_none()))
            })
            .collect();
        let committed = refs.iter().filter(|t| t.committed.is_some()).count();
        let dated = refs.iter().filter(|t| rules::target(t).is_some()).count();
        // Nor does it need a date: it is in a tag, or due in the next one.
        let complete_undated = complete
            .iter()
            .filter(|t| rules::target(t).is_none())
            .count();
        let undated: Vec<&Ticket> = waiting
            .iter()
            .copied()
            .filter(|t| rules::target(t).is_none())
            .collect();
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
            if !idle.past_limit.is_empty() {
                parts.push(format!(
                    "{} past the idle limit, {} of them idle over {} days",
                    counted(idle.past_limit.len(), "S1/S2 ticket"),
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
        notes.extend(self.patch_notes(id, &refs, "ticket"));
        let blockers = rules::customer_blockers(cfg, self.today, &refs);
        let open = items.len();
        let idle_label = format!("Idle over {} days", cfg.idle_alert_days);
        let mut idle_note = vec![count_of(
            format!("{} over {} days", idle_month.len(), cfg.idle_offtrack_days),
            &format!("Idle over {} days", cfg.idle_offtrack_days),
            keys_of(&idle_month),
        )];
        if !idle_unknown.is_empty() {
            idle_note.push(words(", "));
            idle_note.push(count_of(
                format!("{} unknown", idle_unknown.len()),
                "Idle time unknown",
                keys_of(&idle_unknown),
            ));
        }
        let fix_note = if next_tag.is_empty() {
            vec![words("patches merged or every linked fix done")]
        } else {
            vec![
                words("patches merged or every linked fix done; "),
                count_of(
                    format!("{} due in the next tag", next_tag.len()),
                    "Due in the next tag",
                    keys_of(&next_tag),
                ),
            ]
        };
        let mut dated_note = vec![
            words(format!("{committed} committed, ")),
            count_of(
                format!("{} have no date", undated.len()),
                "No date",
                keys_of(&undated),
            ),
        ];
        if complete_undated > 0 {
            dated_note.push(words(format!(", {complete_undated} complete")));
        }
        let undated = undated.len();
        let kpis = vec![
            Kpi {
                by_source,
                ..kpi("Open", open.to_string(), source_note, None)
            },
            tile(
                "S1 / S2",
                vec![
                    count_of(s1.len().to_string(), "S1", keys_of(&s1)),
                    words(" / "),
                    count_of(s2.len().to_string(), "S2", keys_of(&s2)),
                ],
                vec![count_of(
                    format!("{} past the idle limit", idle.past_limit.len()),
                    "S1/S2 past the idle limit",
                    idle.past_limit.clone(),
                )],
                (!idle.past_limit.is_empty()).then_some(Tone::Danger),
            ),
            tile(
                &idle_label,
                vec![count_of(
                    idle_long.len().to_string(),
                    &idle_label,
                    keys_of(&idle_long),
                )],
                idle_note,
                None,
            ),
            tile(
                "Fix complete",
                vec![count_of(
                    delivered.len().to_string(),
                    "Fix complete",
                    keys_of(&delivered),
                )],
                fix_note,
                None,
            ),
            tile("Dated", vec![words(dated.to_string())], dated_note, None),
        ];
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
        let view = View {
            id,
            label: id.label().into(),
            kind: ViewKind::Customers,
            verdict: date_verdict.max(idle.verdict),
            summary,
            kpis,
            tracks: Vec::new(),
            undated,
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

    /// `t` is part of `version`'s work: every open item is, unless a scope
    /// filter lists the version's open work.
    fn in_scope(&self, project: &str, version: &str, t: &Ticket) -> bool {
        project != self.cfg.ex_project
            || self
                .raw
                .scopes
                .get(version)
                .is_none_or(|s| s.open.contains(&t.key))
    }

    /// What `version`'s scope filter counts, for its view's notes; `None`
    /// when it has none.
    fn scope_note(&self, spec: &TrackSpec<'_>, version: &str) -> Option<String> {
        let scope = self
            .raw
            .scopes
            .get(version)
            .filter(|_| spec.project == self.cfg.ex_project)?;
        let in_version: BTreeSet<&IssueKey> = spec
            .open
            .iter()
            .filter(|t| t.fix_versions.iter().any(|f| f == version))
            .map(|t| &t.key)
            .collect();
        let kept = in_version.iter().filter(|k| scope.open.contains(k)).count();
        let mut note = format!(
            "{version} is the work filter {} lists: {}, and its closures for the closing rate. {} in {version} not counted.",
            scope.filter,
            counted(kept, "open item"),
            counted(in_version.len() - kept, "other open item"),
        );
        let outside = scope.open.len() - kept;
        if outside > 0 {
            note.push_str(&format!(
                " The filter also lists {} without {version}, not counted either.",
                counted(outside, "open item")
            ));
        }
        Some(note)
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
            .filter(|t| {
                t.fix_versions
                    .iter()
                    .any(|f| spec.versions.contains(f) && self.in_scope(spec.project, f, t))
            })
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
                agreed_date: self.cfg.release_dates.get(spec.date_key).copied(),
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
        let scope_notes = spec
            .versions
            .iter()
            .filter_map(|v| self.scope_note(spec, v));
        Judged {
            track,
            items,
            notes: pm_note.into_iter().chain(scope_notes).collect(),
            date_key: spec.date_key,
        }
    }

    /// An EX release to judge.
    fn ex_spec<'s>(
        &self,
        name: &'s str,
        versions: &'s [String],
        open: &'s [Ticket],
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
            open,
            closed: &self.raw.ex_closed,
            page,
            label,
            date_key: name,
        }
    }

    /// The Development views, and the dates of the releases judged there.
    fn release_views(&self) -> (Vec<View>, ReleaseDates) {
        let (cfg, raw) = (self.cfg, self.raw);
        let pages = &cfg.pm_pages;
        let mut releases = ReleaseDates::new();

        let tracked = fetch::tracked_versions(cfg, &raw.ex_versions);
        let exa6: Vec<String> = fetch::release_versions(cfg, &tracked)
            .into_iter()
            .find(|(view, _)| *view == ViewId::Exa6)
            .map(|(_, names)| names)
            .unwrap_or_default();
        let exa6_open = self.items(&raw.ex_open, ViewId::Exa6);
        let mut exa6_tracks = Vec::new();
        let mut exa6_items = Vec::new();
        let mut exa6_notes = self.notes(ViewId::Exa6);
        for name in &exa6 {
            let page = (*name == pages.exa6_release).then_some(pages.exa6);
            let spec = self.ex_spec(name, std::slice::from_ref(name), &exa6_open, page, None);
            let judged = self.release_track(&spec);
            keep_date(&mut releases, &judged);
            exa6_tracks.push(judged.track);
            exa6_items.extend(judged.items);
            exa6_notes.extend(judged.notes);
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
        let exa6_items = unique(exa6_items);
        exa6_notes.extend(self.patch_notes(ViewId::Exa6, &exa6_items, "item"));
        let mut exa6_view = self.release_view(
            ViewId::Exa6,
            exa6_tracks,
            &exa6_items,
            self.window(ViewId::Exa6, &raw.ex_closed, &exa6),
        );
        exa6_view.health = self.health(true);
        exa6_view.notes = exa6_notes;

        let beta = std::slice::from_ref(&cfg.exa7_beta_version);
        let beta_open = self.items(&raw.ex_open, ViewId::Exa7Beta);
        let judged = self.release_track(&TrackSpec {
            date_key: ViewId::Exa7Beta.label(),
            ..self.ex_spec(
                &cfg.exa7_beta_version,
                beta,
                &beta_open,
                Some(pages.exa7),
                Some(&pages.exa7_beta_label),
            )
        });
        keep_date(&mut releases, &judged);
        let mut beta_view = self.release_view(
            ViewId::Exa7Beta,
            vec![judged.track],
            &judged.items,
            self.window(ViewId::Exa7Beta, &raw.ex_closed, beta),
        );
        beta_view.health = self.health(false);
        beta_view.notes = self.notes(ViewId::Exa7Beta);
        beta_view.notes.extend(judged.notes);
        beta_view
            .notes
            .extend(self.patch_notes(ViewId::Exa7Beta, &judged.items, "item"));

        let ga = &cfg.exa7_ga_versions;
        let ga_name = format!("EXA7 GA ({})", ga.join(" + "));
        let ga_open = self.items(&raw.ex_open, ViewId::Exa7Ga);
        let judged = self.release_track(&TrackSpec {
            date_key: ViewId::Exa7Ga.label(),
            ..self.ex_spec(
                &ga_name,
                ga,
                &ga_open,
                Some(pages.exa7),
                Some(&pages.exa7_ga_label),
            )
        });
        keep_date(&mut releases, &judged);
        let mut ga_view = self.release_view(
            ViewId::Exa7Ga,
            vec![judged.track],
            &judged.items,
            self.window(ViewId::Exa7Ga, &raw.ex_closed, ga),
        );
        ga_view.health = self.health(false);
        ga_view.notes = self.notes(ViewId::Exa7Ga);
        ga_view.notes.extend(judged.notes);
        ga_view
            .notes
            .extend(self.patch_notes(ViewId::Exa7Ga, &judged.items, "item"));

        let phases: Vec<String> = fetch::phases(cfg, &raw.sd_versions)
            .iter()
            .map(|v| v.name.clone())
            .collect();
        let sd_open = self.items(&raw.sd_open, ViewId::ExaSd);
        let mut sd_notes = self.notes(ViewId::ExaSd);
        let mut sd_tracks: Vec<Track> = Vec::new();
        for name in &phases {
            let page = (*name == cfg.exa_sd_pm_phase).then_some(pages.exa_sd);
            let spec = TrackSpec {
                name,
                project: &cfg.exa_sd_project,
                versions: std::slice::from_ref(name),
                open: &sd_open,
                closed: &raw.sd_closed,
                page,
                label: None,
                date_key: name,
            };
            let judged = self.release_track(&spec);
            keep_date(&mut releases, &judged);
            sd_tracks.push(judged.track);
            sd_notes.extend(judged.notes);
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
        let tests = sd_open.iter().filter(|t| rules::is_test_case(t)).count();
        let mut sd_view = self.release_view(
            ViewId::ExaSd,
            sd_tracks,
            &sd_open.iter().collect::<Vec<_>>(),
            self.window(ViewId::ExaSd, &raw.sd_closed, &phases),
        );
        // Open scope counts the open EXA SD items other than Test items and
        // says how many those are; closures and additions count only items
        // in the phases.
        if let Some(open) = sd_view.kpis.first_mut() {
            let work: Vec<&Ticket> = sd_open.iter().filter(|t| !rules::is_test_case(t)).collect();
            *open = tile(
                "Open scope",
                vec![count_of(
                    work.len().to_string(),
                    "Open scope",
                    keys_of(&work),
                )],
                vec![words(if tests == 1 {
                    "1 is a Test item".to_string()
                } else {
                    format!("{tests} are Test items")
                })],
                None,
            );
        }
        if let Some(window) = sd_view.kpis.last_mut() {
            window.note = "closed / added in the phases (new or moved in)".into();
        }
        sd_view.notes = sd_notes;
        let views = [exa6_view, beta_view, ga_view, sd_view]
            .into_iter()
            .map(|v| self.with_gaps(v))
            .collect();
        (views, releases)
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
        let complete = subset(items, rules::is_complete);
        // A patch in review puts an item in review, whatever its Jira status.
        let review = subset(items, |t| {
            !rules::is_complete(t) && (t.delivery.is_some() || cfg.in_review(&t.status))
        });
        let tests = subset(items, rules::is_test_case).len();
        // The risk numbers leave out test cases and complete items, and say so.
        let work = subset(items, |t| !rules::is_test_case(t) && !rules::is_complete(t));
        let complete_work = subset(&complete, |t| !rules::is_test_case(t));
        let left_out = |what: &str| {
            count_of(
                format!("{} complete", complete_work.len()),
                &format!("Complete, not counted in {what}"),
                keys_of(&complete_work),
            )
        };
        let uncounted = |what: &str| -> Vec<TilePart> {
            match (tests, complete_work.is_empty()) {
                (0, true) => Vec::new(),
                (n, true) => vec![words(format!(", {n} test cases not counted"))],
                (0, false) => vec![words(", "), left_out(what), words(" not counted")],
                (n, false) => vec![
                    words(format!(", {n} test cases and ")),
                    left_out(what),
                    words(" not counted"),
                ],
            }
        };
        let blocker_priority = subset(&work, |t| rules::has_priority(t, BLOCKER));
        let blocked = subset(&work, rules::is_blocked_status);
        let critical = subset(&work, |t| rules::has_priority(t, CRITICAL));
        let s1 = subset(&work, |t| t.severity_level() == Some(1));
        let s2 = subset(&work, |t| t.severity_level() == Some(2));
        let with_due = subset(&work, |t| t.due.is_some());
        let past_due = subset(&work, |t| t.due.is_some_and(|d| d < today));
        let review_part = count_of(
            format!("{} in review or verification", review.len()),
            "In review or verification",
            keys_of(&review),
        );
        let open_note = if complete.is_empty() {
            vec![review_part]
        } else {
            vec![
                count_of(
                    format!("{} complete", complete.len()),
                    "Complete",
                    keys_of(&complete),
                ),
                words(", "),
                review_part,
            ]
        };
        let last_days = vec![
            linked(
                format!("-{}", window.closed.len()),
                self.jira_search(&window.closed),
            ),
            words(" / "),
            window.added.as_ref().map_or_else(
                || words("+?"),
                |keys| linked(format!("+{}", keys.len()), self.jira_search(keys)),
            ),
        ];
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
        let mut sorted: Vec<Ticket> = items.iter().map(|&t| t.clone()).collect();
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
                tile(
                    "Open scope",
                    vec![count_of(
                        items.len().to_string(),
                        "Open scope",
                        keys_of(items),
                    )],
                    open_note,
                    None,
                ),
                tile(
                    "Blocker priority",
                    vec![count_of(
                        blocker_priority.len().to_string(),
                        "Blocker priority",
                        keys_of(&blocker_priority),
                    )],
                    [
                        vec![
                            words("+"),
                            count_of(
                                format!("{} blocked", blocked.len()),
                                "Blocked",
                                keys_of(&blocked),
                            ),
                        ],
                        uncounted("Blocker priority"),
                    ]
                    .concat(),
                    (!blocker_priority.is_empty()).then_some(Tone::Danger),
                ),
                tile(
                    "Critical",
                    vec![count_of(
                        critical.len().to_string(),
                        "Critical",
                        keys_of(&critical),
                    )],
                    [
                        vec![
                            count_of(format!("S1 {}", s1.len()), "S1", keys_of(&s1)),
                            words(", "),
                            count_of(format!("S2 {}", s2.len()), "S2", keys_of(&s2)),
                        ],
                        uncounted("Critical"),
                    ]
                    .concat(),
                    None,
                ),
                tile(
                    "Past due date",
                    vec![count_of(
                        past_due.len().to_string(),
                        "Past due date",
                        keys_of(&past_due),
                    )],
                    [
                        vec![
                            words("of "),
                            count_of(
                                format!("{} with a date", with_due.len()),
                                "With a due date",
                                keys_of(&with_due),
                            ),
                        ],
                        uncounted("Past due date"),
                    ]
                    .concat(),
                    None,
                ),
                Kpi {
                    value_parts: last_days.clone(),
                    ..kpi(
                        &format!("Last {} days", cfg.closed_window_days),
                        text_of(&last_days),
                        "closed / added (new or moved in)".into(),
                        None,
                    )
                },
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
                agreed_date: self.cfg.release_dates.get(&data.config.name).copied(),
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
            .map(|t| self.item(t.clone(), ViewId::Future))
            .collect();
        let verdict = cards
            .iter()
            .map(|c| c.track.verdict)
            .max()
            .unwrap_or(Verdict::NoDate);
        let refs: Vec<&Ticket> = items.iter().collect();
        let behind: Vec<&InitiativeCard> = cards
            .iter()
            .filter(|c| c.track.verdict >= Verdict::AtRisk)
            .collect();
        let behind_keys: BTreeSet<IssueKey> = behind
            .iter()
            .flat_map(|c| c.track.keys.iter().cloned())
            .collect();
        // A ticket blocking two initiatives counts once.
        let blockers: BTreeSet<IssueKey> = cards
            .iter()
            .flat_map(|c| c.blockers.iter().map(|b| b.key.clone()))
            .collect();
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
                tile(
                    "Initiatives",
                    vec![count_of(
                        cards.len().to_string(),
                        "Initiatives",
                        keys_of(&refs),
                    )],
                    vec![count_of(
                        format!("{} at risk or off track", behind.len()),
                        "Initiatives at risk or off track",
                        behind_keys.into_iter().collect(),
                    )],
                    None,
                ),
                tile(
                    "Open items",
                    vec![count_of(
                        items.len().to_string(),
                        "Open items",
                        keys_of(&refs),
                    )],
                    vec![words("across every initiative")],
                    None,
                ),
                tile(
                    "Blockers",
                    vec![count_of(
                        blockers.len().to_string(),
                        "Blockers",
                        blockers.iter().cloned().collect(),
                    )],
                    vec![words("test cases not counted")],
                    (!blockers.is_empty()).then_some(Tone::Danger),
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
fn keys_in(map: &KeysByVersion, names: &[String]) -> Vec<IssueKey> {
    names
        .iter()
        .flat_map(|n| map.get(n).into_iter().flatten())
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// How many distinct keys `names` hold in a per-fixVersion key map.
fn count_in(map: &KeysByVersion, names: &[String]) -> usize {
    keys_in(map, names).len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commitments::Commitment;
    use crate::config::InitiativeConfig;
    use crate::confluence::LabelledDate;
    use crate::fetch::{Note, Scope};
    use crate::model::{
        ChangeStatus, DateSource, HealthPart, Instance, LinkRef, PatchRef, PmRef, TargetSource,
    };
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
            scopes: BTreeMap::new(),
            sd_closed: keys(&[("NVMesh-P2", &["EXM-90", "EXM-91"])]),
            added: BTreeMap::from([
                (
                    EXA6,
                    Some(vec!["EX-2".parse().unwrap(), "EX-80".parse().unwrap()]),
                ),
                (EXA7_BETA, Some(vec![])),
                (EXA7_GA, Some(vec![])),
                (EXA_SD, None),
            ]),
            jira_cloud: Url::parse("https://jira.example/").unwrap(),
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
        snapshot(
            &Config::default(),
            raw,
            GerritState::Read(&Patches::default()),
            timing(),
        )
        .unwrap()
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
        assert!(!complete(
            &Config::default(),
            &raw,
            GerritState::Read(&Patches::default())
        ));
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

        let snap = snapshot(
            &cfg,
            &raw(),
            GerritState::Read(&Patches::default()),
            timing(),
        )
        .unwrap();

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
    fn a_release_is_judged_against_its_agreed_date() {
        let mut cfg = Config::default();
        cfg.release_dates.insert("EXA7 Beta".into(), d(2026, 12, 1));
        let snap = snapshot(
            &cfg,
            &raw(),
            GerritState::Read(&Patches::default()),
            timing(),
        )
        .unwrap();
        let track = |id: ViewId, name: &str| {
            view(&snap, id)
                .tracks
                .iter()
                .find(|t| t.name == name)
                .map(|t| (t.ref_source, t.ref_date))
        };
        let agreed = |y, m, day| Some((Some(DateSource::Agreed), Some(d(y, m, day))));

        assert_eq!(track(EXA6, "ES6.3.10"), agreed(2027, 1, 27));
        assert_eq!(
            track(EXA7_GA, "EXA7 GA (ES7.0.0 + ES7.1.0)"),
            agreed(2027, 3, 31),
            "EXA7 GA is set by its view's name, whatever versions it covers"
        );
        assert_eq!(track(EXA7_BETA, "ES7.0.0_Beta"), agreed(2026, 12, 1));
        assert_eq!(
            track(EXA6, "ES6.3.9_C1"),
            Some((Some(DateSource::Jira), Some(d(2026, 11, 6)))),
            "a release without one keeps the PM page or Jira date"
        );
        // A Future work initiative's date is kept under its name.
        cfg.release_dates
            .insert("New HA (EAM)".into(), d(2027, 3, 31));
        let snap = snapshot(
            &cfg,
            &raw(),
            GerritState::Read(&Patches::default()),
            timing(),
        )
        .unwrap();
        assert_eq!(
            view(&snap, FUTURE)
                .initiatives
                .iter()
                .find(|c| c.track.name == "New HA (EAM)")
                .map(|c| (c.track.ref_source, c.track.ref_date)),
            agreed(2027, 3, 31)
        );
    }

    #[test]
    fn a_scope_filter_decides_a_releases_open_work() {
        let open = |snap: &Snapshot, name: &str| {
            view(snap, EXA6)
                .tracks
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.open)
        };
        assert_eq!(
            open(&build(&raw()), "ES6.3.10"),
            Some(8),
            "without a filter, every open item"
        );
        let mut raw = raw();
        // Also in ES6.3.9_C1, so it stays in EXA6 through that release.
        if let Some(t) = raw.ex_open.iter_mut().find(|t| t.key.as_str() == "EX-3") {
            t.fix_versions.push("ES6.3.9_C1".into());
        }
        raw.scopes.insert(
            "ES6.3.10".into(),
            Scope {
                filter: 49208,
                open: ["EX-1", "EX-2", "EX-999"]
                    .iter()
                    .map(|k| k.parse().unwrap())
                    .collect(),
            },
        );

        let snap = build(&raw);
        let exa6 = view(&snap, EXA6);
        let keys: Vec<&str> = exa6.items.iter().map(|t| t.key.as_str()).collect();

        assert_eq!(
            (open(&snap, "ES6.3.10"), open(&snap, "ES6.3.9_C1")),
            (Some(2), Some(2))
        );
        assert_eq!(keys, ["EX-1", "EX-2", "EX-3", "EX-50"]);
        assert!(
            exa6.notes.iter().any(|n| n
                == "ES6.3.10 is the work filter 49208 lists: 2 open items, and its closures for the closing rate. \
                    6 other open items in ES6.3.10 not counted. \
                    The filter also lists 1 open item without ES6.3.10, not counted either."),
            "{:?}",
            exa6.notes
        );
        assert_eq!(
            view(&snap, EXA7_BETA).items.len(),
            1,
            "other releases keep every item"
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
        let unread = snapshot(&Config::default(), &raw, GerritState::NotRead, timing()).unwrap();
        assert_eq!(
            gerrit_error(&unread),
            Some(("Gerrit".into(), Some("not read yet".into())))
        );
    }

    #[test]
    fn unread_patches_cap_the_views_they_decide() {
        let raw = raw();
        let partial = Patches {
            map: HashMap::new(),
            errors: vec!["change search: timed out".into()],
        };
        let deciding = vec![ALL, OTHERS, GCP, NVIDIA, EXA6, EXA7_BETA, EXA7_GA];

        for gerrit in [
            GerritState::NotRead,
            GerritState::Failed("no credentials"),
            GerritState::Read(&partial),
        ] {
            let snap = snapshot(&Config::default(), &raw, gerrit, timing()).unwrap();

            assert_eq!(capped(&snap), deciding, "{gerrit:?}");
            assert!(!complete(&Config::default(), &raw, gerrit), "{gerrit:?}");
        }
        assert!(
            view(&build(&raw), EXA6)
                .notes
                .iter()
                .all(|n| !n.starts_with("Gerrit not fully read")),
            "a full read leaves no note"
        );
        let unread = snapshot(&Config::default(), &raw, GerritState::NotRead, timing()).unwrap();
        assert!(view(&unread, GCP).notes.contains(
            &"Gerrit not fully read (not read yet): where patches are missing, Jira decides delivery."
                .to_string()
        ));
        // Without a branch that decides delivery, Gerrit decides no verdict.
        let mut cfg = Config::default();
        for f in &mut cfg.tag_families {
            f.views.clear();
        }
        let snap = snapshot(&cfg, &raw, GerritState::NotRead, timing()).unwrap();
        assert_eq!(capped(&snap), Vec::<ViewId>::new());
        assert!(complete(&cfg, &raw, GerritState::NotRead));
    }

    #[test]
    fn the_watch_list_covers_every_row_once() {
        let cfg = Config::default();
        let mut raw = raw();
        if let Some(t) = raw.ex_open.iter_mut().find(|t| t.key.as_str() == "EX-1") {
            t.links = vec![link("LU-18086", "relates to", false)];
        }
        let snap = build(&raw);

        let watched = watch_list(&cfg, &snap);
        let mut keys: Vec<String> = watched.iter().map(|w| w.key.to_string()).collect();
        keys.sort();
        let mut rows: Vec<String> = [
            "DDN-3571", "DDN-6000", "DDN-6001", "DDN-6741", "DDN-6900", "DDN-7162", "GCP-315",
            "NCP-163", "EX-1", "EX-50", "EX-60", "EX-70", "EXM-1", "EXM-9", "EXM-500",
        ]
        .map(String::from)
        .into();
        rows.extend((2..=cfg.small_release_max + 3).map(|i| format!("EX-{i}")));
        rows.sort();
        let search = |key: &str| {
            watched
                .iter()
                .find(|w| w.key.as_str() == key)
                .map(|w| w.search.iter().map(ToString::to_string).collect::<Vec<_>>())
        };

        assert_eq!(keys, rows, "every row of every view, test items too, once");
        assert_eq!(
            search("DDN-7162"),
            Some(vec!["DDN-7162".into(), "EX-100".into()])
        );
        assert_eq!(
            search("NCP-163"),
            Some(vec!["NCP-163".into(), "NP-131".into()])
        );
        assert_eq!(
            search("DDN-6900"),
            Some(vec!["DDN-6900".into()]),
            "an EXR key never names a Lustre change"
        );
        assert_eq!(
            search("EX-1"),
            Some(vec!["EX-1".into()]),
            "a release item's patches name it, not its linked LU ticket"
        );
        assert_eq!(
            search("DDN-6741"),
            Some(vec!["DDN-6741".into()]),
            "an escalation record is no fix"
        );
    }

    fn patch(number: u64, branch: &str, status: ChangeStatus, tag: Option<&str>) -> PatchRef {
        PatchRef {
            number,
            url: Url::parse(&format!(
                "https://review.whamcloud.com/c/ex/lustre-release/+/{number}"
            ))
            .unwrap(),
            branch: branch.into(),
            status,
            first_tag: tag.map(String::from),
        }
    }

    #[test]
    fn a_ticket_in_review_on_both_branches_waits_for_the_later_release() {
        let mut raw = raw();
        let mut both = idle(ticket("GCP-316", "Client evictions under load"), 1);
        (both.instance, both.source) = (Instance::Whamcloud, Some("Whamcloud".into()));
        raw.customers.push(both);
        let patches = Patches {
            map: HashMap::from([(
                "GCP-316".parse().unwrap(),
                vec![
                    patch(1, "b_es6_0", ChangeStatus::New, None),
                    patch(2, "b_es7_0", ChangeStatus::New, None),
                ],
            )]),
            errors: vec![],
        };
        let release = |cfg: &Config| {
            let snap = snapshot(cfg, &raw, GerritState::Read(&patches), timing()).unwrap();
            let d = item(&snap, GCP, "GCP-316").delivery.clone().unwrap();
            (d.release, d.date, view(&snap, GCP).notes.clone())
        };

        // ES6.3.10 (27 Jan 2027, agreed) comes after EXA7 Beta (30 Nov 2026).
        let (name, date, notes) = release(&Config::default());
        assert_eq!(
            (name.as_deref(), date),
            (Some("ES6.3.10"), Some(d(2027, 1, 27)))
        );
        assert!(notes.iter().all(|n| !n.contains("has no date")));

        // A release without a date counts as the later one, and the view says so.
        let mut cfg = Config::default();
        if let Some(es7) = cfg.tag_families.iter_mut().find(|f| f.branch == "b_es7_0") {
            es7.release = "EXA7 Delta".into();
        }
        let (name, date, notes) = release(&cfg);
        assert_eq!((name.as_deref(), date), (Some("EXA7 Delta"), None));
        assert!(notes.contains(
            &"EXA7 Delta has no date, so patches in review on b_es7_0 give none.".to_string()
        ));
    }

    #[test]
    fn release_and_future_tile_counts_name_their_tickets() {
        let cfg = Config::default();
        let snap = build(&raw());
        let parts = |id: ViewId, label: &str| {
            let k = view(&snap, id)
                .kpis
                .iter()
                .find(|k| k.label == label)
                .unwrap();
            k.value_parts
                .iter()
                .chain(&k.note_parts)
                .filter(|p| p.filter.is_some() || p.url.is_some())
                .map(|p| {
                    let keys: Vec<&str> = p.keys.iter().map(IssueKey::as_str).collect();
                    let url = p.url.as_ref().map(ToString::to_string).unwrap_or_default();
                    format!(
                        "{} | {} | {} | {url}",
                        p.text,
                        p.filter.as_deref().unwrap_or(""),
                        keys.join(" ")
                    )
                })
                .collect::<Vec<_>>()
        };
        let in_review: Vec<String> = (2..=cfg.small_release_max + 3)
            .map(|i| format!("EX-{i}"))
            .collect();

        assert_eq!(
            parts(EXA6, "Open scope"),
            [
                format!("9 | Open scope | EX-50 {} EX-1 | ", in_review.join(" ")),
                format!(
                    "{} in review or verification | In review or verification | {} | ",
                    in_review.len(),
                    in_review.join(" ")
                ),
            ]
        );
        assert_eq!(
            parts(EXA6, "Blocker priority"),
            [
                "1 | Blocker priority | EX-1 | ",
                "0 blocked | Blocked |  | "
            ]
        );
        // Closed and added items are not in the tables: they link to Jira.
        assert_eq!(
            parts(EXA6, "Last 28 days"),
            [
                "-4 |  |  | https://jira.example/issues/?jql=key+in+%28EX-90%2C+EX-91%2C+EX-92%2C+EX-93%29+ORDER+BY+key",
                "+2 |  |  | https://jira.example/issues/?jql=key+in+%28EX-2%2C+EX-80%29+ORDER+BY+key",
            ]
        );
        assert_eq!(
            view(&snap, EXA6)
                .kpis
                .last()
                .map(|k| (k.value.as_str(), k.note.as_str())),
            Some(("-4 / +2", "closed / added (new or moved in)")),
            "the tile reads as before"
        );
        assert_eq!(
            parts(EXA7_BETA, "Last 28 days"),
            ["-1 |  |  | https://jira.example/issues/?jql=key+in+%28EX-94%29+ORDER+BY+key"],
            "+0 links nowhere"
        );
        assert_eq!(
            parts(EXA_SD, "Open scope"),
            ["1 | Open scope | EXM-1 | "],
            "EXA SD counts its items other than Test items"
        );
        assert_eq!(parts(FUTURE, "Open items"), ["1 | Open items | EXM-500 | "]);
    }

    #[test]
    fn each_release_lists_the_open_items_it_counts() {
        let cfg = Config::default();
        let snap = build(&raw());
        let keys = |id: ViewId, name: &str| {
            view(&snap, id)
                .tracks
                .iter()
                .find(|t| t.name == name)
                .map(|t| t.keys.iter().map(ToString::to_string).collect::<Vec<_>>())
        };
        let mut es6310: Vec<String> = (2..=cfg.small_release_max + 3)
            .map(|i| format!("EX-{i}"))
            .collect();
        es6310.push("EX-1".into());

        assert_eq!(keys(EXA6, "ES6.3.9_C1"), Some(vec!["EX-50".to_string()]));
        assert_eq!(keys(EXA6, "ES6.3.10"), Some(es6310));
        assert_eq!(
            keys(EXA_SD, "NVMesh-P2"),
            Some(vec!["EXM-1".to_string(), "EXM-9".to_string()]),
            "Test items too"
        );
    }

    #[test]
    fn tile_counts_name_the_tickets_they_count() {
        let mut raw = raw();
        for t in &mut raw.customers {
            match t.key.as_str() {
                "DDN-6741" => t.idle_days = Some(10),
                "DDN-6900" => t.idle_days = None,
                _ => {}
            }
        }
        let snap = build(&raw);
        let parts = |label: &str| {
            let k = view(&snap, OTHERS)
                .kpis
                .iter()
                .find(|k| k.label == label)
                .unwrap();
            k.value_parts
                .iter()
                .chain(&k.note_parts)
                .map(|p| {
                    let keys: Vec<&str> = p.keys.iter().map(IssueKey::as_str).collect();
                    (p.text.clone(), p.filter.clone(), keys.join(" "))
                })
                .collect::<Vec<_>>()
        };
        let count = |text: &str, filter: &str, keys: &str| {
            (text.to_string(), Some(filter.to_string()), keys.to_string())
        };
        let words = |text: &str| (text.to_string(), None, String::new());

        // DDN-3571 is the one S1 ticket, idle 1041 days.
        assert_eq!(
            parts("S1 / S2"),
            [
                count("1", "S1", "DDN-3571"),
                words(" / "),
                count("0", "S2", ""),
                count(
                    "1 past the idle limit",
                    "S1/S2 past the idle limit",
                    "DDN-3571"
                ),
            ]
        );
        assert_eq!(
            parts("Idle over 7 days"),
            [
                count("2", "Idle over 7 days", "DDN-3571 DDN-6741"),
                count("1 over 30 days", "Idle over 30 days", "DDN-3571"),
                words(", "),
                count("1 unknown", "Idle time unknown", "DDN-6900"),
            ]
        );
        assert_eq!(
            parts("Fix complete"),
            [
                count("1", "Fix complete", "DDN-6000"),
                words("patches merged or every linked fix done"),
            ]
        );
        assert_eq!(
            parts("Dated"),
            [
                words("0"),
                words("0 committed, "),
                count(
                    "5 have no date",
                    "No date",
                    "DDN-3571 DDN-6000 DDN-6001 DDN-6741 DDN-6900"
                ),
            ]
        );
        let dated = view(&snap, OTHERS)
            .kpis
            .iter()
            .find(|k| k.label == "Dated")
            .unwrap();
        assert_eq!(
            (dated.value.as_str(), dated.note.as_str()),
            ("0", "0 committed, 5 have no date"),
            "the tile reads as the pieces do"
        );
    }

    #[test]
    fn complete_tickets_count_as_complete_not_as_stalled() {
        let cfg = Config::default();
        let raw = raw();
        // Merged, not in a tag yet: due in the next one.
        let merged = patch(1, "b_es6_0", ChangeStatus::Merged, None);
        let tagged = patch(2, "b_es6_0", ChangeStatus::Merged, Some("2.14.0-ddn262"));
        let in_review = patch(3, "b_es6_0", ChangeStatus::New, None);
        let patches = Patches {
            map: HashMap::from([
                ("DDN-3571".parse().unwrap(), vec![merged.clone()]),
                ("DDN-6000".parse().unwrap(), vec![tagged.clone()]),
                ("EX-1".parse().unwrap(), vec![merged.clone()]),
                ("EX-2".parse().unwrap(), vec![merged]),
                ("EX-3".parse().unwrap(), vec![tagged]),
                ("EX-50".parse().unwrap(), vec![in_review]),
            ]),
            errors: vec![],
        };
        let with = snapshot(&cfg, &raw, GerritState::Read(&patches), timing()).unwrap();
        let without = build(&raw);
        let tile = |snap: &Snapshot, id: ViewId, label: &str| {
            view(snap, id)
                .kpis
                .iter()
                .find(|k| k.label == label)
                .map(|k| (k.value.clone(), k.note.clone()))
        };
        let blocks = |snap: &Snapshot, id: ViewId, key: &str| {
            view(snap, id)
                .blockers
                .iter()
                .any(|b| b.key.as_str() == key)
        };

        // DDN-3571, S1 and idle 1041 days, put All others off track; complete,
        // it waits for its tag and needs no date.
        assert_eq!(
            (view(&without, OTHERS).verdict, view(&with, OTHERS).verdict),
            (Verdict::OffTrack, Verdict::NoDate)
        );
        assert!(blocks(&without, OTHERS, "DDN-3571") && !blocks(&with, OTHERS, "DDN-3571"));
        assert_eq!(
            tile(&with, OTHERS, "S1 / S2"),
            Some(("1 / 0".into(), "0 past the idle limit".into()))
        );
        assert_eq!(
            tile(&with, OTHERS, "Idle over 7 days").map(|(v, _)| v),
            Some("0".into())
        );
        assert_eq!(
            tile(&with, OTHERS, "Fix complete"),
            Some((
                "2".into(),
                "patches merged or every linked fix done; 1 due in the next tag".into()
            ))
        );
        assert_eq!(
            tile(&with, NVIDIA, "Fix complete"),
            Some(("0".into(), "patches merged or every linked fix done".into()))
        );
        assert_eq!(
            tile(&with, OTHERS, "Dated").map(|(_, note)| note),
            Some("0 committed, 3 have no date, 2 complete".into())
        );
        assert_eq!(view(&with, OTHERS).undated, 3);
        // EX-1, a Blocker in EXA6, leaves the blockers and the risk tiles.
        // EX-2 and EX-3 (In Review) count as complete in the open scope, and
        // EX-50 (To Do) as in review, by its patch.
        assert!(blocks(&without, EXA6, "EX-1") && !blocks(&with, EXA6, "EX-1"));
        assert_eq!(
            tile(&with, EXA6, "Blocker priority"),
            Some(("0".into(), "+0 blocked, 3 complete not counted".into()))
        );
        let left_out = view(&with, EXA6)
            .kpis
            .iter()
            .find(|k| k.label == "Blocker priority")
            .and_then(|k| k.note_parts.iter().find(|p| p.text == "3 complete"))
            .map(|p| {
                (
                    p.filter.clone(),
                    p.keys.iter().map(ToString::to_string).collect::<Vec<_>>(),
                )
            });
        assert_eq!(
            left_out,
            Some((
                Some("Complete, not counted in Blocker priority".to_string()),
                vec!["EX-2".to_string(), "EX-3".to_string(), "EX-1".to_string()]
            )),
            "what a tile leaves out can be shown too"
        );
        assert_eq!(
            tile(&with, EXA6, "Open scope").map(|(_, note)| note),
            Some(format!(
                "3 complete, {} in review or verification",
                cfg.small_release_max + 1
            ))
        );
    }

    #[test]
    fn patches_stand_in_for_jira_where_their_branch_decides() {
        let raw = raw();
        let merged = patch(1, "b_es6_0", ChangeStatus::Merged, Some("2.14.0-ddn262"));
        let es6_review = patch(2, "b_es6_0", ChangeStatus::New, None);
        let es7_review = patch(3, "b_es7_0", ChangeStatus::New, None);
        let patches = Patches {
            map: HashMap::from([
                ("DDN-7162".parse().unwrap(), vec![merged.clone()]),
                ("DDN-6000".parse().unwrap(), vec![es6_review]),
                ("NCP-163".parse().unwrap(), vec![es7_review.clone()]),
                (
                    "GCP-315".parse().unwrap(),
                    vec![merged.clone(), es7_review.clone()],
                ),
                ("EX-2".parse().unwrap(), vec![merged]),
                (
                    "EX-60".parse().unwrap(),
                    vec![patch(4, "b_es6_0", ChangeStatus::Merged, None)],
                ),
            ]),
            errors: vec![],
        };
        let cfg = Config::default();
        let with = snapshot(&cfg, &raw, GerritState::Read(&patches), timing()).unwrap();
        let without = build(&raw);
        let delivery = |id: ViewId, key: &str| item(&with, id, key).delivery.clone();
        let target = |id: ViewId, key: &str| rules::target(item(&with, id, key));

        // NVIDIA runs EXA6: the merged es6 patch delivers the ticket, whatever
        // its linked fix (EX-100, In Review) says.
        let nvidia = item(&with, NVIDIA, "DDN-7162");
        assert_eq!(
            (
                nvidia.fix_state,
                nvidia.fix.as_ref(),
                target(NVIDIA, "DDN-7162")
            ),
            (FixState::Delivered, None, None)
        );
        assert_eq!(
            nvidia
                .delivery
                .as_ref()
                .map(|d| (d.merged, d.branches[0].tag.as_deref())),
            Some((true, Some("2.14.0-ddn262")))
        );
        assert!(item(&without, NVIDIA, "DDN-7162").fix.is_some());
        // Its linked fix done, the ticket read as delivered; the es6 patch in
        // review says otherwise, and dates it by ES6.3.10's agreed date.
        assert_eq!(
            item(&without, OTHERS, "DDN-6000").fix_state,
            FixState::Delivered
        );
        assert_eq!(
            item(&with, OTHERS, "DDN-6000").fix_state,
            FixState::UnderWay
        );
        assert_eq!(
            target(OTHERS, "DDN-6000"),
            Some((d(2027, 1, 27), TargetSource::Patch))
        );
        assert_eq!(
            delivery(OTHERS, "DDN-6000").and_then(|d| d.release),
            Some("ES6.3.10".to_string())
        );
        // An es7 patch says nothing about an NVIDIA ticket.
        assert_eq!(delivery(NVIDIA, "NCP-163"), None);
        // GCP runs both: the es7 patch in review holds the ticket, dated by EXA7 Beta.
        let gcp = delivery(GCP, "GCP-315").unwrap();
        assert_eq!(
            (
                gcp.branches.len(),
                gcp.merged,
                gcp.release.as_deref(),
                gcp.date
            ),
            (2, false, Some("EXA7 Beta"), Some(d(2026, 11, 30)))
        );
        assert_eq!(delivery(ALL, "GCP-315"), Some(gcp));
        // Release items: a merged es6 patch counts EX-2 (In Review) as done in
        // EXA6; an es6 patch says nothing about an EXA7 Beta item.
        let weighted = |snap: &Snapshot| {
            view(snap, EXA6)
                .tracks
                .iter()
                .find(|t| t.name == "ES6.3.10")
                .map(|t| t.weighted)
        };
        assert_eq!(
            weighted(&with)
                .zip(weighted(&without))
                .map(|(w, wo)| ((wo - w) * 10.0).round()),
            Some(3.0)
        );
        assert_eq!(delivery(EXA7_BETA, "EX-60"), None);
        assert!(view(&with, EXA6).notes.contains(
            &"Patches on b_es6_0 decide the delivery of 1 item: 1 merged, 0 in review.".to_string()
        ));
        assert!(view(&with, ALL).notes.contains(
            &"Patches on b_es6_0 or b_es7_0 decide the delivery of 3 tickets: 1 merged, 2 in review."
                .to_string()
        ));
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
        // On a ticket without a dated fix, so the layout keeps both a fix and a delivery.
        let patches = Patches {
            map: HashMap::from([("GCP-315".parse().unwrap(), vec![patch])]),
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
