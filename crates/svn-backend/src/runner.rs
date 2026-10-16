//! Process runner: spawning svn, cancellation, per-host concurrency limits,
//! auth/cert prompts and error classification (DESIGN.md §3.1, §9, §12).
//!
//! Every operation runs on its own worker thread (see `svn.rs`). The worker
//! calls [`Inner::execute`], which
//!
//! 1. waits for a slot in the per-host limiter (by priority, cancellable),
//! 2. spawns `svn` in its own process group with `--non-interactive`,
//!    `LC_ALL=C` and, if known, stored credentials / trusted cert failures,
//! 3. hands stdout to a consumer (normally a `crate::parse` function) while a
//!    second thread drains stderr and a watcher thread kills the process group
//!    as soon as the job's [`CancelToken`] is set,
//! 4. on failure classifies stderr and, for auth/cert errors in interactive
//!    mode, coordinates a single prompt per host and retries.

use crate::cache::Cache;
use crate::error::{ErrorKind, SvnError};
use crate::job::CancelToken;
use crate::parse::ParseError;
use crate::svn::{AuthPrompt, CertPrompt, Credentials, Priority, SvnConfig};
use crate::url::RepoUrl;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::Duration;

/// How often blocked waits (limiter queue, prompts, process watcher) look at
/// the cancel token. Bounds cancellation latency.
const POLL: Duration = Duration::from_millis(10);
/// Max prompts a single job shows before giving up.
const MAX_PROMPTS_PER_JOB: u32 = 3;
/// Max auth/cert retries for a single job (guards against loops).
const MAX_RETRIES: u32 = 8;
/// Stderr kept per process.
const MAX_STDERR: usize = 256 * 1024;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Shared state behind [`crate::Svn`].
pub(crate) struct Inner {
    pub(crate) config: SvnConfig,
    pub(crate) cache: Cache,
    limiter: Limiter,
    hosts: Mutex<HashMap<String, HostState>>,
    hosts_cv: Condvar,
    auth_tx: async_channel::Sender<AuthPrompt>,
    pub(crate) auth_rx: async_channel::Receiver<AuthPrompt>,
    cert_tx: async_channel::Sender<CertPrompt>,
    pub(crate) cert_rx: async_channel::Receiver<CertPrompt>,
}

/// Per-host auth and cert-trust state.
#[derive(Default)]
struct HostState {
    creds: Option<Credentials>,
    /// Bumped whenever `creds` change.
    auth_gen: u64,
    auth_prompting: bool,
    /// Bumped whenever a prompt is declined, so jobs waiting on that prompt
    /// fail instead of prompting again.
    auth_declines: u64,
    /// Accepted `--trust-server-cert-failures` names.
    trusted: Vec<String>,
    cert_gen: u64,
    cert_prompting: bool,
    cert_declines: u64,
}

/// What an attempt ran with, to detect stale credentials/trust afterwards.
#[derive(Clone)]
struct Snapshot {
    creds: Option<Credentials>,
    auth_gen: u64,
    trusted: Vec<String>,
    cert_gen: u64,
}

enum Decision {
    Retry,
    Fail,
    Cancelled,
}

/// Handed to stdout consumers so they can end the process early (output
/// truncated at a byte limit, or the result receiver went away). A stopped
/// run returns the consumer's result regardless of svn's exit status.
pub(crate) struct Stopper {
    proc: Arc<Proc>,
}

impl Stopper {
    pub(crate) fn stop(&self) {
        self.proc.stopped.store(true, Ordering::SeqCst);
        self.proc.kill();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.proc.stopped.load(Ordering::SeqCst)
    }
}

/// A running svn child. Killing targets the whole process group (svn is
/// spawned as a group leader) so helpers such as ssh tunnels die too and
/// nothing keeps our stdout pipe open.
struct Proc {
    child: Mutex<ProcChild>,
    pid: i32,
    stopped: AtomicBool,
}

struct ProcChild {
    child: Child,
    status: Option<ExitStatus>,
}

impl Proc {
    fn kill(&self) {
        let st = lock(&self.child);
        if st.status.is_none() {
            // SAFETY: plain syscall. The group cannot have been recycled
            // because the leader is not reaped yet (we hold the lock that
            // guards reaping).
            unsafe {
                libc::kill(-self.pid, libc::SIGKILL);
            }
        }
    }

