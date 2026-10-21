//! The public facade: one [`Svn`] per application, cheap to clone.

use crate::error::{ErrorKind, SvnError};
use crate::job::{CancelToken, Job, Stream};
use crate::model::*;
use crate::parse::{self, ParseError};
use crate::runner::{Inner, JobCtx};
use crate::url::RepoUrl;
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct SvnConfig {
    /// The svn binary (default `svn`, looked up in `PATH`).
    pub svn_binary: PathBuf,
    /// Passed as `--config-dir` when set (tests use a temp dir so they never
    /// touch the user's `~/.subversion`).
    pub config_dir: Option<PathBuf>,
    /// Maximum concurrent svn processes per `scheme://host:port`.
    pub max_concurrent_per_host: usize,
    /// When true, auth/cert failures are sent to [`Svn::auth_prompts`] /
    /// [`Svn::cert_prompts`] and the request waits for an answer. When
    /// false they fail immediately with `ErrorKind::Auth`/`Cert`.
    pub interactive: bool,
    /// Entries per batch for streaming results.
    pub batch_size: usize,
    /// Max total directory entries held by the list cache.
    pub list_cache_entries: usize,
    /// Max bytes held by the cat cache.
    pub cat_cache_bytes: usize,
}

impl Default for SvnConfig {
    fn default() -> Self {
        Self {
            svn_binary: PathBuf::from("svn"),
            config_dir: None,
            max_concurrent_per_host: 4,
            interactive: true,
            batch_size: 1000,
            list_cache_entries: 2_000_000,
            cat_cache_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Scheduling priority: when the per-host limit is reached, higher
/// priorities start first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Speculative prefetch, background tree expansion.
    Background,
    Normal,
    /// What the user is looking at right now.
    Interactive,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// A request for credentials, delivered on [`Svn::auth_prompts`]. All jobs
/// for the same `host_key` wait on a single prompt. Dropping the prompt
/// without responding counts as "cancel".
#[derive(Debug)]
pub struct AuthPrompt {
    /// `scheme://host:port`, e.g. `https://svn.example.com:443`. Credentials
    /// are scoped to this key (also the key for keyring storage).
    pub host_key: String,
    /// The URL whose request failed.
    pub url: RepoUrl,
    /// Username that was rejected, when retrying after a failure.
    pub previous_username: Option<String>,
    /// True if credentials were supplied for this host and got rejected.
    pub rejected: bool,
    pub(crate) reply: async_channel::Sender<Option<Credentials>>,
}

impl AuthPrompt {
    /// `None` cancels: waiting jobs fail with `ErrorKind::Auth`.
    pub fn respond(self, credentials: Option<Credentials>) {
        let _ = self.reply.try_send(credentials);
    }
}

/// A request to trust a server certificate, delivered on
/// [`Svn::cert_prompts`]. Trust lasts for the life of the [`Svn`] only.
#[derive(Debug)]
pub struct CertPrompt {
    /// `host:port`.
    pub host: String,
    pub url: RepoUrl,
    /// svn `--trust-server-cert-failures` names: `unknown-ca`,
    /// `cn-mismatch`, `expired`, `not-yet-valid`, `other`.
    pub failures: Vec<String>,
    /// svn's error message, for display.
    pub message: String,
    pub(crate) reply: async_channel::Sender<bool>,
}

impl CertPrompt {
    pub fn respond(self, trust: bool) {
        let _ = self.reply.try_send(trust);
    }
}

/// Handle to the backend. Clones share caches, credentials and limits.
///
/// Cancellation: dropping a [`Job`]/[`Stream`] or calling `cancel()` kills
/// the svn process within ~10 ms (jobs still queued in the per-host limiter
/// or waiting for a prompt just give up). If the handle is still alive, a
/// final `Err(ErrorKind::Cancelled)` is delivered on a best-effort basis
/// (for streams it may be dropped when the channel is full); callers that
/// cancelled should simply ignore the handle.
///
/// Streams flush a batch when it reaches `batch_size` items or when 50 ms
/// have passed since the previous batch (checked as items arrive), so slow
/// outputs such as a log search show results early.
#[derive(Clone)]
pub struct Svn {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for Svn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Svn").field("config", &self.inner.config).finish()
    }
}

const FLUSH_INTERVAL: Duration = Duration::from_millis(50);

/// Collects stream items into batches and sends them with backpressure.
struct Batcher<T> {
    tx: async_channel::Sender<Result<Vec<T>, SvnError>>,
    token: CancelToken,
    size: usize,
    buf: Vec<T>,
    last_flush: Instant,
    closed: bool,
}

/// Send on a bounded channel, blocking while it is full but giving up when
/// the receiver is gone or the token is cancelled. Returns false if the
/// item was not delivered.
fn send_item<T>(tx: &async_channel::Sender<T>, token: &CancelToken, mut item: T) -> bool {
    loop {
        match tx.try_send(item) {
            Ok(()) => return true,
            Err(async_channel::TrySendError::Closed(_)) => return false,
            Err(async_channel::TrySendError::Full(back)) => {
                if token.is_cancelled() {
                    return false;
                }
                item = back;
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

impl<T> Batcher<T> {
    fn new(tx: async_channel::Sender<Result<Vec<T>, SvnError>>, token: CancelToken, size: usize) -> Self {
        Self {
            tx,
            token,
            size,
            buf: Vec::new(),
            last_flush: Instant::now(),
            closed: false,
        }
    }

    /// Queue an item. Returns false once the receiver is gone (the caller
    /// should stop producing).
    fn push(&mut self, item: T) -> bool {
        if self.closed {
            return false;
        }
        self.buf.push(item);
        if self.buf.len() >= self.size || self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush()
        } else {
            true
        }
    }

    fn flush(&mut self) -> bool {
        if self.closed {
            return false;
        }
        if self.buf.is_empty() {
            return true;
        }
        let batch = std::mem::replace(&mut self.buf, Vec::with_capacity(self.size.min(4096)));
        self.last_flush = Instant::now();
        if !send_item(&self.tx, &self.token, Ok(batch)) {
            self.closed = true;
        }
        !self.closed
    }

    /// Discard unsent items (an attempt failed and will be retried).
    fn reset(&mut self) {
        self.buf.clear();
    }

    fn fail(&mut self, err: SvnError) {
        if self.closed {
            return;
        }
        if self.token.is_cancelled() {
            let _ = self.tx.try_send(Err(SvnError::cancelled()));
        } else {
            send_item(&self.tx, &self.token, Err(err));
        }
    }
}

/// Turn a panic on a worker thread (e.g. a parser bug) into an error
/// instead of silently closing the channel (which would look like a
/// cancellation).
fn catch_panic<T>(f: impl FnOnce() -> Result<T, SvnError>) -> Result<T, SvnError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|p| {
        let msg = p
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| p.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        Err(SvnError::new(ErrorKind::Other, format!("Internal error: {msg}")))
    })
}

fn args<const N: usize>(a: [&str; N]) -> Vec<String> {
    a.iter().map(|s| (*s).to_owned()).collect()
}

/// The svn arguments for a diff, and the URL they run against.
fn diff_args(spec: &DiffSpec) -> (&RepoUrl, Vec<String>) {
    let (url, op, range, peg) = match spec {
        DiffSpec::Change { url, rev } => (url, "-c", rev.to_string(), *rev),
        DiffSpec::ChangeAt { url, peg, rev } => (url, "-c", rev.to_string(), *peg),
        DiffSpec::Range { url, peg, from, to } => (url, "-r", format!("{from}:{to}"), *peg),
    };
    let a = args(["diff", "--internal-diff", op, &range, &url.with_peg(&Rev::Number(peg))]);
    (url, a)
}

fn info_at_args(url: &RepoUrl, peg: RevNum, rev: RevNum) -> Vec<String> {
    args(["info", "--xml", "-r", &rev.to_string(), &url.with_peg(&Rev::Number(peg))])
}

fn parse_failure(what: &str) -> SvnError {
    SvnError::new(ErrorKind::Parse, format!("svn {what} returned no result"))
}

impl Svn {
    pub fn new(config: SvnConfig) -> Self {
        Self {
            inner: Arc::new(Inner::new(config)),
        }
    }

    /// Receiver for credential prompts. The UI runs one loop on this.
    /// All calls return the same channel (clones compete for prompts).
    pub fn auth_prompts(&self) -> async_channel::Receiver<AuthPrompt> {
        self.inner.auth_rx.clone()
    }

    /// Receiver for certificate-trust prompts.
    pub fn cert_prompts(&self) -> async_channel::Receiver<CertPrompt> {
        self.inner.cert_rx.clone()
    }

    /// Pre-load credentials for a host (e.g. from the keyring). `host_key`
    /// is `scheme://host:port`, see [`RepoUrl::host_key`].
    pub fn set_credentials(&self, host_key: &str, credentials: Credentials) {
        self.inner.set_credentials(host_key, credentials);
    }

    fn spawn_job<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Inner, &JobCtx) -> Result<T, SvnError> + Send + 'static,
    ) -> Job<T> {
        let (job, tx, token) = Job::new();
        let inner = self.inner.clone();
        let ctx = inner.job_ctx(token);
        let spawned = std::thread::Builder::new()
            .name("svn-job".into())
            .spawn(move || {
                let r = catch_panic(|| f(&inner, &ctx));
                let _ = tx.try_send(r);
            });
        match spawned {
            Ok(_) => job,
            Err(e) => Job::ready(Err(SvnError::new(
                ErrorKind::Other,
                format!("Cannot spawn worker thread: {e}"),
            ))),
        }
    }

    fn spawn_stream<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Inner, &JobCtx, &mut Batcher<T>) -> Result<(), SvnError> + Send + 'static,
    ) -> Stream<T> {
        let (stream, tx, token) = Stream::new();
        let inner = self.inner.clone();
        let ctx = inner.job_ctx(token);
        let size = inner.config.batch_size;
        let spawned = std::thread::Builder::new()
            .name("svn-stream".into())
            .spawn(move || {
                let mut b = Batcher::new(tx, ctx.token.clone(), size);
                match catch_panic(|| f(&inner, &ctx, &mut b)) {
                    Ok(()) => {
                        b.flush();
                    }
                    Err(e) => b.fail(e),
                }
            });
        if let Err(e) = spawned {
            let (stream, tx, _) = Stream::new();
            let _ = tx.try_send(Err(SvnError::new(
                ErrorKind::Other,
                format!("Cannot spawn worker thread: {e}"),
            )));
            return stream;
        }
        stream
    }

    /// `svn info --xml URL@REV`. Use with `Rev::Head` to resolve HEAD to a
    /// number, or `Rev::Date` to resolve a date.
    pub fn info(&self, url: &RepoUrl, rev: &Rev, prio: Priority) -> Job<Info> {
        if let Rev::Number(n) = rev
            && let Some(hit) = self.inner.cache.get_info(url.as_str(), *n)
        {
            return Job::ready(Ok(hit));
        }
        let url = url.clone();
        let rev = rev.clone();
        self.spawn_job(move |inner, ctx| {
            let a = args(["info", "--xml", &url.with_peg(&rev)]);
            let infos = inner.execute(&url, &a, prio, ctx, |r, _| parse::info(r))?;
            let info = infos.into_iter().next().ok_or_else(|| parse_failure("info"))?;
            // URL@HEAD and URL@N are the same node when N is the operative
            // revision, so a resolved HEAD can be cached under its number.
            inner.cache.put_info(url.as_str(), info.rev, info.clone());
            Ok(info)
        })
    }

    /// `svn list --xml URL@REV` (immediate children only). Cached per
    /// `(url, rev)`; a cache hit streams the cached entries immediately.
    pub fn list(&self, url: &RepoUrl, rev: RevNum, prio: Priority) -> Stream<Entry> {
        if let Some(hit) = self.inner.cache.get_list(url.as_str(), rev) {
            return self.spawn_stream(move |_, _, b| {
                for e in hit.iter() {
                    if !b.push(e.clone()) {
                        break;
                    }
                }
                Ok(())
            });
        }
        let url = url.clone();
        self.spawn_stream(move |inner, ctx, b| {
            let a = args([
                "list",
                "--xml",
                "--depth",
                "immediates",
                &url.with_peg(&Rev::Number(rev)),
            ]);
            let limit = inner.config.list_cache_entries;
            let mut all: Vec<Entry> = Vec::new();
            let mut cacheable = true;
            let mut complete = false;
            inner.execute(&url, &a, prio, ctx, |r, stop| {
                b.reset();
                all.clear();
                cacheable = true;
                let res = parse::list(r, |e| {
                    if stop.is_stopped() {
                        return;
                    }
                    if cacheable {
                        if all.len() < limit {
                            all.push(e.clone());
                        } else {
                            cacheable = false;
                            all = Vec::new();
                        }
                    }
                    if !b.push(e) {
                        stop.stop();
                    }
                });
                complete = res.is_ok() && !stop.is_stopped();
                res
            })?;
            if complete && cacheable && b.flush() {
                inner.cache.put_list(url.as_str(), rev, Arc::new(all));
            }
            Ok(())
        })
    }

    /// `svn log --xml URL@PEG -r START:END ...`, newest first.
    ///
    /// Search terms are passed as `--search T1 --search-and T2 ...`, so an
    /// entry must match every term (each term is a case-insensitive glob
    /// matched anywhere in author, date, message — or changed paths when
    /// `verbose`). Note that with a search, svn applies `-l` to the number of
    /// revisions *examined*, not to the number of matches.
    pub fn log(&self, url: &RepoUrl, peg: RevNum, query: &LogQuery, prio: Priority) -> Stream<LogEntry> {
        let url = url.clone();
        let q = query.clone();
        self.spawn_stream(move |inner, ctx, b| {
            let mut a = args(["log", "--xml", &url.with_peg(&Rev::Number(peg))]);
            a.push("-r".into());
            a.push(format!("{}:{}", q.start.to_svn_arg(), q.end.to_svn_arg()));
            if q.limit > 0 {
                a.push("-l".into());
                a.push(q.limit.to_string());
            }
            for (i, term) in q.search.iter().enumerate() {
                a.push(if i == 0 { "--search" } else { "--search-and" }.into());
                a.push(term.clone());
            }
            if q.stop_on_copy {
                a.push("--stop-on-copy".into());
            }
            if q.verbose {
                a.push("-v".into());
            }
            inner.execute(&url, &a, prio, ctx, |r, stop| {
                b.reset();
                parse::log(r, |e| {
                    if !stop.is_stopped() && !b.push(e) {
                        stop.stop();
                    }
                })
            })
        })
    }

    /// One revision with its changed paths (`svn log -v -r REV`). Pass the
    /// repository root as `url` so the entry exists whatever paths changed.
    /// Cached per `(url, rev)`. Fails with `NotFound` if svn reports no
    /// entry (the path was not changed in `rev`).
    pub fn log_entry(&self, url: &RepoUrl, rev: RevNum, prio: Priority) -> Job<LogEntry> {
        if let Some(hit) = self.inner.cache.get_log_entry(url.as_str(), rev) {
            return Job::ready(Ok(hit));
        }
        let url = url.clone();
        self.spawn_job(move |inner, ctx| {
            let a = args([
                "log",
                "--xml",
                "-v",
                "-r",
                &rev.to_string(),
                &url.with_peg(&Rev::Number(rev)),
            ]);
            let found = inner.execute(&url, &a, prio, ctx, |r, _| {
                let mut found = None;
                parse::log(r, |e| {
                    if found.is_none() || e.rev == rev {
                        found = Some(e);
                    }
                })?;
                Ok(found)
            })?;
            let entry = found.ok_or_else(|| {
                SvnError::new(ErrorKind::NotFound, format!("No log entry for r{rev} at {url}"))
            })?;
            inner.cache.put_log_entry(url.as_str(), rev, entry.clone());
            Ok(entry)
        })
    }

    /// `svn cat URL@REV`. Stops reading after `max_bytes` (setting
    /// `truncated`). Cached per `(url, rev)` when not truncated.
    pub fn cat(&self, url: &RepoUrl, rev: RevNum, max_bytes: Option<u64>, prio: Priority) -> Job<CatOutput> {
        if let Some(hit) = self.inner.cache.get_cat(url.as_str(), rev) {
            let mut data = hit.as_ref().clone();
            let mut truncated = false;
            if let Some(max) = max_bytes
                && data.len() as u64 > max
            {
                data.truncate(max as usize);
                truncated = true;
            }
            return Job::ready(Ok(CatOutput { data, truncated }));
        }
        let url = url.clone();
        self.spawn_job(move |inner, ctx| {
            let a = args(["cat", &url.with_peg(&Rev::Number(rev))]);
            let out = inner.execute(&url, &a, prio, ctx, |r, stop| {
                let mut data = Vec::new();
                let io_err = |e: std::io::Error| ParseError(format!("reading svn output: {e}"));
                match max_bytes {
                    None => {
                        r.read_to_end(&mut data).map_err(io_err)?;
                        Ok(CatOutput { data, truncated: false })
                    }
                    Some(max) => {
                        r.take(max.saturating_add(1))
                            .read_to_end(&mut data)
                            .map_err(io_err)?;
                        let truncated = data.len() as u64 > max;
                        if truncated {
                            data.truncate(max as usize);
                            stop.stop();
                        }
                        Ok(CatOutput { data, truncated })
                    }
                }
            })?;
            if !out.truncated {
                inner.cache.put_cat(url.as_str(), rev, Arc::new(out.data.clone()));
            }
            Ok(out)
        })
    }

    /// `svn proplist -v --xml URL@REV`.
    pub fn proplist(&self, url: &RepoUrl, rev: RevNum, prio: Priority) -> Job<Vec<Property>> {
        let url = url.clone();
        self.spawn_job(move |inner, ctx| {
            let a = args(["proplist", "-v", "--xml", &url.with_peg(&Rev::Number(rev))]);
            inner.execute(&url, &a, prio, ctx, |r, _| parse::proplist(r))
        })
    }

    /// `svn blame --xml URL@REV [-r START:REV]`.
    pub fn blame(&self, url: &RepoUrl, rev: RevNum, start: Option<RevNum>, prio: Priority) -> Stream<BlameLine> {
        let url = url.clone();
        self.spawn_stream(move |inner, ctx, b| {
            let mut a = args(["blame", "--xml"]);
            if let Some(s) = start {
                a.push("-r".into());
                a.push(format!("{s}:{rev}"));
            }
            a.push(url.with_peg(&Rev::Number(rev)));
            inner.execute(&url, &a, prio, ctx, |r, stop| {
                b.reset();
                parse::blame(r, |l| {
                    if !stop.is_stopped() && !b.push(l) {
                        stop.stop();
                    }
                })
            })
        })
    }

    /// `svn diff --internal-diff ...` streamed per file.
    pub fn diff(&self, spec: &DiffSpec, prio: Priority) -> Stream<FileDiff> {
        let spec = spec.clone();
        self.spawn_stream(move |inner, ctx, b| {
            let (url, a) = diff_args(&spec);
            inner.execute(url, &a, prio, ctx, |r, stop| {
                b.reset();
                parse::diff(r, |f| {
                    if !stop.is_stopped() && !b.push(f) {
                        stop.stop();
                    }
                })
            })
        })
    }

    /// `svn info --xml -r REV URL@PEG`: the node that is `url` at `peg`, as
    /// it was at `rev` (`rev <= peg`). svn follows renames and moves, so the
    /// returned [`Info::url`] is the node's URL at `rev`, which may differ
    /// from `url`. Use it to turn a path seen at a newer revision into the
    /// path to view, browse or blame at an older one. Fails with `NotFound`
    /// when the node did not exist yet at `rev`.
    pub fn info_at(&self, url: &RepoUrl, peg: RevNum, rev: RevNum, prio: Priority) -> Job<Info> {
        if peg == rev {
            return self.info(url, &Rev::Number(rev), prio);
        }
        let url = url.clone();
        self.spawn_job(move |inner, ctx| {
            let a = info_at_args(&url, peg, rev);
            let infos = inner.execute(&url, &a, prio, ctx, |r, _| parse::info(r))?;
            let info = infos.into_iter().next().ok_or_else(|| parse_failure("info"))?;
            // The result describes `info.url@rev`, so cache it under that.
            inner.cache.put_info(info.url.as_str(), info.rev, info.clone());
            Ok(info)
        })
    }

    /// Drop all cached data (HEAD-relative results are never cached, so this
    /// is only needed to free memory).
    pub fn clear_cache(&self) {
        self.inner.cache.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> RepoUrl {
        RepoUrl::parse(s).unwrap()
    }

    #[test]
    fn diff_argument_construction() {
        let u = url("http://h/r/trunk/new%20name.c");
        let change = DiffSpec::Change { url: u.clone(), rev: 8 };
        let (target, a) = diff_args(&change);
        assert_eq!(target, &u);
        assert_eq!(a, ["diff", "--internal-diff", "-c", "8", "http://h/r/trunk/new%20name.c@8"]);

        let at = DiffSpec::ChangeAt { url: u.clone(), peg: 50, rev: 8 };
        let (target, a) = diff_args(&at);
        assert_eq!(target, &u);
        assert_eq!(a, ["diff", "--internal-diff", "-c", "8", "http://h/r/trunk/new%20name.c@50"]);

        let range = DiffSpec::Range { url: u.clone(), peg: 50, from: 3, to: 9 };
        let (_, a) = diff_args(&range);
        assert_eq!(a, ["diff", "--internal-diff", "-r", "3:9", "http://h/r/trunk/new%20name.c@50"]);
    }

    #[test]
    fn info_at_argument_construction() {
        let u = url("http://h/r/trunk/doc");
        assert_eq!(
            info_at_args(&u, 50, 20),
            ["info", "--xml", "-r", "20", "http://h/r/trunk/doc@50"]
        );
    }
}
