//! The build-health tile: es6/es7 Lustre and EMF nightly pass rates read from
//! EXA Reporter's local API, over HTTPS pinned to EXA Reporter's certificate.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::time::Duration;
use tracing::warn;

use crate::config::Config;
use crate::http;
use crate::jira::str_at;
use crate::model::{BuildHealth, HealthPart};
use crate::tls;

/// EXA Reporter aggregates Maloo on a cold cache, which takes a while.
const TIMEOUT: Duration = Duration::from_secs(180);
/// Most Maloo sessions EXA Reporter reads per branch (`limit=500` in
/// lustre_reporter's `api_stability`). A count this high means the window
/// may hold older sessions the rate leaves out.
const SESSION_LIMIT: u64 = 500;

/// Reads the tile. A part that fails carries its own error; only a bad
/// configuration fails the whole read.
pub(crate) fn fetch(cfg: &Config) -> Result<BuildHealth> {
    let base = cfg.reporter_url()?;
    let days = cfg.reporter_days.to_string();
    let parts = match tls::pinned_client_config(&cfg.reporter_cert) {
        Ok(tls) => {
            let agent = ureq::AgentBuilder::new()
                .timeout(TIMEOUT)
                .tls_config(tls)
                .build();
            let call = |path: &str, branch: Option<&str>| -> Result<Value> {
                let mut url = base
                    .join(path)
                    .with_context(|| format!("building the URL for {path}"))?;
                url.query_pairs_mut().append_pair("days", &days);
                if let Some(b) = branch {
                    url.query_pairs_mut().append_pair("branch", b);
                }
                let text = http::get_text(&agent, &url, None)?;
                serde_json::from_str(&text)
                    .with_context(|| format!("decoding the reply from {}", url.path()))
            };
            std::thread::scope(|s| {
                let es6 =
                    s.spawn(|| call("api/stability", Some("es6")).and_then(|v| branch_part(&v)));
                let es7 =
                    s.spawn(|| call("api/stability", Some("es7")).and_then(|v| branch_part(&v)));
                let emf = call("api/emf/stability", None).and_then(|v| emf_part(&v));
                [joined(es6.join()), joined(es7.join()), emf]
            })
        }
        Err(e) => {
            let message = format!("{e:#}");
            [Err(anyhow!("{message}")), Err(anyhow!("{message}")), Err(e)]
        }
    };
    let [es6, es7, emf] = parts.map(|r| {
        Some(r.unwrap_or_else(|e| {
            warn!("EXA Reporter: {e:#}");
            HealthPart {
                error: Some(format!("{e:#}")),
                ..HealthPart::default()
            }
        }))
    });
    Ok(BuildHealth {
        url: base,
        days: cfg.reporter_days,
        es6,
        es7,
        emf,
    })
}

fn joined<T>(r: std::thread::Result<Result<T>>) -> Result<T> {
    r.map_err(|_| anyhow!("the request thread panicked"))?
}

/// The `summary` of a reply. EXA Reporter answers HTTP 200 with
/// `{"ok": false, "kind", "error"}` when its own source failed.
fn summary(reply: &Value) -> Result<&Value> {
    if reply.get("ok").and_then(Value::as_bool) == Some(false) {
        let kind = str_at(reply, "/kind").unwrap_or("error");
        bail!(
            "EXA Reporter: {kind}: {}",
            str_at(reply, "/error").unwrap_or("no detail")
        );
    }
    reply
        .get("summary")
        .ok_or_else(|| anyhow!("no summary in the reply"))
}

fn count(summary: &Value, key: &str) -> Result<u64> {
    summary
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("no {key} count"))
}

/// A rate, which EXA Reporter leaves empty only when there was nothing to rate.
fn rate(summary: &Value, key: &str, counted: u64) -> Result<Option<f64>> {
    match summary.get(key).and_then(Value::as_f64) {
        Some(r) => Ok(Some(r)),
        None if counted == 0 => Ok(None),
        None => Err(anyhow!("no {key} for {counted} counted")),
    }
}

