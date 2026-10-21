//! Files the dashboard writes: the snapshot, a one-line-per-refresh history,
//! the caches, the certificate, and the user's own files (commitments,
//! settings). Whole files are replaced through a uniquely named temporary file
//! that is synced and renamed, so readers never see a half-written file and
//! two writers never share one; the history is appended in place. New files
//! are readable by the owner only: they hold customer ticket data and keys.

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{File, TryLockError};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use tracing::warn;

use crate::model::{Comment, Snapshot};

/// The snapshot the UI renders.
pub(crate) const SNAPSHOT_FILE: &str = "latest.json";
/// One line per refresh with open and blocker counts per view, kept for
/// trend charts later; nothing reads it yet.
pub(crate) const HISTORY_FILE: &str = "history.jsonl";
/// Version of a history line; bumped when a count changes meaning. 3: GCP
/// and NVIDIA blockers include tickets rated S1/S2 from their priority, and
/// `partial` covers every failed source.
const HISTORY_FORMAT: u32 = 3;
/// Newest human comment per ticket.
pub(crate) const COMMENTS_FILE: &str = "comments.json";
/// First ddn tag per merged Gerrit revision.
pub(crate) const TAGS_FILE: &str = "tags.json";
/// What the last refresh worker wrote to stderr.
pub(crate) const REFRESH_LOG: &str = "refresh.log";
/// Held by the running refresh, so a second one (a manual `refresh-once`
/// next to the server's) stops instead of interleaving writes.
const REFRESH_LOCK: &str = "refresh.lock";
/// Permissions of every file created here.
pub(crate) const OWNER_ONLY: u32 = 0o600;

/// A cached newest human comment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct CachedComment {
    /// The ticket's `updated` stamp when read; any change reads it again.
    pub(crate) updated: String,
    /// The comment; `None` when no person ever commented.
    pub(crate) comment: Option<Comment>,
}

/// The comment cache file. The answers depend on which authors count as
/// bots, so the list is stored with them and a change starts a new cache.
#[derive(Serialize, Deserialize)]
struct CommentFile {
    bots: Vec<String>,
    entries: HashMap<String, CachedComment>,
}

/// The bot list as stored: lowercase and sorted, so order and case do not matter.
fn bot_key(bots: &[String]) -> Vec<String> {
    let mut key: Vec<String> = bots.iter().map(|b| b.to_lowercase()).collect();
    key.sort();
    key.dedup();
    key
}

/// Reads `path`, or `None` when it does not exist; other read errors fail.
pub(crate) fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Replaces `path` with `bytes` atomically.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = match path.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(dir) => dir,
        None => Path::new("."),
    };
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    tmp.write_all(bytes)
        .with_context(|| format!("writing {}", tmp.path().display()))?;
    tmp.as_file()
        .sync_all()
        .with_context(|| format!("syncing {}", tmp.path().display()))?;
    tmp.persist(path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// Writes `value` as JSON to `path` atomically.
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_atomic(path, &serde_json::to_vec(value)?)
}

/// One history line: per view, open items and blockers (cards included).
/// `partial` marks a refresh in which a source failed or, with `gaps`, data
/// a verdict depends on was missing.
pub(crate) fn history_line(snap: &Snapshot, gaps: bool) -> serde_json::Value {
    let views: BTreeMap<&str, (usize, usize)> = snap
        .tabs
        .iter()
        .flat_map(|t| &t.views)
        .map(|v| (v.id.as_str(), (v.items.len(), v.blocker_count())))
        .collect();
    let partial = gaps || snap.sources.iter().any(|s| s.error.is_some());
    serde_json::json!({
        "format": HISTORY_FORMAT,
        "generated_at": snap.generated_at,
        "partial": partial,
        "views": views,
    })
}

/// Appends a line to the history file.
pub(crate) fn append_history(path: &Path, line: &serde_json::Value) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(OWNER_ONLY)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("appending to {}", path.display()))
}

/// Reads a cache file. Caches only hold data the next refresh can read
/// again, so a missing or unreadable one starts empty; an unreadable one is logged.
fn load_json<T: DeserializeOwned + Default>(path: &Path) -> T {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            warn!("{} is unreadable and will be rebuilt: {e}", path.display());
            T::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(e) => {
            warn!(
                "{} could not be read and will be rebuilt: {e}",
                path.display()
            );
            T::default()
        }
    }
}

/// Writes a cache file; a failure is logged, since the next refresh rebuilds it.
fn save_json<T: Serialize>(path: &Path, value: &T) {
    if let Err(e) = write_json(path, value) {
        warn!("saving {}: {e:#}", path.display());
    }
}

/// The tag cache: first tag per merged revision.
pub(crate) fn load_tags(path: &Path) -> HashMap<String, String> {
    load_json(path)
}

/// Saves the tag cache.
pub(crate) fn save_tags(path: &Path, tags: &HashMap<String, String>) {
    save_json(path, tags);
}

/// The comment cache, or an empty one when it was built with other bots.
pub(crate) fn load_comments(path: &Path, bots: &[String]) -> HashMap<String, CachedComment> {
    match load_json::<Option<CommentFile>>(path) {
        Some(file) if file.bots == bot_key(bots) => file.entries,
        Some(_) => {
            warn!("the bot list changed; every newest comment will be read again");
            HashMap::new()
        }
        None => HashMap::new(),
    }
}

