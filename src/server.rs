//! HTTPS server. Serves the dashboard and the latest snapshot, accepts edits
//! of committed dates and of the refresh interval, and runs each refresh in a
//! short-lived worker process so the server itself stays small.

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tracing::{error, info, warn};

use crate::commitments::{self, Commitments};
use crate::config::{self, Config, REFRESH_CHOICES};
use crate::model::{Committed, IssueKey, SNAPSHOT_FORMAT};
use crate::store::{self, REFRESH_LOG, SNAPSHOT_FILE};
use crate::tls::{self, CertPaths};

const INDEX: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
/// The .xlsx writer behind the page's table exports.
const XLSX_JS: &str = include_str!("../static/xlsx.js");
const STYLE: &str = include_str!("../static/style.css");
const JS_TYPE: &str = "text/javascript; charset=utf-8";
/// The worker sub-command the server runs for each refresh.
pub(crate) const REFRESH_COMMAND: &str = "refresh-once";
/// How often the scheduler checks whether a refresh is due and rereads the
/// refresh interval from the config file.
const SCHEDULER_TICK: Duration = Duration::from_secs(30);
/// How often the certificate is checked for renewal.
const CERT_CHECK: Duration = Duration::from_secs(24 * 60 * 60);
/// A refresh still running after this is stopped. A cold one (every comment
/// read afresh) took under 3 minutes on 21 Oct 2026.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Time open requests get to finish when the server is told to stop.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// Header the dashboard sends with every change; other sites cannot send it
/// without a CORS preflight, which this server never grants.
const CLIENT_HEADER: &str = "x-exa-status";
/// Longest note kept with a committed date: a sentence or two; more belongs in Jira.
const MAX_NOTE_CHARS: usize = 500;
/// Only the dashboard's own files may load; nothing may frame it.
const CSP: &str = "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// Shared server state.
pub(crate) struct AppState {
    /// Settings as loaded at start; the refresh interval lives in `refresh_minutes`.
    cfg: Config,
    /// The port served, for the Host and Origin checks.
    port: u16,
    /// The config file the interval is saved to and reread from.
    config_path: PathBuf,
    /// The config file was named by `EXA_STATUS_CONFIG`, so it must exist.
    config_named: bool,
    /// The executable run as the refresh worker.
    worker: PathBuf,
    /// Minutes between automatic refreshes; 0 means by hand only.
    refresh_minutes: AtomicU64,
    /// The snapshot on disk has an older layout than this server's page.
    outdated_snapshot: AtomicBool,
    /// Refresh progress.
    refresh: Mutex<RefreshState>,
    /// The last config problem logged, so it is logged once, not every tick.
    config_warning: Mutex<Option<String>>,
    /// Serializes writes to the commitments and config files.
    file_writes: tokio::sync::Mutex<()>,
}

impl AppState {
    /// State for a server on `port` that runs `worker refresh-once` and
    /// saves settings to `config_path`.
    pub(crate) fn new(
        cfg: Config,
        port: u16,
        (config_path, config_named): (PathBuf, bool),
        worker: PathBuf,
    ) -> Self {
        Self {
            refresh_minutes: AtomicU64::new(cfg.refresh_minutes),
            cfg,
            port,
            config_path,
            config_named,
            worker,
            outdated_snapshot: AtomicBool::new(false),
            refresh: Mutex::new(RefreshState::default()),
            config_warning: Mutex::new(None),
            file_writes: tokio::sync::Mutex::new(()),
        }
    }
}

/// Where the refresh worker is.
#[derive(Debug, Default)]
struct RefreshState {
    /// A worker is running.
    running: bool,
    /// Another refresh was asked for while one ran.
    rerun: bool,
    /// When the last worker started.
    last_attempt: Option<DateTime<Local>>,
    /// When the last worker ended.
    finished: Option<DateTime<Local>>,
    /// The last worker failed.
    failed: bool,
}

/// What a request for a refresh did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Begin {
    /// A worker started.
    Started,
    /// One is running; another follows it.
    Queued,
}

impl RefreshState {
    /// Starts a refresh, or queues one behind the running one.
    fn begin(&mut self, now: DateTime<Local>) -> Begin {
        if self.running {
            self.rerun = true;
            Begin::Queued
        } else {
            self.running = true;
            self.last_attempt = Some(now);
            Begin::Started
        }
    }

    /// Records the end of a refresh; true when a queued one should start.
    fn end(&mut self, now: DateTime<Local>, ok: bool) -> bool {
        self.running = false;
        self.finished = Some(now);
        self.failed = !ok;
        std::mem::take(&mut self.rerun)
    }

