//! Read-only GitHub client: the EMF pull requests and the ticket keys each
//! names. The token comes from the gh CLI, which owns it, and only goes into
//! an HTTP header.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;
use tracing::warn;
use url::Url;

use crate::config::Config;
use crate::gerrit;
use crate::http;
use crate::model::{IssueKey, PullRef, PullState};
use crate::store;

/// GitHub's REST API root.
const API: &str = "https://api.github.com/";
/// Pull requests per page, GitHub's most.
const PAGE_SIZE: usize = 100;
/// How far before the last read a pull request's update is read again, so
/// one updated while the last read ran is not missed.
const OVERLAP_MINUTES: i64 = 60;

/// A pull request and the ticket keys its title and branch name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Pull {
    /// What the page shows of it.
    pub(crate) pr: PullRef,
    /// The keys it names.
    pub(crate) keys: BTreeSet<IssueKey>,
    /// When GitHub last saw it change.
    pub(crate) updated: DateTime<Utc>,
}

/// Every pull request read so far, kept between refreshes so that each one
/// reads only those updated since.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    /// When the last read began.
    read_at: Option<DateTime<Utc>>,
    /// By number.
    pulls: BTreeMap<u64, Pull>,
}

/// The EMF pull requests: every one on the first refresh, then only those
/// updated since the last read, merged into the cache in the data directory.
pub(crate) fn pulls(cfg: &Config) -> Result<Vec<Pull>> {
    let path = cfg.data_file(store::PULLS_FILE);
    let mut cache = load(&path);
    let started = Utc::now();
    let since = cache
        .read_at
        .map(|t| t - Duration::minutes(OVERLAP_MINUTES));
    let auth = format!("Bearer {}", token(cfg)?);
    let agent = http::agent();
    let base = Url::parse(API)
        .and_then(|u| u.join(&format!("repos/{}/pulls", cfg.emf_repo)))
        .with_context(|| format!("building the pull request URL for {}", cfg.emf_repo))?;
    for page in 1.. {
        let mut url = base.clone();
        url.query_pairs_mut()
            .append_pair("state", "all")
            .append_pair("sort", "updated")
            .append_pair("direction", "desc")
            .append_pair("per_page", &PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
        let text = http::get_text(&agent, &url, Some(&auth))?;
        let reply: Value =
            serde_json::from_str(&text).context("decoding GitHub's pull requests")?;
        let found = parse_page(&reply)?;
        let older = since.is_some_and(|s| found.iter().any(|p| p.updated < s));
        let last = found.len() < PAGE_SIZE;
        cache
            .pulls
            .extend(found.into_iter().map(|p| (p.pr.number, p)));
        if older || last {
            break;
        }
    }
    cache.read_at = Some(started);
    if let Err(e) = store::write_json(&path, &cache) {
        warn!("{e:#}");
    }
    Ok(cache.pulls.into_values().collect())
}

/// The cache, or an empty one when it is missing or unreadable: then every
/// pull request is read again.
fn load(path: &Path) -> Cache {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The GitHub token of `github_user`, from the gh CLI.
fn token(cfg: &Config) -> Result<String> {
    let mut command = Command::new(&cfg.gh_command);
    command.args(["auth", "token", "--hostname", "github.com"]);
    if let Some(user) = &cfg.github_user {
        command.args(["--user", user]);
    }
    let out = command
        .output()
        .with_context(|| format!("running {}", cfg.gh_command.display()))?;
    if !out.status.success() {
        bail!(
            "{} auth token failed: {}",
            cfg.gh_command.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let token = String::from_utf8(out.stdout).context("the gh token is not text")?;
    let token = token.trim();
    if token.is_empty() {
        bail!("gh has no GitHub token");
    }
    Ok(token.to_string())
}

/// One page of `GET /repos/{repo}/pulls`.
pub(crate) fn parse_page(reply: &Value) -> Result<Vec<Pull>> {
    reply
        .as_array()
        .ok_or_else(|| anyhow!("pull requests: expected a list"))?
        .iter()
        .map(|p| {
            let number = p
                .get("number")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("a pull request without a number"))?;
            let text = |pointer: &str| {
                p.pointer(pointer)
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("pull request {number}: no {pointer}"))
            };
            let state = if p.get("merged_at").is_some_and(|m| !m.is_null()) {
                PullState::Merged
            } else if text("/state")? == "open" {
                PullState::Open
            } else {
                PullState::Closed
            };
            let title = text("/title")?;
            let head = p.pointer("/head/ref").and_then(Value::as_str).unwrap_or("");
            Ok(Pull {
                pr: PullRef {
                    number,
                    url: Url::parse(text("/html_url")?)
                        .with_context(|| format!("pull request {number}: bad html_url"))?,
                    branch: text("/base/ref")?.to_string(),
                    state,
                },
                keys: gerrit::keys_in(&format!("{title} {head}")),
                updated: text("/updated_at")?
                    .parse()
                    .with_context(|| format!("pull request {number}: bad updated_at"))?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pull(
        number: u64,
        state: &str,
        merged: Option<&str>,
        base: &str,
        title: &str,
        head: &str,
    ) -> Value {
        json!({
            "number": number,
            "html_url": format!("https://github.com/whamcloud/exascaler-management-framework/pull/{number}"),
            "title": title,
            "state": state,
            "merged_at": merged,
            "base": {"ref": base},
            "head": {"ref": head},
            "updated_at": "2026-11-03T12:00:00Z",
        })
    }

    #[test]
    fn a_page_gives_each_pull_request_its_state_branch_and_keys() {
        let reply = json!([
            pull(
                6062,
                "closed",
                Some("2026-11-02T10:00:00Z"),
                "release/EXA7_beta",
                "[EXA7_beta] EX-16275: Wait for node-manager socket",
                "backport/EXA7_beta/pr-6011"
            ),
            pull(
                6068,
                "open",
                None,
                "release/EXA7_beta",
                "Write the logrotate snippet",
                "lyadav/EX-16276"
            ),
            pull(
                6000,
                "closed",
                None,
                "master",
                "`GCP-313`: feat(gcp): tune ko2iblnd",
                "arshinde-mah/GCP-313-rdma"
            ),
        ]);

        let found = parse_page(&reply).unwrap();
        let summary: Vec<(u64, PullState, &str, Vec<String>)> = found
            .iter()
            .map(|p| {
                (
                    p.pr.number,
                    p.pr.state,
                    p.pr.branch.as_str(),
                    p.keys
                        .iter()
                        .map(ToString::to_string)
                        .filter(|k| !k.starts_with("PR-"))
                        .collect(),
                )
            })
            .collect();

        assert_eq!(
            summary,
            [
                (
                    6062,
                    PullState::Merged,
                    "release/EXA7_beta",
                    vec!["EX-16275".to_string()]
                ),
                (
                    6068,
                    PullState::Open,
                    "release/EXA7_beta",
                    vec!["EX-16276".to_string()]
                ),
                (
                    6000,
                    PullState::Closed,
                    "master",
                    vec!["GCP-313".to_string()]
                ),
            ],
            "a key in the title or the branch name counts"
        );
    }

    #[test]
    fn a_page_without_a_list_or_a_field_is_an_error() {
        assert!(parse_page(&json!({"message": "Bad credentials"})).is_err());
        let mut broken = pull(1, "open", None, "master", "EX-1: x", "a");
        broken["base"] = json!({});
        assert!(parse_page(&json!([broken])).is_err());
    }
}