/// Saves the comment cache with the bot list it was built with.
pub(crate) fn save_comments(path: &Path, bots: &[String], entries: HashMap<String, CachedComment>) {
    save_json(
        path,
        &CommentFile {
            bots: bot_key(bots),
            entries,
        },
    );
}

/// Takes the refresh lock in `data_dir`; it is released when the file closes.
pub(crate) fn lock_refresh(data_dir: &Path) -> Result<File> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let path = data_dir.join(REFRESH_LOCK);
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(OWNER_ONLY)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => {
            bail!("another refresh is running (it holds {})", path.display())
        }
        Err(TryLockError::Error(e)) => {
            Err(e).with_context(|| format!("locking {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::os::unix::fs::PermissionsExt;

    fn bots(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    fn entry() -> CachedComment {
        CachedComment {
            updated: "u".into(),
            comment: Some(Comment {
                date: NaiveDate::from_ymd_opt(2026, 10, 7).unwrap(),
                author: Some("Jason".into()),
            }),
        }
    }

    fn snapshot(source_error: Option<&str>) -> Snapshot {
        use crate::model::{Tab, Verdict, View, ViewId, ViewKind};
        let view = View {
            id: ViewId::Gcp,
            label: "GCP".into(),
            kind: ViewKind::Customers,
            verdict: Verdict::OnTrack,
            summary: String::new(),
            kpis: vec![],
            tracks: vec![],
            date_groups: vec![],
            undated: 0,
            blockers: vec![],
            items: vec![],
            notes: vec![],
            pm: None,
            health: None,
            initiatives: vec![],
            editable_commitments: true,
        };
        Snapshot {
            format: crate::model::SNAPSHOT_FORMAT,
            generated_at: chrono::Local::now(),
            today: NaiveDate::from_ymd_opt(2026, 10, 21).unwrap(),
            duration_secs: 1.0,
            sources: vec![crate::model::SourceStatus {
                name: "Gerrit".into(),
                error: source_error.map(String::from),
            }],
            tabs: vec![Tab {
                id: "customers".into(),
                label: "Customers".into(),
                verdict: Verdict::OnTrack,
                views: vec![view],
            }],
        }
    }

    #[test]
    fn a_history_line_counts_per_view_and_marks_any_failure_partial() {
        let line = history_line(&snapshot(None), false);

        assert_eq!(line["format"], HISTORY_FORMAT);
        assert_eq!(line["partial"], false);
        assert_eq!(line["views"], serde_json::json!({"customers-gcp": [0, 0]}));
        assert_eq!(history_line(&snapshot(None), true)["partial"], true);
        assert_eq!(
            history_line(&snapshot(Some("timed out")), false)["partial"],
            true,
            "a failed source the verdicts do not use still marks the line"
        );
    }

    #[test]
    fn history_lines_are_appended_privately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HISTORY_FILE);

        append_history(&path, &serde_json::json!({"n": 1})).unwrap();
        append_history(&path, &serde_json::json!({"n": 2})).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"n\":1}\n{\"n\":2}\n"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn the_comment_cache_round_trips_and_starts_empty_when_missing_or_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        let gerrit = bots(&["Gerrit"]);

        assert!(load_comments(&path, &gerrit).is_empty());

        save_comments(
            &path,
            &gerrit,
            HashMap::from([("cloud:DDN-1".to_string(), entry())]),
        );

        assert_eq!(
            load_comments(&path, &gerrit).get("cloud:DDN-1"),
            Some(&entry())
        );

        std::fs::write(&path, "not json").unwrap();

        assert!(load_comments(&path, &gerrit).is_empty());
    }

    #[test]
    fn a_new_bot_list_drops_the_comment_cache_but_order_and_case_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.json");
        save_comments(
            &path,
            &bots(&["Gerrit", "Maloo"]),
            HashMap::from([("cloud:DDN-1".to_string(), entry())]),
        );

        assert_eq!(load_comments(&path, &bots(&["maloo", "GERRIT"])).len(), 1);
        assert!(load_comments(&path, &bots(&["Gerrit", "Maloo", "Jenkins"])).is_empty());
    }

    #[test]
    fn write_atomic_replaces_the_file_privately_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latest.json");

        write_json(&path, &serde_json::json!({"a": 1})).unwrap();
        write_json(&path, &serde_json::json!({"a": 2})).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":2}");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temporary file is left"
        );
    }

    #[test]
    fn read_optional_tells_missing_from_present() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("present.json");
        std::fs::write(&file, "{}").unwrap();

        assert_eq!(read_optional(&file).unwrap().as_deref(), Some("{}"));
        assert_eq!(
            read_optional(&dir.path().join("absent.json")).unwrap(),
            None
        );
    }

    #[test]
    fn a_second_refresh_cannot_take_the_lock() {
        let dir = tempfile::tempdir().unwrap();

        let held = lock_refresh(dir.path()).unwrap();
        let err = lock_refresh(dir.path()).unwrap_err();
        drop(held);

        assert!(
            err.to_string().starts_with("another refresh is running"),
            "{err}"
        );
        assert!(
            lock_refresh(dir.path()).is_ok(),
            "the lock is free once the holder closes it"
        );
    }
}