    fn try_wait(&self) -> io::Result<Option<ExitStatus>> {
        let mut st = lock(&self.child);
        if let Some(s) = st.status {
            return Ok(Some(s));
        }
        let r = st.child.try_wait()?;
        st.status = r;
        Ok(r)
    }

    /// Wait for exit, killing the child if `token` is cancelled meanwhile.
    fn wait(&self, token: &CancelToken) -> io::Result<ExitStatus> {
        let mut delay = Duration::from_millis(1);
        loop {
            if let Some(s) = self.try_wait()? {
                return Ok(s);
            }
            if token.is_cancelled() {
                self.kill();
            }
            thread::sleep(delay);
            delay = (delay * 2).min(POLL);
        }
    }
}

impl Inner {
    pub(crate) fn new(mut config: SvnConfig) -> Self {
        config.max_concurrent_per_host = config.max_concurrent_per_host.max(1);
        config.batch_size = config.batch_size.max(1);
        let (auth_tx, auth_rx) = async_channel::unbounded();
        let (cert_tx, cert_rx) = async_channel::unbounded();
        Self {
            cache: Cache::new(config.list_cache_entries, config.cat_cache_bytes),
            limiter: Limiter::new(config.max_concurrent_per_host),
            config,
            hosts: Mutex::new(HashMap::new()),
            hosts_cv: Condvar::new(),
            auth_tx,
            auth_rx,
            cert_tx,
            cert_rx,
        }
    }

    pub(crate) fn set_credentials(&self, host_key: &str, creds: Credentials) {
        let mut hosts = lock(&self.hosts);
        let st = hosts.entry(host_key.to_owned()).or_default();
        st.creds = Some(creds);
        st.auth_gen += 1;
        self.hosts_cv.notify_all();
    }

    fn snapshot(&self, host: &str) -> Snapshot {
        let hosts = lock(&self.hosts);
        match hosts.get(host) {
            Some(st) => Snapshot {
                creds: st.creds.clone(),
                auth_gen: st.auth_gen,
                trusted: st.trusted.clone(),
                cert_gen: st.cert_gen,
            },
            None => Snapshot {
                creds: None,
                auth_gen: 0,
                trusted: Vec::new(),
                cert_gen: 0,
            },
        }
    }

    /// Run `svn <args>` for `url` and feed its stdout to `consume`.
    ///
    /// `consume` may be called once per attempt (auth/cert retries), so it
    /// must reset any state it accumulates. Returns `Cancelled` when `token`
    /// is set before the result is known.
    pub(crate) fn execute<T>(
        &self,
        url: &RepoUrl,
        args: &[String],
        prio: Priority,
        token: &CancelToken,
        mut consume: impl FnMut(&mut dyn BufRead, &Stopper) -> Result<T, ParseError>,
    ) -> Result<T, SvnError> {
        let host = url.host_key();
        let mut prompts = 0;
        let mut retries = 0;
        loop {
            if token.is_cancelled() {
                return Err(SvnError::cancelled());
            }
            let snap = self.snapshot(&host);
            let result = {
                let Some(_permit) = self.limiter.acquire(&host, prio, token) else {
                    return Err(SvnError::cancelled());
                };
                self.run_process(args, &snap, token, &mut consume)
            };
            let err = match result {
                Ok(v) => return Ok(v),
                Err(e) => e,
            };
            if !self.config.interactive || retries >= MAX_RETRIES {
                return Err(err);
            }
            let decision = match err.kind {
                ErrorKind::Auth => self.await_auth(&host, url, &snap, token, &mut prompts),
                ErrorKind::Cert => self.await_cert(&host, url, &snap, &err, token, &mut prompts),
                _ => return Err(err),
            };
            match decision {
                Decision::Retry => retries += 1,
                Decision::Fail => return Err(err),
                Decision::Cancelled => return Err(SvnError::cancelled()),
            }
        }
    }

