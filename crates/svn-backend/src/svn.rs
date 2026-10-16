//! The public facade: one [`Svn`] per application, cheap to clone.

use crate::job::{Job, Stream};
use crate::model::*;
use crate::url::RepoUrl;
use std::path::PathBuf;

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
#[derive(Clone)]
pub struct Svn {
    #[allow(dead_code)]
    pub(crate) inner: std::sync::Arc<crate::runner::Inner>,
}

impl Svn {
    pub fn new(config: SvnConfig) -> Self {
        let _ = config;
        todo!("Svn::new")
    }

    /// Receiver for credential prompts. The UI runs one loop on this.
    pub fn auth_prompts(&self) -> async_channel::Receiver<AuthPrompt> {
        todo!()
    }

    /// Receiver for certificate-trust prompts.
    pub fn cert_prompts(&self) -> async_channel::Receiver<CertPrompt> {
        todo!()
    }

    /// Pre-load credentials for a host (e.g. from the keyring).
    pub fn set_credentials(&self, host_key: &str, credentials: Credentials) {
        let _ = (host_key, credentials);
        todo!()
    }

    /// `svn info --xml URL@REV`. Use with `Rev::Head` to resolve HEAD to a
    /// number, or `Rev::Date` to resolve a date.
    pub fn info(&self, url: &RepoUrl, rev: &Rev, prio: Priority) -> Job<Info> {
        let _ = (url, rev, prio);
        todo!()
    }

    /// `svn list --xml URL@REV` (immediate children only). Cached per
    /// `(url, rev)`; a cache hit streams the cached entries immediately.
    pub fn list(&self, url: &RepoUrl, rev: RevNum, prio: Priority) -> Stream<Entry> {
        let _ = (url, rev, prio);
        todo!()
    }

    /// `svn log --xml URL@PEG -r START:END ...`, newest first.
    pub fn log(&self, url: &RepoUrl, peg: RevNum, query: &LogQuery, prio: Priority) -> Stream<LogEntry> {
        let _ = (url, peg, query, prio);
        todo!()
    }

    /// One revision with its changed paths (`svn log -v -r REV`). Pass the
    /// repository root as `url` so the entry exists whatever paths changed.
    /// Cached per `(url, rev)`.
    pub fn log_entry(&self, url: &RepoUrl, rev: RevNum, prio: Priority) -> Job<LogEntry> {
        let _ = (url, rev, prio);
        todo!()
    }

    /// `svn cat URL@REV`. Stops reading after `max_bytes` (setting
    /// `truncated`). Cached per `(url, rev)` when not truncated.
    pub fn cat(&self, url: &RepoUrl, rev: RevNum, max_bytes: Option<u64>, prio: Priority) -> Job<CatOutput> {
        let _ = (url, rev, max_bytes, prio);
        todo!()
    }

    /// `svn proplist -v --xml URL@REV`.
    pub fn proplist(&self, url: &RepoUrl, rev: RevNum, prio: Priority) -> Job<Vec<Property>> {
        let _ = (url, rev, prio);
        todo!()
    }

    /// `svn blame --xml URL@REV [-r START:REV]`.
    pub fn blame(&self, url: &RepoUrl, rev: RevNum, start: Option<RevNum>, prio: Priority) -> Stream<BlameLine> {
        let _ = (url, rev, start, prio);
        todo!()
    }

    /// `svn diff --internal-diff ...` streamed per file.
    pub fn diff(&self, spec: &DiffSpec, prio: Priority) -> Stream<FileDiff> {
        let _ = (spec, prio);
        todo!()
    }

    /// Drop all cached data (HEAD-relative results are never cached, so this
    /// is only needed to free memory).
    pub fn clear_cache(&self) {
        todo!()
    }
}
