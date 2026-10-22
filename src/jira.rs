//! Read-only Jira REST client for both instances. Credentials come from the
//! llm_jira tool's own `~/.jira-tool.json` (`cloud`: basic, `lu`: bearer) and
//! are only ever placed in an HTTP header, never logged or passed to a process.

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use std::sync::LazyLock;
use tracing::warn;
use url::Url;

use crate::http;
use crate::model::{Category, Comment, FixState, Instance, IssueKey, LinkRef, Severity, Ticket};

/// DDN's "Severity" field ("1".."4", or "Severity 4 - ..." text on SFAFE).
const SEVERITY_FIELD: &str = "customfield_15801";
/// DDN's "Site Affected" field.
const SITE_FIELD: &str = "customfield_16133";
/// Fields every ticket query asks for. Comment bodies are left out on purpose:
/// the newest human comment is read separately, only where idle time matters.
static FIELDS: LazyLock<String> = LazyLock::new(|| {
    format!(
        "summary,status,priority,assignee,issuetype,created,updated,duedate,fixVersions,issuelinks,labels,{SEVERITY_FIELD},{SITE_FIELD}"
    )
});
/// Instance names in `~/.jira-tool.json`.
const CLOUD_NAME: &str = "cloud";
const WHAMCLOUD_NAME: &str = "lu";
/// Issues per search page (the most both instances return).
pub(crate) const PAGE_SIZE: usize = 100;
/// Comments per page when looking for the newest one written by a person.
/// Most tickets have one in the first page; bot-heavy ones page further.
const COMMENT_PAGE_SIZE: usize = 50;
/// Jira's timestamp format, e.g. `2026-10-17T20:00:00.000-0400`.
const TIMESTAMP: &str = "%Y-%m-%dT%H:%M:%S%.f%z";

/// One Jira site and the header that authenticates to it.
struct Site {
    base: Url,
    auth: String,
}

/// Client for Jira cloud and Whamcloud Jira.
pub(crate) struct Jira {
    agent: ureq::Agent,
    cloud: Site,
    whamcloud: Site,
}

/// A fixVersion.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Version {
    /// Its name, e.g. `ES6.3.10`.
    pub(crate) name: String,
    /// Release date.
    pub(crate) release: Option<NaiveDate>,
    /// Start date.
    pub(crate) start: Option<NaiveDate>,
    /// Already released.
    pub(crate) released: bool,
    /// Archived.
    pub(crate) archived: bool,
}

/// The tickets a query found.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Tickets {
    /// Every issue that could be read.
    pub(crate) items: Vec<Ticket>,
    /// Issues left out because they could not be read (logged one by one).
    pub(crate) skipped: usize,
}

fn site_from(tool_config: &Value, name: &str) -> Result<Site> {
    let instance = tool_config
        .pointer(&format!("/instances/{name}"))
        .ok_or_else(|| anyhow!("no instance '{name}' in the Jira tool config"))?;
    let server =
        str_at(instance, "/server").ok_or_else(|| anyhow!("instance '{name}' has no server"))?;
    let base = Url::parse(&format!("{}/", server.trim_end_matches('/')))
        .with_context(|| format!("instance '{name}' server is not a URL"))?;
    let token =
        str_at(instance, "/auth/token").ok_or_else(|| anyhow!("instance '{name}' has no token"))?;
    let auth = match str_at(instance, "/auth/type") {
        Some("basic") => {
            let email = str_at(instance, "/auth/email")
                .ok_or_else(|| anyhow!("instance '{name}' has no email"))?;
            http::basic_auth(email, token)
        }
        Some("bearer" | "pat") => format!("Bearer {token}"),
        other => bail!("instance '{name}' has auth type {other:?}, expected basic or bearer"),
    };
    Ok(Site { base, auth })
}