    /// After an auth failure with `snap`: wait for (or show) the single
    /// outstanding prompt for `host`.
    fn await_auth(
        &self,
        host: &str,
        url: &RepoUrl,
        snap: &Snapshot,
        token: &CancelToken,
        prompts: &mut u32,
    ) -> Decision {
        let mut hosts = lock(&self.hosts);
        let declines_at_start = hosts.entry(host.to_owned()).or_default().auth_declines;
        loop {
            if token.is_cancelled() {
                return Decision::Cancelled;
            }
            let st = hosts.entry(host.to_owned()).or_default();
            if st.auth_gen != snap.auth_gen {
                // Someone supplied newer credentials meanwhile.
                return Decision::Retry;
            }
            if st.auth_declines != declines_at_start {
                // The prompt we were waiting on was declined.
                return Decision::Fail;
            }
            if st.auth_prompting {
                hosts = self
                    .hosts_cv
                    .wait_timeout(hosts, POLL)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
                continue;
            }
            if *prompts >= MAX_PROMPTS_PER_JOB {
                return Decision::Fail;
            }
            // We are the prompter.
            *prompts += 1;
            st.auth_prompting = true;
            drop(hosts);

            let (reply_tx, reply_rx) = async_channel::bounded(1);
            let prompt = AuthPrompt {
                host_key: host.to_owned(),
                url: url.clone(),
                previous_username: snap.creds.as_ref().map(|c| c.username.clone()),
                rejected: snap.creds.is_some(),
                reply: reply_tx,
            };
            let answer = if self.auth_tx.try_send(prompt).is_ok() {
                wait_reply(&reply_rx, token)
            } else {
                Some(None)
            };

            hosts = lock(&self.hosts);
            let st = hosts.entry(host.to_owned()).or_default();
            st.auth_prompting = false;
            self.hosts_cv.notify_all();
            return match answer {
                None => Decision::Cancelled,
                Some(Some(creds)) => {
                    st.creds = Some(creds);
                    st.auth_gen += 1;
                    Decision::Retry
                }
                Some(None) => {
                    st.auth_declines += 1;
                    Decision::Fail
                }
            };
        }
    }

    /// After a certificate failure: wait for (or show) the single
    /// outstanding cert prompt for `host`.
    fn await_cert(
        &self,
        host: &str,
        url: &RepoUrl,
        snap: &Snapshot,
        err: &SvnError,
        token: &CancelToken,
        prompts: &mut u32,
    ) -> Decision {
        let failures = cert_failures(&err.message);
        let mut hosts = lock(&self.hosts);
        let declines_at_start = hosts.entry(host.to_owned()).or_default().cert_declines;
        loop {
            if token.is_cancelled() {
                return Decision::Cancelled;
            }
            let st = hosts.entry(host.to_owned()).or_default();
            if st.cert_gen != snap.cert_gen {
                return Decision::Retry;
            }
            if st.cert_declines != declines_at_start {
                return Decision::Fail;
            }
            if st.cert_prompting {
                hosts = self
                    .hosts_cv
                    .wait_timeout(hosts, POLL)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
                continue;
            }
            if *prompts >= MAX_PROMPTS_PER_JOB || failures.iter().all(|f| st.trusted.contains(f)) {
                // Already trusting everything svn complained about: retrying
                // would not help.
                return Decision::Fail;
            }
            *prompts += 1;
            st.cert_prompting = true;
            drop(hosts);

            let (reply_tx, reply_rx) = async_channel::bounded(1);
            let prompt = CertPrompt {
                host: host
                    .split_once("://")
                    .map_or(host, |(_, h)| h)
                    .to_owned(),
                url: url.clone(),
                failures: failures.clone(),
                message: err.message.clone(),
                reply: reply_tx,
            };
            let answer = if self.cert_tx.try_send(prompt).is_ok() {
                wait_reply(&reply_rx, token)
            } else {
                Some(false)
            };

            hosts = lock(&self.hosts);
            let st = hosts.entry(host.to_owned()).or_default();
            st.cert_prompting = false;
            self.hosts_cv.notify_all();
            return match answer {
                None => Decision::Cancelled,
                Some(true) => {
                    for f in failures {
                        if !st.trusted.contains(&f) {
                            st.trusted.push(f);
                        }
                    }
                    st.cert_gen += 1;
                    Decision::Retry
                }
                Some(false) => {
                    st.cert_declines += 1;
                    Decision::Fail
                }
            };
        }
    }