/// es6/es7 numbers from `/api/stability`: sessions with no failure, of all sessions.
pub(crate) fn branch_part(reply: &Value) -> Result<HealthPart> {
    let summary = summary(reply)?;
    let sessions = count(summary, "sessions")?;
    Ok(HealthPart {
        rate: rate(summary, "session_pass_rate", sessions)?,
        count: sessions,
        excluded: 0,
        capped: sessions >= SESSION_LIMIT,
        error: None,
    })
}

/// EMF numbers from `/api/emf/stability`. EXA Reporter rates passed runs of
/// the finished ones (passed and failed); cancelled, skipped, timed-out and
/// running runs are left out of the rate, so they are counted apart.
pub(crate) fn emf_part(reply: &Value) -> Result<HealthPart> {
    let summary = summary(reply)?;
    let finished = count(summary, "passed")? + count(summary, "failed")?;
    let excluded = count(summary, "other")?;
    Ok(HealthPart {
        rate: rate(summary, "pass_rate", finished)?,
        count: finished,
        excluded,
        capped: false,
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use test_case::test_case;

    #[test]
    fn branch_part_reads_the_summary() {
        let reply = json!({"ok": true, "summary": {"sessions": 500, "session_pass_rate": 55.8, "testset_pass_rate": 92.5}});

        assert_eq!(
            branch_part(&reply).unwrap(),
            HealthPart {
                rate: Some(55.8),
                count: 500,
                excluded: 0,
                capped: true,
                error: None
            }
        );
        let fewer = json!({"summary": {"sessions": 499, "session_pass_rate": 50.0}});
        assert!(
            !branch_part(&fewer).unwrap().capped,
            "below EXA Reporter's limit"
        );
    }

    #[test]
    fn emf_rates_the_finished_runs_and_counts_the_rest_apart() {
        let reply = json!({"summary": {"runs": 11, "passed": 8, "failed": 2, "other": 1, "pass_rate": 80.0}});

        assert_eq!(
            emf_part(&reply).unwrap(),
            HealthPart {
                rate: Some(80.0),
                count: 10,
                excluded: 1,
                capped: false,
                error: None
            }
        );
    }

    #[test]
    fn a_window_of_only_unfinished_runs_has_no_rate_rather_than_an_error() {
        let reply = json!({"summary": {"runs": 2, "passed": 0, "failed": 0, "other": 2, "pass_rate": null}});

        assert_eq!(
            emf_part(&reply).unwrap(),
            HealthPart {
                rate: None,
                count: 0,
                excluded: 2,
                capped: false,
                error: None
            }
        );
        assert_eq!(
            branch_part(&json!({"summary": {"sessions": 0, "session_pass_rate": null}})).unwrap(),
            HealthPart {
                rate: None,
                count: 0,
                excluded: 0,
                capped: false,
                error: None
            }
        );
    }

    #[test_case(json!({"ok": false, "kind": "auth", "error": "Maloo credentials expired"}), "EXA Reporter: auth: Maloo credentials expired" ; "its own source failed")]
    #[test_case(json!({}), "no summary in the reply" ; "no summary")]
    #[test_case(json!({"summary": {"sessions": 3}}), "no session_pass_rate for 3 counted" ; "rate missing")]
    #[test_case(json!({"summary": {"session_pass_rate": 1.0}}), "no sessions count" ; "count missing")]
    fn odd_replies_are_errors_not_zeros(reply: Value, message: &str) {
        assert_eq!(branch_part(&reply).unwrap_err().to_string(), message);
    }

    #[test]
    fn a_missing_certificate_is_reported_on_every_part() {
        let cfg = Config {
            reporter_cert: "/nonexistent/cert.pem".into(),
            ..Config::default()
        };

        let health = fetch(&cfg).unwrap();

        for part in [&health.es6, &health.es7, &health.emf] {
            let part = part.as_ref().unwrap();
            assert_eq!((part.rate, part.count), (None, 0));
            assert!(
                part.error
                    .as_deref()
                    .unwrap_or_default()
                    .contains("/nonexistent/cert.pem")
            );
        }
    }
}