impl Jira {
    /// Reads both sites from the llm_jira tool config at `path`.
    pub(crate) fn from_tool_config(path: &Path) -> Result<Self> {
        let raw =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Value =
            serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Self {
            agent: http::agent(),
            cloud: site_from(&config, CLOUD_NAME)?,
            whamcloud: site_from(&config, WHAMCLOUD_NAME)?,
        })
    }

    fn site(&self, instance: Instance) -> &Site {
        match instance {
            Instance::Cloud => &self.cloud,
            Instance::Whamcloud => &self.whamcloud,
        }
    }

    /// The site root, ending in `/`.
    pub(crate) fn base(&self, instance: Instance) -> &Url {
        &self.site(instance).base
    }

    /// GETs `path` (relative, no leading `/`) on one site and decodes the JSON.
    pub(crate) fn get(
        &self,
        instance: Instance,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Value> {
        let site = self.site(instance);
        let mut url = site
            .base
            .join(path)
            .with_context(|| format!("building the URL for {path}"))?;
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query.iter().copied());
        }
        let text = http::get_text(&self.agent, &url, Some(&site.auth))?;
        serde_json::from_str(&text)
            .with_context(|| format!("decoding the reply from {}", url.path()))
    }

    /// Every issue matching `jql`, across all pages, as returned by Jira.
    pub(crate) fn search(&self, instance: Instance, jql: &str, fields: &str) -> Result<Vec<Value>> {
        let page_size = PAGE_SIZE.to_string();
        match instance {
            Instance::Cloud => cloud_pages(|token| {
                let mut query = vec![
                    ("jql", jql),
                    ("fields", fields),
                    ("maxResults", page_size.as_str()),
                ];
                if let Some(t) = token {
                    query.push(("nextPageToken", t));
                }
                self.get(instance, "rest/api/3/search/jql", &query)
            }),
            Instance::Whamcloud => server_pages(|start| {
                let start = start.to_string();
                let query = [
                    ("jql", jql),
                    ("fields", fields),
                    ("startAt", start.as_str()),
                    ("maxResults", page_size.as_str()),
                ];
                self.get(instance, "rest/api/2/search", &query)
            }),
        }
    }

    /// Every ticket matching `jql`. An issue that cannot be read is logged,
    /// counted and left out rather than failing the whole query.
    pub(crate) fn tickets(&self, instance: Instance, jql: &str) -> Result<Tickets> {
        let base = self.base(instance);
        let mut found = Tickets::default();
        for issue in self.search(instance, jql, &FIELDS)? {
            match ticket_from(&issue, instance, base) {
                Ok(t) => found.items.push(t),
                Err(e) => {
                    warn!("skipping an unreadable issue from '{jql}': {e:#}");
                    found.skipped += 1;
                }
            }
        }
        Ok(found)
    }

    /// The fixVersions of a Jira cloud project.
    pub(crate) fn versions(&self, project: &str) -> Result<Vec<Version>> {
        let reply = self.get(
            Instance::Cloud,
            &format!("rest/api/3/project/{project}/versions"),
            &[],
        )?;
        let list = reply
            .as_array()
            .ok_or_else(|| anyhow!("{project} versions: expected a list"))?;
        Ok(list.iter().filter_map(version_from).collect())
    }

    /// The newest comment not written by a bot; `None` when no person ever commented.
    pub(crate) fn latest_human_comment(
        &self,
        instance: Instance,
        key: &IssueKey,
        bots: &[String],
    ) -> Result<Option<Comment>> {
        let api = match instance {
            Instance::Cloud => 3,
            Instance::Whamcloud => 2,
        };
        let path = format!("rest/api/{api}/issue/{key}/comment");
        let page_size = COMMENT_PAGE_SIZE.to_string();
        newest_human(
            |start| {
                let start = start.to_string();
                let query = [
                    ("orderBy", "-created"),
                    ("startAt", start.as_str()),
                    ("maxResults", page_size.as_str()),
                ];
                self.get(instance, &path, &query)
            },
            bots,
        )
        .with_context(|| format!("comments of {key}"))
    }
}

/// Every issue of a token-paged search (Jira cloud); `read` returns the page
/// after the given token. A page without what paging relies on is an error,
/// not the last page.
fn cloud_pages(mut read: impl FnMut(Option<&str>) -> Result<Value>) -> Result<Vec<Value>> {
    let mut issues = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut page = read(token.as_deref())?;
        issues.extend(take_issues(&mut page)?);
        if page
            .get("isLast")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("search page without isLast"))?
        {
            return Ok(issues);
        }
        let next = str_at(&page, "/nextPageToken")
            .ok_or_else(|| anyhow!("search page without nextPageToken"))?;
        token = Some(next.to_string());
    }
}

