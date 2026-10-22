//! Read-only Gerrit client: which EXAScaler Lustre changes mention a ticket,
//! and the first ddn tag that contains each merged one. Credentials come from
//! the gerrit-cli tool's `.env` file and only go into an HTTP header.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use url::Url;

use crate::http;
use crate::model::{ChangeStatus, IssueKey};

/// Gerrit prefixes JSON replies with this to defeat cross-site script inclusion.
const XSSI_PREFIX: &str = ")]}'";
/// Keys in the gerrit-cli `.env` file.
const URL_KEY: &str = "GERRIT_URL";
const USER_KEY: &str = "GERRIT_USER";
const PASSWORD_KEY: &str = "GERRIT_PASS";
/// Changes per search page; Gerrit flags `_more_changes` when there are more.
const PAGE_SIZE: usize = 100;

/// Client for review.whamcloud.com.
pub(crate) struct Gerrit {
    agent: ureq::Agent,
    base: Url,
    auth: String,
}

/// A change found by a search.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Change {
    /// Change number.
    pub(crate) number: u64,
    /// Target branch.
    pub(crate) branch: String,
    /// Review state.
    pub(crate) status: ChangeStatus,
    /// Current patch set's commit, when Gerrit returned it.
    pub(crate) revision: Option<String>,
    /// Ticket keys its commit message names.
    pub(crate) keys: BTreeSet<IssueKey>,
}

/// One page of search results.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ChangePage {
    /// The changes.
    pub(crate) changes: Vec<Change>,
    /// Gerrit has more results after these.
    pub(crate) more: bool,
}

impl Gerrit {
    /// Reads the address and credentials from the gerrit-cli `.env` file,
    /// with the same quoting rules as the tool's python-dotenv.
    pub(crate) fn from_env_file(path: &Path) -> Result<Self> {
        let shown = path.display();
        let mut env = HashMap::new();
        for item in dotenvy::from_path_iter(path)
            .map_err(|e| anyhow!("reading {shown}: {}", without_line(&e)))?
        {
            let (k, v) = item.map_err(|e| anyhow!("reading {shown}: {}", without_line(&e)))?;
            env.insert(k, v);
        }
        let value = |key: &str| env.get(key).ok_or_else(|| anyhow!("{shown} has no {key}"));
        let base = Url::parse(&format!("{}/", value(URL_KEY)?.trim_end_matches('/')))
            .with_context(|| format!("{URL_KEY} in {shown} is not a URL"))?;
        Ok(Self {
            agent: http::agent(),
            base,
            auth: http::basic_auth(value(USER_KEY)?, value(PASSWORD_KEY)?),
        })
    }

    fn get(&self, url: &Url) -> Result<Value> {
        let text = http::get_text(&self.agent, url, Some(&self.auth))?;
        serde_json::from_str(strip_xssi(&text))
            .with_context(|| format!("decoding the reply from {}", url.path()))
    }

    /// Changes on `branches` of `project` whose commit message names any of
    /// `keys`, across every result page.
    pub(crate) fn changes_mentioning(
        &self,
        keys: &[IssueKey],
        project: &str,
        branches: &[String],
    ) -> Result<Vec<Change>> {
        if keys.is_empty() || branches.is_empty() {
            return Ok(Vec::new());
        }
        let query = search_query(keys, project, branches);
        all_pages(|start| {
            let mut url = self
                .base
                .join("a/changes/")
                .context("building the change search URL")?;
            url.query_pairs_mut()
                .append_pair("q", &query)
                .append_pair("o", "CURRENT_REVISION")
                .append_pair("o", "CURRENT_COMMIT")
                .append_pair("n", &PAGE_SIZE.to_string())
                .append_pair("S", &start.to_string());
            parse_changes(&self.get(&url)?)
        })
    }