    /// When the next automatic refresh is due: once the snapshot is an
    /// interval old and so is the last attempt, which may have failed (a
    /// failing source is retried once per interval, not every tick). `None`
    /// when refreshing by hand only.
    fn next_due(
        &self,
        snapshot_at: Option<DateTime<Local>>,
        minutes: u64,
        now: DateTime<Local>,
    ) -> Option<DateTime<Local>> {
        let minutes = i64::try_from(minutes).ok().filter(|m| *m > 0)?;
        let interval = chrono::Duration::try_minutes(minutes)?;
        Some(
            [snapshot_at, self.last_attempt]
                .into_iter()
                .flatten()
                .map(|t| t + interval)
                .max()
                .unwrap_or(now),
        )
    }

    /// An automatic refresh is due now.
    fn due(
        &self,
        snapshot_at: Option<DateTime<Local>>,
        minutes: u64,
        now: DateTime<Local>,
    ) -> bool {
        !self.running
            && self
                .next_due(snapshot_at, minutes, now)
                .is_some_and(|t| t <= now)
    }
}

/// Refresh progress, as the UI polls it.
#[derive(Serialize)]
struct Status {
    /// A refresh is running.
    refreshing: bool,
    /// Another is queued behind it.
    queued: bool,
    /// When the last refresh ended.
    finished: Option<DateTime<Local>>,
    /// The last refresh failed.
    failed: bool,
    /// When the snapshot was written.
    snapshot_at: Option<DateTime<Local>>,
    /// When the next automatic refresh is due.
    next_refresh: Option<DateTime<Local>>,
    /// The refresh interval.
    interval_minutes: u64,
    /// What a refresh request just did.
    refresh: Option<Begin>,
}

/// A change of the refresh interval.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsBody {
    refresh_minutes: u64,
}

/// A committed date to set, or to clear with `date: null`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommitmentBody {
    key: String,
    date: Option<NaiveDate>,
    note: Option<String>,
}

/// Just the layout version of a snapshot file.
#[derive(Deserialize)]
struct SnapshotHead {
    /// Missing in snapshots written before the field existed.
    format: Option<u32>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs blocking file work off the async threads.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| anyhow!("a file task stopped: {e}"))?
}

/// Serves the dashboard until the process gets SIGTERM or SIGINT; a running
/// refresh worker is stopped with it.
pub(crate) async fn serve(cfg: Config, port: u16, cert: CertPaths) -> Result<()> {
    let tls = RustlsConfig::from_pem_file(&cert.cert, &cert.key)
        .await
        .context("loading the TLS certificate")?;
    let addr = SocketAddr::new(cfg.host, port);
    let worker = std::env::current_exe().context("finding the worker executable")?;
    let cert_dir = cfg.cert_dir.clone();
    let state = Arc::new(AppState::new(cfg, port, Config::path(), worker));
    // An unreadable snapshot counts as outdated, so the first tick replaces it.
    let outdated = match snapshot_format(&state.cfg.data_file(SNAPSHOT_FILE)).await {
        Ok(format) => format.is_some_and(|f| f < SNAPSHOT_FORMAT),
        Err(e) => {
            warn!("{e:#}; a refresh replaces it");
            true
        }
    };
    state.outdated_snapshot.store(outdated, Ordering::Relaxed);
    let handle: Handle<SocketAddr> = Handle::new();
    for kind in [SignalKind::terminate(), SignalKind::interrupt()] {
        let mut stream = signal(kind).context("watching for stop signals")?;
        let handle = handle.clone();
        tokio::spawn(async move {
            if stream.recv().await.is_some() {
                info!("stopping");
                handle.graceful_shutdown(Some(SHUTDOWN_GRACE));
            }
        });
    }
    tokio::spawn(scheduler(Arc::clone(&state)));
    tokio::spawn(renew_certificate(tls.clone(), cert_dir));
    info!("EXA status: https://localhost:{port}");
    axum_server::bind_rustls(addr, tls)
        .handle(handle)
        .serve(router(state).into_make_service())
        .await
        .context("serving")
}

/// The routes, behind the Host/Origin guard.
pub(crate) fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/xlsx.js", get(xlsx_js))
        .route("/style.css", get(style))
        .route("/api/snapshot", get(snapshot))
        .route("/api/status", get(status))
        .route("/api/refresh", post(refresh))
        .route("/api/settings", get(get_settings).post(set_settings))
        .route("/api/commitments", post(set_commitment))
        .layer(middleware::from_fn_with_state(Arc::clone(&state), guard))
        .with_state(state)
}