/// Every issue of an offset-paged search (Jira server); `read` returns the
/// page starting at the given offset.
fn server_pages(mut read: impl FnMut(usize) -> Result<Value>) -> Result<Vec<Value>> {
    let mut issues = Vec::new();
    loop {
        let mut page = read(issues.len())?;
        let batch = take_issues(&mut page)?;
        let total = page
            .get("total")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("search page without total"))?;
        let got = batch.len();
        issues.extend(batch);
        if got == 0 || issues.len() as u64 >= total {
            return Ok(issues);
        }
    }
}

/// The newest comment by a person, reading pages of comments (newest first)
/// from offset 0 until one has a person's comment or the list ends.
fn newest_human(
    mut read: impl FnMut(usize) -> Result<Value>,
    bots: &[String],
) -> Result<Option<Comment>> {
    let mut start = 0;
    loop {
        let reply = read(start)?;
        let comments = reply
            .get("comments")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("comment page without a comment list"))?;
        if let Some(found) = newest_in(comments, bots) {
            return Ok(Some(found));
        }
        let total = reply
            .get("total")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("comment page without total"))?;
        start += comments.len();
        if comments.is_empty() || start as u64 >= total {
            return Ok(None);
        }
    }
}

/// Moves the `issues` list out of a search page.
fn take_issues(page: &mut Value) -> Result<Vec<Value>> {
    match page.get_mut("issues").map(Value::take) {
        Some(Value::Array(issues)) => Ok(issues),
        _ => bail!("search page without an issue list"),
    }
}

/// The newest comment in one page that was not written by a bot.
fn newest_in(comments: &[Value], bots: &[String]) -> Option<Comment> {
    comments
        .iter()
        .filter_map(|c| {
            let author = str_at(c, "/author/displayName");
            if author.is_some_and(|a| bots.iter().any(|b| b.eq_ignore_ascii_case(a))) {
                return None;
            }
            Some(Comment {
                date: local_date_of(str_at(c, "/created"))?,
                author: author.map(String::from),
            })
        })
        .max_by_key(|c| c.date)
}