    /// One svn invocation.
    fn run_process<T>(
        &self,
        args: &[String],
        snap: &Snapshot,
        token: &CancelToken,
        consume: &mut impl FnMut(&mut dyn BufRead, &Stopper) -> Result<T, ParseError>,
    ) -> Result<T, SvnError> {
        let mut full: Vec<String> = vec!["--non-interactive".into()];
        if let Some(dir) = &self.config.config_dir {
            full.push("--config-dir".into());
            full.push(dir.to_string_lossy().into_owned());
        }
        if let Some(c) = &snap.creds {
            full.push("--username".into());
            full.push(c.username.clone());
            full.push("--password-from-stdin".into());
            full.push("--no-auth-cache".into());
        }
        if !snap.trusted.is_empty() {
            full.push(format!("--trust-server-cert-failures={}", snap.trusted.join(",")));
        }
        full.extend(args.iter().cloned());
        let command = command_line(&self.config.svn_binary.to_string_lossy(), &full);
        let fail = |kind: ErrorKind, message: String, stderr: String| SvnError {
            kind,
            code: None,
            message,
            command: command.clone(),
            stderr,
        };

        let mut cmd = Command::new(&self.config.svn_binary);
        cmd.args(&full)
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env_remove("LANGUAGE")
            .stdin(if snap.creds.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(fail(
                    ErrorKind::SvnNotFound,
                    format!("Cannot run '{}': {e}", self.config.svn_binary.display()),
                    String::new(),
                ));
            }
            Err(e) => {
                return Err(fail(
                    ErrorKind::Other,
                    format!("Cannot run '{}': {e}", self.config.svn_binary.display()),
                    String::new(),
                ));
            }
        };
        if let (Some(c), Some(mut stdin)) = (&snap.creds, child.stdin.take()) {
            // Small write into an empty pipe: does not block. EPIPE (svn
            // exited early) is fine; the exit status tells the story.
            let mut buf = Vec::with_capacity(c.password.len() + 1);
            buf.extend_from_slice(c.password.as_bytes());
            buf.push(b'\n');
            let _ = stdin.write_all(&buf);
        }
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let pid = child.id() as i32;
        let proc = Arc::new(Proc {
            child: Mutex::new(ProcChild {
                child,
                status: None,
            }),
            pid,
            stopped: AtomicBool::new(false),
        });

        // stderr reader: avoids pipe deadlock when svn writes lots of warnings.
        let (err_tx, err_rx) = std::sync::mpsc::channel();
        thread::Builder::new()
            .name("svn-stderr".into())
            .spawn(move || {
                let mut stderr = stderr;
                let mut buf = Vec::new();
                let _ = (&mut stderr).take(MAX_STDERR as u64).read_to_end(&mut buf);
                let _ = io::copy(&mut stderr, &mut io::sink());
                let _ = err_tx.send(buf);
            })
            .map_err(|e| fail(ErrorKind::Other, format!("Cannot spawn thread: {e}"), String::new()))?;

        // Watcher: kills the process group promptly on cancellation.
        let finished = Arc::new(AtomicBool::new(false));
        let watcher = {
            let proc = proc.clone();
            let finished = finished.clone();
            let token = token.clone();
            thread::Builder::new().name("svn-watch".into()).spawn(move || {
                while !finished.load(Ordering::SeqCst) {
                    if token.is_cancelled() {
                        proc.kill();
                        return;
                    }
                    thread::sleep(POLL);
                }
            })
        };

        let stopper = Stopper { proc: proc.clone() };
        let mut reader = BufReader::with_capacity(64 * 1024, stdout);
        let result = consume(&mut reader, &stopper);
        if result.is_err() || stopper.is_stopped() || token.is_cancelled() {
            proc.kill();
        } else {
            // Drain anything the parser didn't need so svn can exit.
            let _ = io::copy(&mut reader, &mut io::sink());
        }
        drop(reader);
        let status = proc.wait(token);
        finished.store(true, Ordering::SeqCst);
        if let Ok(w) = watcher {
            let _ = w.join();
        }
        // The group is dead or exited, so stderr hits EOF promptly; the
        // timeout only guards against a stray grandchild holding it open.
        let stderr = err_rx
            .recv_timeout(Duration::from_secs(2))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_default();