/// Accepts requests for this machine only (blocks DNS rebinding) and changes
/// only from the dashboard page (blocks other sites), then adds security headers.
async fn guard(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let authority = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(String::from)
        .or_else(|| request.uri().authority().map(ToString::to_string));
    if !authority
        .as_deref()
        .is_some_and(|a| is_local(a, state.port))
    {
        return problem(StatusCode::FORBIDDEN, "unexpected Host");
    }
    let reads_only = matches!(*request.method(), Method::GET | Method::HEAD);
    if !reads_only && !from_dashboard(request.headers(), state.port) {
        return problem(
            StatusCode::FORBIDDEN,
            "changes are accepted from the dashboard page only",
        );
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn is_local(authority: &str, port: u16) -> bool {
    authority == format!("localhost:{port}") || authority == format!("127.0.0.1:{port}")
}

fn from_dashboard(headers: &HeaderMap, port: u16) -> bool {
    let origin_ok = match headers.get(header::ORIGIN).map(|o| o.to_str()) {
        None => true,
        Some(Ok(origin)) => origin
            .strip_prefix("https://")
            .is_some_and(|a| is_local(a, port)),
        Some(Err(_)) => false,
    };
    origin_ok && headers.get(CLIENT_HEADER).is_some_and(|v| v == "1")
}

fn problem(code: StatusCode, message: &str) -> Response {
    (code, Json(json!({ "error": message }))).into_response()
}

async fn index() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], INDEX)
}

async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, JS_TYPE)], APP_JS)
}

async fn xlsx_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, JS_TYPE)], XLSX_JS)
}

async fn style() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], STYLE)
}

/// The layout version of the snapshot file; `None` when there is none.
async fn snapshot_format(path: &Path) -> Result<Option<u32>> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let head: SnapshotHead =
        serde_json::from_slice(&bytes).with_context(|| format!("decoding {}", path.display()))?;
    Ok(Some(head.format.unwrap_or(0)))
}

/// The snapshot, if its layout matches this server's page. An older one
/// reads as empty (the scheduler replaces it); a newer one means the server
/// must be restarted. Reading never starts a refresh.
async fn snapshot(State(state): State<Arc<AppState>>) -> Response {
    let bytes = match tokio::fs::read(state.cfg.data_file(SNAPSHOT_FILE)).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            return Json(json!({ "empty": true })).into_response();
        }
        Err(e) => {
            error!("reading the snapshot: {e}");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the snapshot could not be read",
            );
        }
    };
    match serde_json::from_slice::<SnapshotHead>(&bytes).map(|h| h.format.unwrap_or(0)) {
        Ok(SNAPSHOT_FORMAT) => {
            ([(header::CONTENT_TYPE, "application/json")], bytes).into_response()
        }
        Ok(older) if older < SNAPSHOT_FORMAT => {
            Json(json!({ "empty": true, "outdated": true })).into_response()
        }
        Ok(_) => problem(
            StatusCode::CONFLICT,
            "the snapshot is newer than this server; restart the dashboard",
        ),
        Err(e) => {
            error!("decoding the snapshot: {e}");
            problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the snapshot could not be read",
            )
        }
    }
}

async fn status(State(state): State<Arc<AppState>>) -> Json<Status> {
    Json(status_of(&state, None).await)
}

async fn refresh(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Status>) {
    let begin = start_refresh(&state);
    (
        StatusCode::ACCEPTED,
        Json(status_of(&state, Some(begin)).await),
    )
}

async fn get_settings(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    Json(json!({
        "refresh_minutes": state.refresh_minutes.load(Ordering::Relaxed),
        "choices": REFRESH_CHOICES,
        "max_note_chars": MAX_NOTE_CHARS,
    }))
}

async fn set_settings(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SettingsBody>,
) -> Response {
    let minutes = body.refresh_minutes;
    if !REFRESH_CHOICES.contains(&minutes) {
        return problem(
            StatusCode::BAD_REQUEST,
            &format!("refresh_minutes must be one of {REFRESH_CHOICES:?}"),
        );
    }
    let _writing = state.file_writes.lock().await;
    let path = state.config_path.clone();
    let existing = match blocking(move || store::read_optional(&path)).await {
        Ok(text) => text,
        Err(e) => {
            error!("{e:#}");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the config file could not be read",
            );
        }
    };
    let text = match config::with_refresh_minutes(existing.as_deref(), minutes) {
        Ok(text) => text,
        Err(e) => {
            warn!("not updating {}: {e:#}", state.config_path.display());
            return problem(
                StatusCode::CONFLICT,
                "the config file is not valid; fix it by hand first",
            );
        }
    };
    let path = state.config_path.clone();
    if let Err(e) = blocking(move || store::write_atomic(&path, text.as_bytes())).await {
        error!("{e:#}");
        return problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the config file could not be written",
        );
    }
    state.refresh_minutes.store(minutes, Ordering::Relaxed);
    Json(json!({ "refresh_minutes": minutes })).into_response()
}