fn version_from(v: &Value) -> Option<Version> {
    let Some(name) = str_at(v, "/name") else {
        warn!("skipping a fixVersion without a name");
        return None;
    };
    Some(Version {
        name: name.to_string(),
        release: date_of(str_at(v, "/releaseDate")),
        start: date_of(str_at(v, "/startDate")),
        released: v.get("released").and_then(Value::as_bool).unwrap_or(false),
        archived: v.get("archived").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// Flattens one REST issue into a `Ticket`. An issue without a summary,
/// status, status category, issue type or updated stamp cannot be judged and
/// is refused.
pub(crate) fn ticket_from(issue: &Value, instance: Instance, base: &Url) -> Result<Ticket> {
    let key: IssueKey = str_at(issue, "/key")
        .ok_or_else(|| anyhow!("issue without a key"))?
        .parse()?;
    let fields = issue
        .get("fields")
        .ok_or_else(|| anyhow!("{key}: no fields"))?;
    let required = |pointer: &str, what: &str| {
        str_at(fields, pointer)
            .map(String::from)
            .ok_or_else(|| anyhow!("{key}: no {what}"))
    };
    let category = fields
        .pointer("/status/statusCategory/key")
        .ok_or_else(|| anyhow!("{key}: no status category"))
        .and_then(|v| {
            Category::deserialize(v).with_context(|| format!("{key}: reading the status category"))
        })?;
    let url = base
        .join(&format!("browse/{key}"))
        .with_context(|| format!("{key}: building its link"))?;
    Ok(Ticket {
        url,
        summary: required("/summary", "summary")?,
        status: required("/status/name", "status")?,
        category,
        priority: str_at(fields, "/priority/name").map(String::from),
        severity: fields
            .get(SEVERITY_FIELD)
            .and_then(|v| severity_of(&key, v))
            .map(Severity::field),
        assignee: str_at(fields, "/assignee/displayName").map(String::from),
        issue_type: required("/issuetype/name", "issue type")?,
        created: local_date_of(str_at(fields, "/created")),
        updated: required("/updated", "updated stamp")?,
        due: date_of(str_at(fields, "/duedate")),
        fix_versions: fields
            .get("fixVersions")
            .and_then(Value::as_array)
            .map(|vs| {
                vs.iter()
                    .filter_map(|v| str_at(v, "/name"))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        site: fields.get(SITE_FIELD).and_then(text_of),
        links: links_of(&key, fields),
        last_comment: None,
        idle_days: None,
        also: Vec::new(),
        source: None,
        fix: None,
        fix_state: FixState::NoFix,
        committed: None,
        patches: Vec::new(),
        delivery: None,
        labels: fields
            .get("labels")
            .and_then(Value::as_array)
            .map(|ls| {
                ls.iter()
                    .filter_map(Value::as_str)
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        verification: None,
        key,
        instance,
    })
}

fn links_of(key: &IssueKey, fields: &Value) -> Vec<LinkRef> {
    let Some(links) = fields.get("issuelinks").and_then(Value::as_array) else {
        return Vec::new();
    };
    links
        .iter()
        .filter_map(|link| {
            let (other, relation) = match (link.get("inwardIssue"), link.get("outwardIssue")) {
                (Some(other), _) => (other, str_at(link, "/type/inward")),
                (None, Some(other)) => (other, str_at(link, "/type/outward")),
                (None, None) => return None,
            };
            let other_key = str_at(other, "/key")?;
            let Some(relation) = relation else {
                warn!("{key}: ignoring a link to {other_key} without a link type");
                return None;
            };
            match other_key.parse() {
                Ok(other_key) => Some(LinkRef {
                    key: other_key,
                    relation: relation.to_string(),
                    done: str_at(other, "/fields/status/statusCategory/key") == Some("done"),
                }),
                Err(e) => {
                    warn!("{key}: ignoring a link to {other_key:?}: {e}");
                    None
                }
            }
        })
        .collect()
}

/// Severity is an option on DDN ("1".."4") and text on SFAFE ("Severity 4 - ...").
/// Anything else is logged and read as no severity.
fn severity_of(key: &IssueKey, v: &Value) -> Option<u8> {
    let text = text_of(v)?;
    let lower = text.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("severity")
        .unwrap_or(&lower)
        .trim_start();
    let mut chars = rest.chars();
    let level = chars
        .next()
        .and_then(|c| c.to_digit(10))
        .filter(|d| (1..=4).contains(d));
    match level {
        Some(level) if !chars.next().is_some_and(|c| c.is_ascii_alphanumeric()) => {
            u8::try_from(level).ok()
        }
        _ => {
            warn!("{key}: severity {text:?} is not 1 to 4");
            None
        }
    }
}

/// Text of a plain or option field, joined when it holds several values.
fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Object(o) => o
            .get("value")
            .or_else(|| o.get("name"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string()),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(text_of).collect();
            (!parts.is_empty()).then(|| parts.join(", "))
        }
        _ => None,
    }
}

/// The string at a JSON pointer.
pub(crate) fn str_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str)
}

/// A plain Jira date (`2026-11-06`), or the date part of a timestamp.
pub(crate) fn date_of(s: Option<&str>) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(s?.get(..10)?, "%Y-%m-%d").ok()
}

/// The local date of a Jira timestamp, which carries the server's offset;
/// a plain date is taken as it is.
fn local_date_of(s: Option<&str>) -> Option<NaiveDate> {
    date_in(s, &Local)
}

fn date_in<Tz: TimeZone>(s: Option<&str>, zone: &Tz) -> Option<NaiveDate> {
    let s = s?;
    match DateTime::parse_from_str(s, TIMESTAMP) {
        Ok(t) => Some(t.with_timezone(zone).date_naive()),
        Err(_) => date_of(Some(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::FixedOffset;
    use serde_json::json;
    use test_case::test_case;

    fn base() -> Url {
        Url::parse("https://ime-ddn.atlassian.net/").unwrap()
    }

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn issue() -> Value {
        json!({
            "key": "DDN-7162",
            "fields": {
                "summary": "Case 01349748 | NVIDIA | AI400X3i-BF3| Ethernet HCAs missing",
                "status": {"name": "Open", "statusCategory": {"key": "new"}},
                "priority": {"name": "Medium"},
                "customfield_15801": {"value": "1"},
                "customfield_16133": "Nvidia",
                "assignee": {"displayName": "Abhijit Gadgil"},
                "issuetype": {"name": "Bug"},
                "created": "2026-10-09T10:00:00.000+0000",
                "updated": "2026-10-16T10:00:00.000-0700",
                "duedate": "2026-11-06",
                "fixVersions": [{"name": "ES6.3.10"}],
                "labels": ["awaiting_validation", "nvidia"],
                "issuelinks": [
                    {"type": {"inward": "is blocked by", "outward": "blocks"},
                     "inwardIssue": {"key": "EX-15945", "fields": {"status": {"statusCategory": {"key": "indeterminate"}}}}},
                    {"type": {"inward": "is cloned by", "outward": "clones"},
                     "outwardIssue": {"key": "EX-1", "fields": {"status": {"statusCategory": {"key": "done"}}}}},
                    {"type": {"inward": "x", "outward": "y"}, "outwardIssue": {"key": "not a key"}},
                    {"outwardIssue": {"key": "EX-2"}},
                    {"type": {"inward": "x", "outward": "y"}}
                ]
            }
        })
    }

    #[test]
    fn ticket_from_reads_every_field() {
        let t = ticket_from(&issue(), Instance::Cloud, &base()).unwrap();

        let expected = Ticket {
            key: "DDN-7162".parse().unwrap(),
            instance: Instance::Cloud,
            url: Url::parse("https://ime-ddn.atlassian.net/browse/DDN-7162").unwrap(),
            summary: "Case 01349748 | NVIDIA | AI400X3i-BF3| Ethernet HCAs missing".into(),
            status: "Open".into(),
            category: Category::New,
            priority: Some("Medium".into()),
            severity: Some(Severity::field(1)),
            assignee: Some("Abhijit Gadgil".into()),
            issue_type: "Bug".into(),
            created: Some(d(2026, 10, 9)),
            updated: "2026-10-16T10:00:00.000-0700".into(),
            due: Some(d(2026, 11, 6)),
            fix_versions: vec!["ES6.3.10".into()],
            site: Some("Nvidia".into()),
            links: vec![
                LinkRef {
                    key: "EX-15945".parse().unwrap(),
                    relation: "is blocked by".into(),
                    done: false,
                },
                LinkRef {
                    key: "EX-1".parse().unwrap(),
                    relation: "clones".into(),
                    done: true,
                },
            ],
            last_comment: None,
            idle_days: None,
            also: vec![],
            source: None,
            fix: None,
            fix_state: FixState::NoFix,
            committed: None,
            patches: vec![],
            delivery: None,
            labels: vec!["awaiting_validation".into(), "nvidia".into()],
            verification: None,
        };
        assert_eq!(t, expected);
    }

    #[test_case("/fields/status", json!({"name": "Open"}), "DDN-7162: no status category" ; "no status category")]
    #[test_case("/fields/status", json!({"name": "Odd", "statusCategory": {"key": "undefined"}}), "DDN-7162: reading the status category" ; "unknown status category")]
    #[test_case("/fields/status", json!({"statusCategory": {"key": "new"}}), "DDN-7162: no status" ; "no status name")]
    #[test_case("/fields/summary", Value::Null, "DDN-7162: no summary" ; "no summary")]
    #[test_case("/fields/issuetype", json!({}), "DDN-7162: no issue type" ; "no issue type")]
    #[test_case("/fields/updated", Value::Null, "DDN-7162: no updated stamp" ; "no updated stamp")]
    fn ticket_from_refuses_what_it_cannot_judge(pointer: &str, value: Value, message: &str) {
        let mut bad = issue();
        if let Some(slot) = bad.pointer_mut(pointer) {
            *slot = value;
        }

        let err = ticket_from(&bad, Instance::Cloud, &base()).unwrap_err();

        assert_eq!(err.to_string(), message);
    }

    #[test]
    fn ticket_from_rejects_a_bad_key() {
        let err = ticket_from(
            &json!({"key": "nope", "fields": {}}),
            Instance::Cloud,
            &base(),
        )
        .unwrap_err();

        assert_eq!(err.to_string(), "\"nope\" is not a Jira key");
    }

    #[test_case(json!({"value": "2"}), Some(2) ; "option field")]
    #[test_case(json!("Severity 4 - No immediate Impact"), Some(4) ; "text field")]
    #[test_case(json!("severity 1"), Some(1) ; "lowercase")]
    #[test_case(json!(null), None ; "unset")]
    #[test_case(json!("none"), None ; "no digit")]
    #[test_case(json!("0"), None ; "zero")]
    #[test_case(json!("9"), None ; "out of range")]
    #[test_case(json!("12"), None ; "two digits")]
    #[test_case(json!("P1/Sev 2"), None ; "other wording")]
    fn severity_reads_one_to_four_only(v: Value, expected: Option<u8>) {
        assert_eq!(severity_of(&"DDN-1".parse().unwrap(), &v), expected);
    }

    #[test_case(json!(["Nvidia", {"value": "DGXC"}]), Some("Nvidia, DGXC") ; "list")]
    #[test_case(json!("  "), None ; "blank")]
    #[test_case(json!({"name": "Aurora"}), Some("Aurora") ; "named object")]
    fn text_of_flattens_field_shapes(v: Value, expected: Option<&str>) {
        assert_eq!(text_of(&v).as_deref(), expected);
    }

    fn comment(author: &str, created: &str) -> Value {
        json!({"author": {"displayName": author}, "created": created})
    }

    #[test]
    fn the_newest_person_wins_whatever_the_order() {
        let comments = vec![
            comment("Gerrit Updater", "2026-10-20T01:00:00.000+0300"),
            comment("Ivan", "2026-10-01T01:00:00.000+0300"),
            comment("Jason Huselton", "2026-10-07T12:00:00.000+0300"),
        ];
        let bots = vec!["gerrit updater".to_string()];

        let c = newest_in(&comments, &bots).unwrap();

        assert_eq!(c.author.as_deref(), Some("Jason Huselton"));
        assert_eq!(newest_in(&[], &bots), None);
    }

    #[test]
    fn a_comment_without_an_author_is_a_person_with_no_name() {
        let comments = vec![json!({"created": "2026-10-07T12:00:00.000+0300"})];

        let c = newest_in(&comments, &["Gerrit".to_string()]).unwrap();

        assert_eq!(c.author, None);
    }

    /// Pages of comments as a fake server returns them, newest first.
    fn pages(all: Vec<Value>, size: usize) -> impl FnMut(usize) -> Result<Value> {
        move |start| {
            let page: Vec<Value> = all.iter().skip(start).take(size).cloned().collect();
            Ok(json!({"comments": page, "total": all.len()}))
        }
    }

    #[test]
    fn comment_paging_goes_past_pages_of_bots() {
        let bots = vec!["Maloo".to_string()];
        let mut all: Vec<Value> = (0..5)
            .map(|_| comment("Maloo", "2026-10-20T01:00:00.000+0300"))
            .collect();
        all.push(comment("Jason", "2026-09-01T12:00:00.000+0300"));

        let found = newest_human(pages(all.clone(), 2), &bots).unwrap();
        let none = newest_human(pages(all.into_iter().take(5).collect(), 2), &bots).unwrap();

        assert_eq!(found.and_then(|c| c.author), Some("Jason".into()));
        assert_eq!(none, None, "only bots: no person ever commented");
    }

    #[test]
    fn a_comment_page_without_its_list_is_an_error() {
        let err = newest_human(|_| Ok(json!({"total": 3})), &[]).unwrap_err();

        assert_eq!(err.to_string(), "comment page without a comment list");
    }

    #[test]
    fn cloud_paging_follows_tokens_until_the_last_page() {
        let mut seen = Vec::new();
        let issues = cloud_pages(|token| {
            seen.push(token.map(String::from));
            Ok(match token {
                None => {
                    json!({"issues": [{"key": "EX-1"}], "isLast": false, "nextPageToken": "t2"})
                }
                Some(_) => json!({"issues": [{"key": "EX-2"}], "isLast": true}),
            })
        })
        .unwrap();

        assert_eq!(issues.len(), 2);
        assert_eq!(seen, [None, Some("t2".to_string())]);
    }

    #[test_case(json!({"issues": [], "nextPageToken": "x"}), "search page without isLast" ; "no isLast")]
    #[test_case(json!({"issues": [], "isLast": false}), "search page without nextPageToken" ; "no token")]
    #[test_case(json!({"isLast": true}), "search page without an issue list" ; "no issues")]
    fn a_cloud_page_without_its_paging_fields_is_an_error(page: Value, message: &str) {
        let err = cloud_pages(|_| Ok(page.clone())).unwrap_err();

        assert_eq!(err.to_string(), message);
    }

    #[test]
    fn server_paging_stops_at_the_total_or_an_empty_page() {
        let all: Vec<Value> = (1..=5)
            .map(|i| json!({"key": format!("GCP-{i}")}))
            .collect();
        let read = |start: usize| -> Result<Value> {
            let page: Vec<Value> = all.iter().skip(start).take(2).cloned().collect();
            Ok(json!({"issues": page, "total": 5}))
        };

        assert_eq!(server_pages(read).unwrap().len(), 5);
        assert_eq!(
            server_pages(|_| Ok(json!({"issues": [], "total": 9})))
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            server_pages(|_| Ok(json!({"issues": []})))
                .unwrap_err()
                .to_string(),
            "search page without total"
        );
    }

    #[test]
    fn timestamps_turn_into_local_dates() {
        let moscow = FixedOffset::east_opt(3 * 3600).unwrap();

        assert_eq!(
            date_in(Some("2026-10-17T20:00:00.000-0400"), &moscow),
            Some(d(2026, 10, 18))
        );
        assert_eq!(
            date_in(Some("2026-10-17T10:00:00.000+0000"), &moscow),
            Some(d(2026, 10, 17))
        );
        assert_eq!(
            date_in(Some("2026-11-06"), &moscow),
            Some(d(2026, 11, 6)),
            "a plain date stays as it is"
        );
        assert_eq!(date_in(Some("soon"), &moscow), None);
    }

    #[test]
    fn version_from_reads_dates_and_flags() {
        let v = version_from(
            &json!({"name": "ES6.3.10", "releaseDate": "2026-12-15", "startDate": "2026-09-01", "released": false}),
        );

        assert_eq!(
            v,
            Some(Version {
                name: "ES6.3.10".into(),
                release: Some(d(2026, 12, 15)),
                start: Some(d(2026, 9, 1)),
                released: false,
                archived: false
            })
        );
        assert_eq!(version_from(&json!({"releaseDate": "2026-12-15"})), None);
    }

    #[test]
    fn fields_ask_for_severity_and_site() {
        let fields: Vec<&str> = FIELDS.split(',').collect();

        assert!(fields.contains(&SEVERITY_FIELD) && fields.contains(&SITE_FIELD));
    }

    #[test]
    fn site_from_builds_both_auth_kinds() {
        let config = json!({"instances": {
            "cloud": {"server": "https://ime-ddn.atlassian.net", "auth": {"type": "basic", "email": "a@b", "token": "t"}},
            "lu": {"server": "https://jira.whamcloud.com/", "auth": {"type": "bearer", "token": "t"}},
            "odd": {"server": "https://x", "auth": {"type": "kerberos", "token": "t"}}
        }});

        let cloud = site_from(&config, "cloud").unwrap();
        let lu = site_from(&config, "lu").unwrap();

        assert_eq!(cloud.base.as_str(), "https://ime-ddn.atlassian.net/");
        assert_eq!(cloud.auth, http::basic_auth("a@b", "t"));
        assert_eq!(
            (lu.base.as_str(), lu.auth.as_str()),
            ("https://jira.whamcloud.com/", "Bearer t")
        );
        assert_eq!(
            site_from(&config, "odd").err().map(|e| e.to_string()),
            Some(
                "instance 'odd' has auth type Some(\"kerberos\"), expected basic or bearer"
                    .to_string()
            )
        );
        assert_eq!(
            site_from(&config, "missing").err().map(|e| e.to_string()),
            Some("no instance 'missing' in the Jira tool config".to_string())
        );
    }
}