        if token.is_cancelled() {
            return Err(SvnError {
                command,
                stderr,
                ..SvnError::cancelled()
            });
        }
        let parse_err = |e: ParseError, stderr: String| SvnError {
            kind: ErrorKind::Parse,
            code: None,
            message: e.0,
            command: command.clone(),
            stderr,
        };
        if stopper.is_stopped() {
            return result.map_err(|e| parse_err(e, stderr));
        }
        let status = status.map_err(|e| fail(ErrorKind::Other, format!("wait failed: {e}"), stderr.clone()))?;
        if status.success() {
            return result.map_err(|e| parse_err(e, stderr));
        }
        let (kind, code, message) = classify_stderr(&stderr);
        let message = if message.is_empty() {
            format!("svn failed ({status})")
        } else {
            message
        };
        Err(SvnError {
            kind,
            code,
            message,
            command,
            stderr,
        })
    }
}

/// Wait for a prompt reply. `None` = cancelled; `Some(answer)`. A dropped
/// prompt (sender gone without reply) counts as `T::default()`.
fn wait_reply<T: Default>(rx: &async_channel::Receiver<T>, token: &CancelToken) -> Option<T> {
    loop {
        match rx.try_recv() {
            Ok(v) => return Some(v),
            Err(async_channel::TryRecvError::Closed) => return Some(T::default()),
            Err(async_channel::TryRecvError::Empty) => {}
        }
        if token.is_cancelled() {
            return None;
        }
        thread::sleep(POLL);
    }
}

/// Shell-like rendering of the command for error details. Passwords are
/// never passed in argv; credentials embedded in URLs are masked anyway.
fn command_line(bin: &str, args: &[String]) -> String {
    let mut out = quote(bin);
    for a in args {
        out.push(' ');
        out.push_str(&quote(&redact_url(a)));
    }
    out
}

fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./:@%+=,{}~".contains(&b));
    if plain {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// `scheme://user:pass@host/...` → `scheme://user:***@host/...`.
fn redact_url(s: &str) -> String {
    if let Some(i) = s.find("://") {
        let rest = &s[i + 3..];
        let auth_end = rest.find('/').unwrap_or(rest.len());
        let authority = &rest[..auth_end];
        if let Some(at) = authority.rfind('@')
            && let Some(colon) = authority[..at].find(':')
        {
            return format!(
                "{}{}:***{}",
                &s[..i + 3],
                &authority[..colon],
                &s[i + 3 + at..]
            );
        }
    }
    s.to_owned()
}

/// Map svn's certificate error text to `--trust-server-cert-failures` names.
pub(crate) fn cert_failures(message: &str) -> Vec<String> {
    let m = message.to_ascii_lowercase();
    let mut out = Vec::new();
    if m.contains("issuer is not trusted") {
        out.push("unknown-ca");
    }
    if m.contains("hostname") {
        out.push("cn-mismatch");
    }
    if m.contains("expired") {
        out.push("expired");
    }
    if m.contains("not yet valid") {
        out.push("not-yet-valid");
    }
    if out.is_empty() {
        out.push("other");
    }
    out.into_iter().map(str::to_owned).collect()
}

// ---------------------------------------------------------------------------
// Per-host concurrency limiter

struct Limiter {
    max: usize,
    state: Mutex<LimState>,
    cv: Condvar,
}

#[derive(Default)]
struct LimState {
    hosts: HashMap<String, HostQueue>,
    seq: u64,
}

#[derive(Default)]
struct HostQueue {
    running: usize,
    /// `(priority, ticket)`; lower ticket = queued earlier.
    waiting: Vec<(Priority, u64)>,
}

impl HostQueue {
    fn best(&self) -> Option<u64> {
        self.waiting
            .iter()
            .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
            .map(|w| w.1)
    }
}

struct Permit<'a> {
    limiter: &'a Limiter,
    host: String,
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut st = lock(&self.limiter.state);
        if let Some(q) = st.hosts.get_mut(&self.host) {
            q.running -= 1;
            if q.running == 0 && q.waiting.is_empty() {
                st.hosts.remove(&self.host);
            }
        }
        self.limiter.cv.notify_all();
    }
}

impl Limiter {
    fn new(max: usize) -> Self {
        Self {
            max,
            state: Mutex::default(),
            cv: Condvar::new(),
        }
    }