async fn set_commitment(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CommitmentBody>,
) -> Response {
    let key: IssueKey = match body.key.parse() {
        Ok(k) => k,
        Err(e) => return problem(StatusCode::BAD_REQUEST, &e.to_string()),
    };
    let note = body
        .note
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    if note
        .as_ref()
        .is_some_and(|n| n.chars().count() > MAX_NOTE_CHARS)
    {
        return problem(
            StatusCode::BAD_REQUEST,
            &format!("the note is longer than {MAX_NOTE_CHARS} characters"),
        );
    }
    let _writing = state.file_writes.lock().await;
    let path = state.cfg.commitments_file.clone();
    let read = blocking(move || store::read_optional(&path)).await;
    let mut all = match read {
        Ok(None) => Commitments::new(),
        Ok(Some(text)) => match commitments::parse(&text) {
            Ok(all) => all,
            Err(e) => {
                warn!(
                    "not updating {}: {e:#}",
                    state.cfg.commitments_file.display()
                );
                return problem(
                    StatusCode::CONFLICT,
                    "the commitments file is not valid; fix it by hand first",
                );
            }
        },
        Err(e) => {
            error!("{e:#}");
            return problem(
                StatusCode::INTERNAL_SERVER_ERROR,
                "the commitments file could not be read",
            );
        }
    };
    commitments::set(&mut all, key.clone(), body.date, note, Local::now());
    let path = state.cfg.commitments_file.clone();
    let written = match commitments::render(&all) {
        Ok(text) => blocking(move || store::write_atomic(&path, text.as_bytes())).await,
        Err(e) => Err(e),
    };
    if let Err(e) = written {
        error!("writing {}: {e:#}", state.cfg.commitments_file.display());
        return problem(
            StatusCode::INTERNAL_SERVER_ERROR,
            "the commitments file could not be written",
        );
    }
    // The entry as stored, so the page shows it before the next snapshot.
    let committed = all.get(&key).map(|c| Committed {
        date: c.date,
        note: c.note.clone(),
        key,
    });
    Json(json!({
        "saved": true,
        "committed": committed,
        "refresh": start_refresh(&state),
    }))
    .into_response()
}

async fn snapshot_time(path: &Path) -> Option<DateTime<Local>> {
    match tokio::fs::metadata(path).await.and_then(|m| m.modified()) {
        Ok(modified) => Some(DateTime::from(modified)),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => {
            warn!("reading the time of {}: {e}", path.display());
            None
        }
    }
}

/// The snapshot time the schedule goes by: an outdated snapshot counts as none.
fn usable(state: &AppState, snapshot_at: Option<DateTime<Local>>) -> Option<DateTime<Local>> {
    snapshot_at.filter(|_| !state.outdated_snapshot.load(Ordering::Relaxed))
}

async fn status_of(state: &AppState, refresh: Option<Begin>) -> Status {
    let snapshot_at = snapshot_time(&state.cfg.data_file(SNAPSHOT_FILE)).await;
    let minutes = state.refresh_minutes.load(Ordering::Relaxed);
    let now = Local::now();
    let progress = lock(&state.refresh);
    Status {
        refreshing: progress.running,
        queued: progress.rerun,
        finished: progress.finished,
        failed: progress.failed,
        snapshot_at,
        next_refresh: progress
            .next_due(usable(state, snapshot_at), minutes, now)
            .map(|t| t.max(now)),
        interval_minutes: minutes,
        refresh,
    }
}

/// Takes a refresh interval edited by hand in the config file (read off the
/// async thread). Keys the server holds for its lifetime (data, commitments
/// and certificate paths, host, port) take effect at restart, which is
/// logged once per change; the others reach the next refresh by themselves.
fn apply_config(state: &AppState, loaded: Result<Config>) {
    match loaded {
        Ok(cfg) => {
            state
                .refresh_minutes
                .store(cfg.refresh_minutes, Ordering::Relaxed);
            let held = (
                &state.cfg.data_dir,
                &state.cfg.commitments_file,
                &state.cfg.cert_dir,
                state.cfg.host,
                state.cfg.port,
            );
            let warning = (held
                != (
                    &cfg.data_dir,
                    &cfg.commitments_file,
                    &cfg.cert_dir,
                    cfg.host,
                    cfg.port,
                ))
                .then(|| {
                    format!(
                        "{} changed a path, host or port; restart the dashboard to use it",
                        state.config_path.display()
                    )
                });
            warn_once(state, warning);
        }
        Err(e) => warn_once(state, Some(format!("{e:#}; keeping the current settings"))),
    }
}

/// Logs a config warning when it differs from the last one.
fn warn_once(state: &AppState, warning: Option<String>) {
    let mut last = lock(&state.config_warning);
    if *last != warning {
        if let Some(w) = &warning {
            warn!("{w}");
        }
        *last = warning;
    }
}