    /// Tags that contain `revision`.
    pub(crate) fn tags_containing(&self, project: &str, revision: &str) -> Result<Vec<String>> {
        if revision.len() != 40 || !revision.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("{revision:?} is not a commit id");
        }
        let mut url = self
            .base
            .join("a/projects/")
            .context("building the project URL")?;
        url.path_segments_mut()
            .map_err(|()| anyhow!("{} cannot take a path", self.base))?
            .pop_if_empty()
            .push(project)
            .push("commits")
            .push(revision)
            .push("in");
        let reply = self.get(&url)?;
        Ok(reply
            .get("tags")
            .and_then(Value::as_array)
            .map(|tags| {
                tags.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Link to a change in the Gerrit UI.
    pub(crate) fn change_url(&self, project: &str, number: u64) -> Result<Url> {
        self.base
            .join(&format!("c/{project}/+/{number}"))
            .with_context(|| format!("building the link to change {number}"))
    }
}

/// Every change of a search, reading pages (`read(start)`) until Gerrit
/// stops flagging `_more_changes`.
fn all_pages(mut read: impl FnMut(usize) -> Result<ChangePage>) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    loop {
        let page = read(changes.len())?;
        let got = page.changes.len();
        changes.extend(page.changes);
        if !page.more || got == 0 {
            return Ok(changes);
        }
    }
}

/// A `.env` error without the offending line, which may hold the password.
fn without_line(e: &dotenvy::Error) -> String {
    match e {
        dotenvy::Error::LineParse(_, index) => {
            format!("a line does not parse (at character {index})")
        }
        other => other.to_string(),
    }
}

/// Gerrit search for changes on `branches` of `project` that mention any key.
pub(crate) fn search_query(keys: &[IssueKey], project: &str, branches: &[String]) -> String {
    let mentions: Vec<String> = keys.iter().map(|k| format!("message:\"{k}\"")).collect();
    let on: Vec<String> = branches.iter().map(|b| format!("branch:{b}")).collect();
    format!(
        "project:{project} ({}) ({})",
        mentions.join(" OR "),
        on.join(" OR ")
    )
}

/// One page of a search reply.
pub(crate) fn parse_changes(reply: &Value) -> Result<ChangePage> {
    let list = reply
        .as_array()
        .ok_or_else(|| anyhow!("change search: expected a list"))?;
    let changes = list
        .iter()
        .map(|c| {
            let number = c
                .get("_number")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("change without a number"))?;
            let status = match c.get("status").and_then(Value::as_str) {
                Some("NEW") => ChangeStatus::New,
                Some("MERGED") => ChangeStatus::Merged,
                Some("ABANDONED") => ChangeStatus::Abandoned,
                other => bail!("change {number}: unknown status {other:?}"),
            };
            let revision = c.get("current_revision").and_then(Value::as_str);
            let message = revision
                .and_then(|r| c.pointer(&format!("/revisions/{r}/commit/message")))
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("change {number}: no commit message"))?;
            Ok(Change {
                number,
                branch: c
                    .get("branch")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("change {number}: no branch"))?
                    .to_string(),
                status,
                revision: revision.map(String::from),
                keys: keys_in(message),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let more = list
        .last()
        .and_then(|c| c.get("_more_changes"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ChangePage { changes, more })
}

/// Ticket keys named in a commit message ("LU-19400", "EX-15945:",
/// "lu-7-followup"), as Gerrit's `message:` search matches them: two
/// hyphen-joined words, any case.
pub(crate) fn keys_in(message: &str) -> BTreeSet<IssueKey> {
    message
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .flat_map(|token| {
            let parts: Vec<&str> = token.split('-').collect();
            parts
                .iter()
                .zip(parts.iter().skip(1))
                .filter_map(|(project, number)| {
                    format!("{project}-{number}")
                        .to_ascii_uppercase()
                        .parse()
                        .ok()
                })
                .collect::<Vec<IssueKey>>()
        })
        .collect()
}

/// The earliest tag of a family (`2.14.0-ddn258` for prefix `2.14.0-ddn`), by number.
pub(crate) fn first_tag(tags: &[String], prefix: &str) -> Option<String> {
    tags.iter()
        .filter_map(|t| Some((tag_number(t, prefix)?, t)))
        .min_by_key(|(n, _)| *n)
        .map(|(_, t)| t.clone())
}

/// A tag's number in its family: 258 for `2.14.0-ddn258` and prefix `2.14.0-ddn`.
pub(crate) fn tag_number(tag: &str, prefix: &str) -> Option<u32> {
    tag.strip_prefix(prefix)?.parse().ok()
}

fn strip_xssi(text: &str) -> &str {
    text.strip_prefix(XSSI_PREFIX).unwrap_or(text).trim_start()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    fn keys(list: &[&str]) -> BTreeSet<IssueKey> {
        list.iter().map(|k| k.parse().unwrap()).collect()
    }

    #[test]
    fn the_env_file_is_read_with_dotenv_quoting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "# creds\nGERRIT_URL=https://review.whamcloud.com/\nexport GERRIT_USER=\"ivan\"\n\nGERRIT_PASS='p=w\"'\n").unwrap();

        let gerrit = Gerrit::from_env_file(&path).unwrap();

        assert_eq!(gerrit.base.as_str(), "https://review.whamcloud.com/");
        assert_eq!(gerrit.auth, http::basic_auth("ivan", "p=w\""));
    }

    #[test]
    fn an_env_parse_error_does_not_repeat_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        std::fs::write(&path, "GERRIT_PASS='secret-value\n").unwrap();

        let err = Gerrit::from_env_file(&path).err().unwrap().to_string();

        assert!(!err.contains("secret-value"), "{err}");
    }

    #[test]
    fn search_query_names_keys_and_branches() {
        let keys: Vec<IssueKey> = vec!["DDN-7162".parse().unwrap(), "EX-15945".parse().unwrap()];
        let branches = vec!["b_es6_0".to_string(), "b_es7_0".to_string()];

        assert_eq!(
            search_query(&keys, "ex/lustre-release", &branches),
            "project:ex/lustre-release (message:\"DDN-7162\" OR message:\"EX-15945\") (branch:b_es6_0 OR branch:b_es7_0)"
        );
    }

    #[test]
    fn parse_changes_reads_status_revision_keys_and_more() {
        let rev = "4b3ec9fbbab94b3ec9fbbab94b3ec9fbbab94b3e";
        let reply = json!([
            {"_number": 66674, "branch": "b_es6_0", "status": "MERGED", "current_revision": rev,
             "revisions": {rev: {"commit": {"message": "EX-14604 llite: fix mkwrite\n\nLU-19400 lu-1\nChange-Id: I12"}}}},
            {"_number": 70001, "branch": "b_es7_0", "status": "NEW", "current_revision": "abc",
             "revisions": {"abc": {"commit": {"message": "EX-15945 x"}}}, "_more_changes": true}
        ]);

        let page = parse_changes(&reply).unwrap();

        assert!(page.more);
        assert_eq!(
            page.changes.first(),
            Some(&Change {
                number: 66674,
                branch: "b_es6_0".into(),
                status: ChangeStatus::Merged,
                revision: Some(rev.into()),
                keys: keys(&["EX-14604", "LU-19400", "LU-1"]),
            })
        );
        assert_eq!(
            page.changes.get(1).map(|c| (c.status, c.keys.clone())),
            Some((ChangeStatus::New, keys(&["EX-15945"])))
        );
    }

    #[test_case(json!({}), "change search: expected a list" ; "not a list")]
    #[test_case(json!([{"branch": "b"}]), "change without a number" ; "no number")]
    #[test_case(json!([{"_number": 1, "status": "DRAFT"}]), "change 1: unknown status Some(\"DRAFT\")" ; "unknown status")]
    #[test_case(json!([{"_number": 1, "status": "NEW"}]), "change 1: no commit message" ; "no message")]
    fn parse_changes_rejects_odd_replies(reply: Value, message: &str) {
        assert_eq!(parse_changes(&reply).unwrap_err().to_string(), message);
    }

    #[test_case("EX-1: x", &["EX-1"] ; "colon after the key")]
    #[test_case("fixes EX-15 and ex-150/LU-7-followup", &["EX-15", "EX-150", "LU-7"] ; "several in any case")]
    #[test_case("2.14.0-ddn258 b_es6_0 Change-Id: Iabc EX-01", &[] ; "tags, branches and ids are not keys")]
    fn keys_in_finds_whole_keys(message: &str, expected: &[&str]) {
        assert_eq!(keys_in(message), keys(expected));
    }

    #[test_case(&["2.14.0-ddn262", "2.14.0-ddn258", "2.16.0-ddn50", "2.14.0-ddn260"], "2.14.0-ddn", Some("2.14.0-ddn258") ; "earliest of the family")]
    #[test_case(&["2.14.0-ddn99", "2.14.0-ddn100"], "2.14.0-ddn", Some("2.14.0-ddn99") ; "numeric not lexical")]
    #[test_case(&["2.16.0-ddn50", "EXA6.3.10"], "2.14.0-ddn", None ; "none of the family")]
    #[test_case(&["2.14.0-ddn258-rc1"], "2.14.0-ddn", None ; "suffix is not a release tag")]
    fn first_tag_picks_the_lowest_number(tags: &[&str], prefix: &str, expected: Option<&str>) {
        let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();

        assert_eq!(first_tag(&tags, prefix).as_deref(), expected);
    }

    fn change(number: u64) -> Change {
        Change {
            number,
            branch: "b_es6_0".into(),
            status: ChangeStatus::New,
            revision: None,
            keys: BTreeSet::new(),
        }
    }

    #[test]
    fn paging_follows_more_changes_from_the_right_offset() {
        let mut starts = Vec::new();
        let changes = all_pages(|start| {
            starts.push(start);
            Ok(match start {
                0 => ChangePage {
                    changes: vec![change(1), change(2)],
                    more: true,
                },
                _ => ChangePage {
                    changes: vec![change(3)],
                    more: false,
                },
            })
        })
        .unwrap();

        assert_eq!(
            changes.iter().map(|c| c.number).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(starts, [0, 2]);
    }

    #[test]
    fn an_empty_page_ends_paging_even_when_more_is_flagged() {
        let mut calls = 0;
        let changes = all_pages(|_| {
            calls += 1;
            Ok(ChangePage {
                changes: vec![],
                more: true,
            })
        })
        .unwrap();

        assert_eq!((changes.len(), calls), (0, 1));
    }

    #[test]
    fn strip_xssi_removes_the_guard_only() {
        assert_eq!(strip_xssi(")]}'\n[1]"), "[1]");
        assert_eq!(strip_xssi("[1]"), "[1]");
    }

    #[test]
    fn tags_containing_rejects_a_bad_revision_before_any_request() {
        let gerrit = Gerrit {
            agent: http::agent(),
            base: Url::parse("https://127.0.0.1:9/").unwrap(),
            auth: String::new(),
        };

        let err = gerrit
            .tags_containing("ex/lustre-release", "4b3ec9f; rm -rf")
            .unwrap_err();

        assert_eq!(err.to_string(), "\"4b3ec9f; rm -rf\" is not a commit id");
    }

    #[test]
    fn change_url_points_at_the_gerrit_ui() {
        let gerrit = Gerrit {
            agent: http::agent(),
            base: Url::parse("https://review.whamcloud.com/").unwrap(),
            auth: String::new(),
        };

        assert_eq!(
            gerrit
                .change_url("ex/lustre-release", 66674)
                .unwrap()
                .as_str(),
            "https://review.whamcloud.com/c/ex/lustre-release/+/66674"
        );
    }
}