    /// Wait for a slot: highest priority first, FIFO within a priority.
    /// `None` if cancelled while queued.
    fn acquire(&self, host: &str, prio: Priority, token: &CancelToken) -> Option<Permit<'_>> {
        let mut st = lock(&self.state);
        st.seq += 1;
        let ticket = st.seq;
        st.hosts
            .entry(host.to_owned())
            .or_default()
            .waiting
            .push((prio, ticket));
        loop {
            let q = st.hosts.get_mut(host).expect("queue exists while waiting");
            if token.is_cancelled() {
                q.waiting.retain(|w| w.1 != ticket);
                if q.running == 0 && q.waiting.is_empty() {
                    st.hosts.remove(host);
                }
                self.cv.notify_all();
                return None;
            }
            if q.running < self.max && q.best() == Some(ticket) {
                q.waiting.retain(|w| w.1 != ticket);
                q.running += 1;
                // Another slot may still be free for the next waiter.
                self.cv.notify_all();
                return Some(Permit {
                    limiter: self,
                    host: host.to_owned(),
                });
            }
            st = self
                .cv
                .wait_timeout(st, POLL)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

// ---------------------------------------------------------------------------
// Error classification

struct ErrLine<'a> {
    code: u32,
    message: &'a str,
    warning: bool,
}

const NOT_FOUND: &[u32] = &[
    2,      // ENOENT, e.g. "Could not find the requested SVN filesystem"
    160005, // invalid filesystem path syntax
    160006, // no such revision
    160013, // path not found
    170000, // illegal repository URL / non-existent in revision
    175007, // HTTP path not found
    180001, // unable to open repository (file://)
    195012, // unable to find repository location
];
const AUTH: &[u32] = &[
    170001, // authorization failed
    175013, // access forbidden (403)
    215004, // no more credentials / authentication failed
    220001, // item is not readable (authz)
];
const CERT: &[u32] = &[230001];
const NETWORK: &[u32] = &[
    101, 104, 110, 111, 113, // ENETUNREACH, ECONNRESET, ETIMEDOUT, ECONNREFUSED, EHOSTUNREACH
    120108, // serf: connection closed
    120171, // serf: SSL communication error
    170013, // unable to connect (generic outer error)
    175002, // HTTP request failed / unexpected status
    175012, // timeout
    210002, // svn:// network error
    670002, // DNS: name or service not known
    670008, // DNS: temporary failure / no address
];

fn parse_lines(stderr: &str) -> Vec<ErrLine<'_>> {
    let mut out = Vec::new();
    for line in stderr.lines() {
        let Some(rest) = line.trim_start().strip_prefix("svn: ") else {
            continue;
        };
        let (rest, warning) = match rest.strip_prefix("warning: ") {
            Some(r) => (r, true),
            None => (rest, false),
        };
        let mut chars = rest.chars();
        let lead = chars.next();
        if !matches!(lead, Some('E' | 'W')) || rest.len() < 8 {
            continue;
        }
        let digits = &rest[1..7];
        if !digits.bytes().all(|b| b.is_ascii_digit()) || rest.as_bytes()[7] != b':' {
            continue;
        }
        let code = digits.parse().unwrap_or(0);
        out.push(ErrLine {
            code,
            message: rest[8..].trim(),
            warning: warning || lead == Some('W'),
        });
    }
    out
}

fn looks_like_auth_text(stderr: &str) -> bool {
    stderr.contains("Authorization failed")
        || stderr.contains("Authentication failed")
        || stderr.contains("401 Unauthorized")
        || stderr.contains("403 Forbidden")
        || stderr.contains("status 401")
        || stderr.contains("status 403")
}