async fn scheduler(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(SCHEDULER_TICK);
    loop {
        tick.tick().await;
        let (path, named) = (state.config_path.clone(), state.config_named);
        apply_config(
            &state,
            blocking(move || Config::load_from(&path, named)).await,
        );
        let snapshot_at = usable(
            &state,
            snapshot_time(&state.cfg.data_file(SNAPSHOT_FILE)).await,
        );
        let minutes = state.refresh_minutes.load(Ordering::Relaxed);
        if lock(&state.refresh).due(snapshot_at, minutes, Local::now()) {
            start_refresh(&state);
        }
    }
}

/// Replaces the certificate a month before it expires, without a restart. A
/// new certificate that fails to load is loaded again at the next check; the
/// old one is still valid then.
async fn renew_certificate(tls: RustlsConfig, dir: PathBuf) {
    let mut tick = tokio::time::interval(CERT_CHECK);
    tick.tick().await;
    let mut pending: Option<CertPaths> = None;
    loop {
        tick.tick().await;
        let renewed = blocking({
            let dir = dir.clone();
            move || tls::renew_if_due(&dir, Utc::now().date_naive())
        })
        .await;
        match renewed {
            Ok(Some(paths)) => pending = Some(paths),
            Ok(None) => {}
            Err(e) => error!("renewing the certificate: {e:#}"),
        }
        if let Some(paths) = pending.take()
            && let Err(e) = tls.reload_from_pem_file(&paths.cert, &paths.key).await
        {
            error!("loading the renewed certificate: {e}; trying again at the next check");
            pending = Some(paths);
        }
    }
}

/// Starts a worker, or queues one more behind the running one.
fn start_refresh(state: &Arc<AppState>) -> Begin {
    let begin = lock(&state.refresh).begin(Local::now());
    if begin == Begin::Started {
        let state = Arc::clone(state);
        tokio::spawn(async move {
            loop {
                let result = run_worker(&state).await;
                match &result {
                    Ok(()) => state.outdated_snapshot.store(false, Ordering::Relaxed),
                    Err(e) => error!("refresh failed: {e:#}"),
                }
                // Ending and starting the queued one under one lock, so no
                // request in between can start a second worker.
                let again = {
                    let mut progress = lock(&state.refresh);
                    progress.end(Local::now(), result.is_ok())
                        && progress.begin(Local::now()) == Begin::Started
                };
                if !again {
                    break;
                }
            }
        });
    }
    begin
}

/// Runs one refresh on the server's own data directory and commitments file,
/// stopping it after `REFRESH_TIMEOUT`; its log replaces the previous one.
async fn run_worker(state: &AppState) -> Result<()> {
    let dir = &state.cfg.data_dir;
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let log = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(store::OWNER_ONLY)
        .open(dir.join(REFRESH_LOG))
        .await
        .context("opening the refresh log")?
        .into_std()
        .await;
    let mut child = tokio::process::Command::new(&state.worker)
        .arg(REFRESH_COMMAND)
        .arg("--data-dir")
        .arg(dir)
        .arg("--commitments")
        .arg(&state.cfg.commitments_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .context("starting the refresh worker")?;
    let Ok(status) = tokio::time::timeout(REFRESH_TIMEOUT, child.wait()).await else {
        child.kill().await.context("stopping the refresh worker")?;
        bail!(
            "the refresh worker ran past {} minutes and was stopped",
            REFRESH_TIMEOUT.as_secs() / 60
        );
    };
    let status = status.context("waiting for the refresh worker")?;
    if !status.success() {
        bail!(
            "the refresh worker exited with {status}; see {}",
            dir.join(REFRESH_LOG).display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use serde_json::Value;
    use test_case::test_case;
    use tower::ServiceExt;

    const PORT: u16 = 9836;

    struct Fixture {
        dir: tempfile::TempDir,
        state: Arc<AppState>,
        app: Router,
    }

    /// A server whose worker is `worker`; `/usr/bin/true` makes a refresh do nothing.
    fn fixture_with(worker: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            data_dir: dir.path().join("data"),
            commitments_file: dir.path().join("commitments.json"),
            ..Config::default()
        };
        let state = Arc::new(AppState::new(
            cfg,
            PORT,
            (dir.path().join("config.local.json"), false),
            PathBuf::from(worker),
        ));
        Fixture {
            app: router(Arc::clone(&state)),
            state,
            dir,
        }
    }

    fn fixture() -> Fixture {
        fixture_with("/usr/bin/true")
    }

    fn request(
        method: Method,
        uri: &str,
        host: Option<&str>,
        headers: &[(&str, &str)],
        body: Option<&str>,
    ) -> Request {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        builder
            .body(body.map_or_else(Body::empty, |b| Body::from(b.to_string())))
            .unwrap()
    }

    const HOST: Option<&str> = Some("localhost:9836");
    const FROM_PAGE: [(&str, &str); 2] =
        [("x-exa-status", "1"), ("origin", "https://localhost:9836")];

    async fn send(app: &Router, req: Request) -> (StatusCode, String) {
        let response = app.clone().oneshot(req).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn send_json(app: &Router, req: Request) -> (StatusCode, Value) {
        let (code, body) = send(app, req).await;
        (code, serde_json::from_str(&body).unwrap())
    }

    #[tokio::test]
    async fn the_page_and_its_files_are_served_with_security_headers() {
        let f = fixture();

        for (path, content_type) in [
            ("/", "text/html; charset=utf-8"),
            ("/app.js", JS_TYPE),
            ("/xlsx.js", JS_TYPE),
            ("/style.css", "text/css; charset=utf-8"),
        ] {
            let response = f
                .app
                .clone()
                .oneshot(request(Method::GET, path, HOST, &[], None))
                .await
                .unwrap();

            let headers = response.headers();
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(headers.get(header::CONTENT_TYPE).unwrap(), content_type);
            assert_eq!(headers.get(header::CONTENT_SECURITY_POLICY).unwrap(), CSP);
            assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        }
        let at = |file: &str| INDEX.find(file).unwrap();
        assert!(
            at("/xlsx.js") < at("/app.js"),
            "the page loads the writer before the code that uses it"
        );
    }

    #[tokio::test]
    async fn other_hosts_are_refused_and_http2_authority_is_accepted() {
        let f = fixture();

        assert_eq!(
            send(
                &f.app,
                request(Method::GET, "/", Some("evil.example:9836"), &[], None)
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(&f.app, request(Method::GET, "/", None, &[], None))
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(
                &f.app,
                request(
                    Method::GET,
                    "https://127.0.0.1:9836/app.js",
                    None,
                    &[],
                    None
                )
            )
            .await
            .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn changes_need_the_dashboard_header_and_origin() {
        let f = fixture();
        let refresh = |headers: &[(&'static str, &'static str)]| {
            request(Method::POST, "/api/refresh", HOST, headers, None)
        };

        assert_eq!(send(&f.app, refresh(&[])).await.0, StatusCode::FORBIDDEN);
        assert_eq!(
            send(
                &f.app,
                refresh(&[("x-exa-status", "1"), ("origin", "https://evil.example")])
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            send(
                &f.app,
                refresh(&[("x-exa-status", "1"), ("origin", "http://localhost:9836")])
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let (code, body) = send_json(&f.app, refresh(&FROM_PAGE)).await;
        assert_eq!(
            (code, &body["refresh"]),
            (StatusCode::ACCEPTED, &json!("started"))
        );
    }

    #[tokio::test]
    async fn head_requests_are_reads() {
        let f = fixture();

        assert_eq!(
            send(&f.app, request(Method::HEAD, "/", HOST, &[], None))
                .await
                .0,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn a_missing_snapshot_reads_as_empty() {
        let f = fixture();

        assert_eq!(
            send_json(
                &f.app,
                request(Method::GET, "/api/snapshot", HOST, &[], None)
            )
            .await,
            (StatusCode::OK, json!({"empty": true}))
        );
    }

    #[tokio::test]
    async fn a_snapshot_is_served_only_in_this_servers_format_and_reading_never_refreshes() {
        let f = fixture();
        let path = f.dir.path().join("data").join(SNAPSHOT_FILE);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let get = || request(Method::GET, "/api/snapshot", HOST, &[], None);

        std::fs::write(
            &path,
            json!({"format": SNAPSHOT_FORMAT, "tabs": []}).to_string(),
        )
        .unwrap();
        assert_eq!(
            send_json(&f.app, get()).await,
            (
                StatusCode::OK,
                json!({"format": SNAPSHOT_FORMAT, "tabs": []})
            )
        );

        std::fs::write(&path, json!({"tabs": []}).to_string()).unwrap();
        assert_eq!(
            send_json(&f.app, get()).await,
            (StatusCode::OK, json!({"empty": true, "outdated": true}))
        );
        assert!(
            !lock(&f.state.refresh).running,
            "a read does not start a refresh"
        );

        std::fs::write(&path, json!({"format": SNAPSHOT_FORMAT + 1}).to_string()).unwrap();
        assert_eq!(send(&f.app, get()).await.0, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn settings_accept_offered_intervals_and_keep_other_keys() {
        let f = fixture();
        let path = f.dir.path().join("config.local.json");
        std::fs::write(&path, r#"{"port": 9999}"#).unwrap();
        let set = |minutes: &str| {
            request(
                Method::POST,
                "/api/settings",
                HOST,
                &FROM_PAGE,
                Some(minutes),
            )
        };

        assert_eq!(
            send(&f.app, set(r#"{"refresh_minutes": 7}"#)).await.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(&f.app, set(r#"{"refresh_minutes": 15}"#)).await.0,
            StatusCode::OK
        );

        let saved = Config::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!((saved.port, saved.refresh_minutes), (9999, 15));
        let (_, now) = send_json(
            &f.app,
            request(Method::GET, "/api/settings", HOST, &[], None),
        )
        .await;
        assert_eq!(
            now,
            json!({"refresh_minutes": 15, "choices": [0, 15, 30, 60], "max_note_chars": MAX_NOTE_CHARS})
        );
    }

    #[tokio::test]
    async fn a_broken_config_file_is_not_overwritten() {
        let f = fixture();
        let path = f.dir.path().join("config.local.json");
        std::fs::write(&path, "not json").unwrap();

        let (code, _) = send(
            &f.app,
            request(
                Method::POST,
                "/api/settings",
                HOST,
                &FROM_PAGE,
                Some(r#"{"refresh_minutes": 15}"#),
            ),
        )
        .await;

        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn an_interval_edited_by_hand_is_picked_up_and_a_broken_file_is_ignored() {
        let f = fixture();
        let path = f.dir.path().join("config.local.json");

        std::fs::write(&path, r#"{"refresh_minutes": 60}"#).unwrap();
        apply_config(&f.state, Config::load_from(&path, false));
        assert_eq!(f.state.refresh_minutes.load(Ordering::Relaxed), 60);

        std::fs::write(&path, "not json").unwrap();
        apply_config(&f.state, Config::load_from(&path, false));
        assert_eq!(f.state.refresh_minutes.load(Ordering::Relaxed), 60);
        assert!(
            lock(&f.state.config_warning)
                .as_deref()
                .is_some_and(|w| w.ends_with("keeping the current settings"))
        );
    }

    #[test]
    fn a_changed_path_is_reported_as_needing_a_restart() {
        let f = fixture();
        let path = f.dir.path().join("config.local.json");
        let data_dir = f.state.cfg.data_dir.display().to_string();
        let commitments = f.state.cfg.commitments_file.display().to_string();
        let same = json!({"data_dir": data_dir, "commitments_file": commitments});
        let moved = json!({"data_dir": "elsewhere", "commitments_file": commitments});

        std::fs::write(&path, same.to_string()).unwrap();
        apply_config(&f.state, Config::load_from(&path, false));
        assert_eq!(*lock(&f.state.config_warning), None);

        std::fs::write(&path, moved.to_string()).unwrap();
        apply_config(&f.state, Config::load_from(&path, false));
        assert_eq!(
            lock(&f.state.config_warning).clone(),
            Some(format!(
                "{} changed a path, host or port; restart the dashboard to use it",
                path.display()
            ))
        );
    }

    #[tokio::test]
    async fn committed_dates_are_validated_saved_and_cleared() {
        let f = fixture();
        let path = f.dir.path().join("commitments.json");
        let commit = |body: &str| {
            request(
                Method::POST,
                "/api/commitments",
                HOST,
                &FROM_PAGE,
                Some(body),
            )
        };

        assert_eq!(
            send(&f.app, commit(r#"{"key": "../etc", "date": "2026-11-01"}"#))
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            send(
                &f.app,
                commit(r#"{"key": "GCP-315", "date": "2026-13-01"}"#)
            )
            .await
            .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let long = format!(
            r#"{{"key": "GCP-315", "date": "2026-11-01", "note": "{}"}}"#,
            "x".repeat(MAX_NOTE_CHARS + 1)
        );
        assert_eq!(send(&f.app, commit(&long)).await.0, StatusCode::BAD_REQUEST);

        let (code, body) = send_json(
            &f.app,
            commit(r#"{"key": "GCP-315", "date": "2026-11-01", "note": " per QBR "}"#),
        )
        .await;
        assert_eq!((code, &body["saved"]), (StatusCode::OK, &json!(true)));
        assert_eq!(
            body["committed"],
            json!({"date": "2026-11-01", "note": "per QBR", "key": "GCP-315"}),
            "the page gets the entry as stored"
        );

        let saved = commitments::load(&path).unwrap();
        let entry = saved.get(&"GCP-315".parse().unwrap()).unwrap();
        assert_eq!(
            (entry.date.to_string(), entry.note.as_deref()),
            ("2026-11-01".to_string(), Some("per QBR"))
        );

        let (code, cleared) =
            send_json(&f.app, commit(r#"{"key": "GCP-315", "date": null}"#)).await;
        assert_eq!(
            (code, &cleared["committed"]),
            (StatusCode::OK, &Value::Null)
        );

        assert!(commitments::load(&path).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_broken_commitments_file_is_not_overwritten() {
        let f = fixture();
        let path = f.dir.path().join("commitments.json");
        std::fs::write(&path, "[]").unwrap();

        let (code, _) = send(
            &f.app,
            request(
                Method::POST,
                "/api/commitments",
                HOST,
                &FROM_PAGE,
                Some(r#"{"key": "GCP-1", "date": "2026-11-01"}"#),
            ),
        )
        .await;

        assert_eq!(code, StatusCode::CONFLICT);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");
    }

    #[tokio::test]
    async fn a_failing_worker_is_reported_with_its_log() {
        let f = fixture_with("/usr/bin/false");

        let err = run_worker(&f.state).await.unwrap_err().to_string();

        assert!(
            err.starts_with("the refresh worker exited with exit status: 1; see "),
            "{err}"
        );
        assert!(f.dir.path().join("data").join(REFRESH_LOG).exists());
    }

    #[tokio::test]
    async fn an_unreadable_snapshot_is_an_error_not_a_current_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SNAPSHOT_FILE);

        assert_eq!(snapshot_format(&path).await.unwrap(), None);
        std::fs::write(&path, json!({"tabs": []}).to_string()).unwrap();
        assert_eq!(snapshot_format(&path).await.unwrap(), Some(0));
        std::fs::write(&path, "{\"format\": 5, \"tabs\": [").unwrap();
        let err = snapshot_format(&path).await.unwrap_err().to_string();
        assert!(err.starts_with("decoding "), "{err}");
    }

    #[test]
    fn the_page_names_every_verdict() {
        use crate::model::Verdict;
        let all = [
            Verdict::NoDate,
            Verdict::NotStarted,
            Verdict::OnTrack,
            Verdict::AtRisk,
            Verdict::OffTrack,
        ];
        for v in all {
            // A new verdict stops this match compiling until it is listed above.
            match v {
                Verdict::NoDate
                | Verdict::NotStarted
                | Verdict::OnTrack
                | Verdict::AtRisk
                | Verdict::OffTrack => {}
            }
            let name = serde_json::to_value(v).unwrap();
            let name = name.as_str().unwrap();
            assert!(
                APP_JS.contains(&format!("  {name}: [")),
                "static/app.js has no label for {name}"
            );
        }
    }

    #[test]
    fn only_localhost_on_our_port_is_local() {
        assert!(is_local("localhost:9836", PORT));
        assert!(is_local("127.0.0.1:9836", PORT));
        assert!(!is_local("localhost:9835", PORT));
        assert!(!is_local("localhost", PORT));
    }

    #[test]
    fn a_request_during_a_refresh_queues_exactly_one_more() {
        let now = Local::now();
        let mut state = RefreshState::default();

        assert_eq!(state.begin(now), Begin::Started);
        assert_eq!(state.begin(now), Begin::Queued);
        assert_eq!(state.begin(now), Begin::Queued);
        assert!(state.end(now, true), "the queued refresh runs");
        assert!(!state.running && !state.rerun);
        assert!(!state.end(now, false), "nothing else was queued");
        assert!(state.failed);
    }

    fn minutes_ago(n: i64) -> Option<DateTime<Local>> {
        Some(Local::now() - chrono::Duration::minutes(n))
    }

    #[test_case(30, None, None, false, true ; "first start")]
    #[test_case(30, minutes_ago(31), minutes_ago(31), false, true ; "snapshot older than the interval")]
    #[test_case(30, minutes_ago(5), minutes_ago(5), false, false ; "fresh snapshot")]
    #[test_case(30, minutes_ago(45), minutes_ago(10), false, false ; "a failed attempt waits one interval")]
    #[test_case(30, None, None, true, false ; "already running")]
    #[test_case(0, None, None, false, false ; "by hand never refreshes on its own")]
    fn a_refresh_is_due(
        minutes: u64,
        snapshot_at: Option<DateTime<Local>>,
        last_attempt: Option<DateTime<Local>>,
        running: bool,
        expected: bool,
    ) {
        let state = RefreshState {
            running,
            last_attempt,
            ..RefreshState::default()
        };

        assert_eq!(state.due(snapshot_at, minutes, Local::now()), expected);
    }

    #[test]
    fn the_next_refresh_waits_for_the_failed_attempt_too() {
        let now = Local::now();
        let state = RefreshState {
            last_attempt: Some(now - chrono::Duration::minutes(10)),
            ..RefreshState::default()
        };

        let next = state.next_due(Some(now - chrono::Duration::minutes(45)), 30, now);

        assert_eq!(next, Some(now + chrono::Duration::minutes(20)));
        assert_eq!(state.next_due(None, 0, now), None);
    }
}