/// Classify svn stderr: `(kind, most specific E-code, message line)`.
///
/// Kinds are checked from most to least specific — Cert, Auth, NotFound,
/// Network — and within a kind the last (innermost) matching line wins.
/// `E200009` ("could not ... all targets") counts as NotFound when svn says
/// targets "don't exist"; the preceding `W160013`/`W170000` warning then
/// provides the code and message.
pub(crate) fn classify_stderr(stderr: &str) -> (ErrorKind, Option<u32>, String) {
    let lines = parse_lines(stderr);
    let last_of = |set: &[u32]| lines.iter().rev().find(|l| set.contains(&l.code));
    let pick = |kind: ErrorKind, l: &ErrLine| (kind, Some(l.code), l.message.to_owned());

    if let Some(l) = last_of(CERT) {
        return pick(ErrorKind::Cert, l);
    }
    if let Some(l) = last_of(AUTH) {
        return pick(ErrorKind::Auth, l);
    }
    if looks_like_auth_text(stderr) {
        let l = lines.iter().rev().find(|l| !l.warning);
        return (
            ErrorKind::Auth,
            l.map(|l| l.code),
            l.map_or_else(|| "Authentication failed".to_owned(), |l| l.message.to_owned()),
        );
    }
    if let Some(l) = last_of(NOT_FOUND) {
        return pick(ErrorKind::NotFound, l);
    }
    if let Some(l) = lines.iter().rev().find(|l| l.code == 200009)
        && (l.message.contains("don't exist") || l.message.contains("non-existent"))
    {
        return pick(ErrorKind::NotFound, l);
    }
    if lines.iter().any(|l| l.code == 200015) {
        return (ErrorKind::Cancelled, Some(200015), "Cancelled".to_owned());
    }
    if let Some(l) = last_of(NETWORK) {
        // Prefer the inner cause over the generic "Unable to connect".
        let inner = lines
            .iter()
            .rev()
            .find(|l| NETWORK.contains(&l.code) && l.code != 170013)
            .unwrap_or(l);
        return pick(ErrorKind::Network, inner);
    }
    // Unknown: the last real error line is usually the most specific, but a
    // bare "Illegal target" after a descriptive warning reads better with
    // the warning.
    if let Some(l) = lines.iter().rev().find(|l| !l.warning) {
        if let Some(w) = lines.iter().find(|l| l.warning) {
            return pick(ErrorKind::Other, w);
        }
        return pick(ErrorKind::Other, l);
    }
    if let Some(l) = lines.last() {
        return pick(ErrorKind::Other, l);
    }
    let first = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    (ErrorKind::Other, None, first.trim_start_matches("svn: ").to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cls(s: &str) -> (ErrorKind, Option<u32>) {
        let (k, c, _) = classify_stderr(s);
        (k, c)
    }

    // All samples below were captured from svn 1.14.5 with LC_ALL=C.

    #[test]
    fn bad_host() {
        let s = "svn: E170013: Unable to connect to a repository at URL 'http://nonexistent.invalid/repo'\n\
                 svn: E670002: Name or service not known\n";
        assert_eq!(cls(s), (ErrorKind::Network, Some(670002)));
        assert_eq!(classify_stderr(s).2, "Name or service not known");
    }

    #[test]
    fn connection_refused() {
        let s = "svn: E170013: Unable to connect to a repository at URL 'http://localhost:1/repo'\n\
                 svn: E000111: Error running context: Connection refused\n";
        assert_eq!(cls(s), (ErrorKind::Network, Some(111)));
        let s = "svn: E170013: Unable to connect to a repository at URL 'svn://localhost:1/repo'\n\
                 svn: E000111: Can't connect to host 'localhost': Connection refused\n";
        assert_eq!(cls(s), (ErrorKind::Network, Some(111)));
    }

    #[test]
    fn timeout() {
        let s = "svn: E170013: Unable to connect to a repository at URL 'http://10.255.255.1/repo'\n\
                 svn: E175012: Connection timed out\n";
        assert_eq!(cls(s), (ErrorKind::Network, Some(175012)));
    }

    #[test]
    fn ssl_protocol_error_and_bad_status() {
        let s = "svn: E170013: Unable to connect to a repository at URL 'https://localhost:8080/svn/small'\n\
                 svn: E120171: Error running context: An error occurred during SSL communication\n";
        assert_eq!(cls(s), (ErrorKind::Network, Some(120171)));
        let s = "svn: E170013: Unable to connect to a repository at URL 'http://localhost:8443/svn/small'\n\
                 svn: E175002: Unexpected HTTP status 400 'Bad Request' on '/svn/small'\n";
        assert_eq!(cls(s), (ErrorKind::Network, Some(175002)));
    }

    #[test]
    fn unable_to_connect_alone_is_network() {
        let s = "svn: E170013: Unable to connect to a repository at URL 'http://x/'\n";
        assert_eq!(cls(s), (ErrorKind::Network, Some(170013)));
    }

    #[test]
    fn no_such_path() {
        let s = "svn: warning: W170000: URL 'https://svn.apache.org/repos/asf/nonexistent-path-xyz' non-existent in revision 1939142\n\
                 \n\
                 svn: E200009: Could not display info for all targets because some targets don't exist\n";
        let (k, c, m) = classify_stderr(s);
        assert_eq!((k, c), (ErrorKind::NotFound, Some(170000)));
        assert!(m.contains("non-existent in revision"));
        let s = "svn: warning: W160013: '/repos/asf/!svn/rvr/1939142/nonexistent-path-xyz' path not found\n\
                 svn: E200009: Could not list all targets because some targets don't exist\n";
        assert_eq!(cls(s), (ErrorKind::NotFound, Some(160013)));
        let s = "svn: E160013: '/repos/asf/!svn/rvr/1939142/nonexistent-path-xyz' path not found\n";
        assert_eq!(cls(s), (ErrorKind::NotFound, Some(160013)));
    }

    #[test]
    fn no_such_revision_and_repo() {
        assert_eq!(
            cls("svn: E160006: No such revision 99999999\n"),
            (ErrorKind::NotFound, Some(160006))
        );
        let s = "svn: E170013: Unable to connect to a repository at URL 'file:///nonexistent/repo'\n\
                 svn: E180001: Unable to open repository 'file:///nonexistent/repo'\n";
        assert_eq!(cls(s), (ErrorKind::NotFound, Some(180001)));
        let s = "svn: E170013: Unable to connect to a repository at URL 'http://localhost:8080/svn/nonexistentrepo'\n\
                 svn: E000002: Could not find the requested SVN filesystem\n";
        assert_eq!(cls(s), (ErrorKind::NotFound, Some(2)));
    }

    #[test]
    fn auth_401() {
        let s = "svn: E170013: Unable to connect to a repository at URL 'http://localhost:8080/auth/small'\n\
                 svn: E215004: No more credentials or we tried too many times.\n\
                 Authentication failed\n";
        assert_eq!(cls(s), (ErrorKind::Auth, Some(215004)));
        assert_eq!(
            cls("svn: E170001: Authorization failed\n"),
            (ErrorKind::Auth, Some(170001))
        );
        let s = "svn: E175002: Unexpected HTTP status 403 'Forbidden' on '/x'\nsvn: 403 Forbidden\n";
        assert_eq!(cls(s).0, ErrorKind::Auth);
    }

    #[test]
    fn cert() {
        for (msg, want) in [
            ("issuer is not trusted", vec!["unknown-ca"]),
            ("certificate has expired", vec!["expired"]),
            ("certificate issued for a different hostname", vec!["cn-mismatch"]),
            (
                "certificate issued for a different hostname, issuer is not trusted",
                vec!["unknown-ca", "cn-mismatch"],
            ),
            ("certificate is not yet valid", vec!["not-yet-valid"]),
            ("something odd", vec!["other"]),
        ] {
            let s = format!(
                "svn: E170013: Unable to connect to a repository at URL 'https://h'\n\
                 svn: E230001: Server SSL certificate verification failed: {msg}\n"
            );
            let (k, c, m) = classify_stderr(&s);
            assert_eq!((k, c), (ErrorKind::Cert, Some(230001)));
            assert_eq!(cert_failures(&m), want);
        }
    }

    #[test]
    fn other_and_empty() {
        let s = "svn: warning: W195007: URL 'https://svn.apache.org/repos/asf/subversion' refers to a directory\n\
                 svn: E200009: Could not cat all targets because some targets are directories\n\
                 svn: E200009: Illegal target for the requested operation\n";
        let (k, c, m) = classify_stderr(s);
        assert_eq!((k, c), (ErrorKind::Other, Some(195007)));
        assert!(m.contains("refers to a directory"));
        assert_eq!(classify_stderr(""), (ErrorKind::Other, None, String::new()));
        assert_eq!(
            classify_stderr("something broke\n"),
            (ErrorKind::Other, None, "something broke".to_owned())
        );
    }

    #[test]
    fn redaction_and_quoting() {
        assert_eq!(redact_url("https://u:pw@h/x@5"), "https://u:***@h/x@5");
        assert_eq!(redact_url("https://u@h/x@5"), "https://u@h/x@5");
        assert_eq!(
            command_line("svn", &["log".into(), "--search".into(), "a b".into()]),
            "svn log --search 'a b'"
        );
    }
}
