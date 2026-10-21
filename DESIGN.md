# gsvn — Design

A lightweight, read-only Subversion repository browser for GNOME on Wayland.

## 1. Goals and scope

**Primary:** browse remote SVN repositories quickly, including directories with
tens of thousands of entries and trees nested very deeply.

**Secondary:** view commit logs (500k+ revisions), diffs, file contents and blame.

**Non-goals for v1:** checkout, commit, working-copy operations, merge-info
display, cross-URL (branch vs. branch) diffs, Flatpak/RPM packaging,
protocols other than `http(s)://`.

The architecture should leave room for checkout and commit later without
having to rework it.

## 2. Decisions summary

| Area | Decision |
|---|---|
| Language / UI | Rust, GTK4 + libadwaita (`gtk4-rs`, `libadwaita-rs`, `sourceview5`) |
| SVN backend | Shell out to the `svn` CLI with `--xml --non-interactive` and parse its output |
| Protocols | `http://` and `https://` only |
| Scale target | 10k–100k+ entries per directory, unbounded depth, 500k+ revisions |
| Browser layout | Sidebar folder tree + file list (ColumnView) |
| File selection | Syntax-highlighted content, file history, blame |
| Navigation | Editable path bar / jump-to-URL, browse at any revision, type-ahead filter, bookmarks + recent repos |
| Auth | Reuse `~/.subversion` auth cache; on failure prompt in GTK dialog, store in GNOME Keyring (libsecret) |
| Log | Page-on-scroll (200/page), server-side `--search`, changed paths per revision |
| Diff | Unified by default, side-by-side toggle; single revision or any two revisions of a path |
| Cache | In-memory, session-scoped, keyed by (URL, numeric revision) |
| Windows | Multiple windows (one GtkApplication, many AdwApplicationWindows) |
| Large files | Binary detection + ~2 MB confirm threshold; all operations cancellable |
| Distribution | `cargo build` (personal/internal use) |

## 3. Architecture

```
gsvn/
├── Cargo.toml              (workspace)
├── crates/
│   ├── svn-backend/        no GTK dependency; unit-testable
│   │   ├── runner.rs       spawn svn, cancellation, concurrency limiter
│   │   ├── xml/            streaming parsers: list, log, info, blame
│   │   ├── diff.rs         unified-diff parser → files/hunks/lines
│   │   ├── auth.rs         error classification, credential injection
│   │   ├── cache.rs        LRU caches
│   │   └── model.rs        Entry, LogEntry, ChangedPath, Revision, RepoUrl
│   └── gsvn/               GTK application
│       ├── app.rs          GtkApplication, actions, window management
│       ├── browser/        browser window: tree, file list, path bar, revision picker
│       ├── log/            log window
│       ├── diff/           diff window (unified + side-by-side)
│       ├── viewer/         file content + blame window
│       ├── auth_dialog.rs
│       └── settings.rs     bookmarks, recent repos, preferences
└── data/                   .desktop, icons, GResource UI files (Blueprint or .ui)
```

Keeping SVN logic in `svn-backend`, with no GTK dependency, makes it testable
against recorded XML fixtures. A later move from the CLI to libsvn can then
happen behind the same trait:

```rust
trait SvnBackend {
    fn info(&self, url: &RepoUrl, rev: Rev) -> Job<Info>;
    fn list(&self, url: &RepoUrl, rev: Rev) -> Stream<Vec<Entry>>;   // batches
    fn log(&self, url: &RepoUrl, q: LogQuery) -> Stream<Vec<LogEntry>>;
    fn changed_paths(&self, url: &RepoUrl, rev: u64) -> Job<Vec<ChangedPath>>;
    fn cat(&self, url: &RepoUrl, rev: Rev, limit: Option<u64>) -> Job<Bytes>;
    fn diff(&self, spec: DiffSpec) -> Stream<DiffChunk>;
    fn blame(&self, url: &RepoUrl, rev: Rev) -> Stream<Vec<BlameLine>>;
}
```

### 3.1 Process runner and threading

- Each SVN operation is one `svn` child process, spawned with
  `gio::Subprocess` or with `std::process` on a worker thread. Results reach
  the GTK main loop over `async-channel` and are consumed with
  `glib::spawn_future_local`.
- **Always pass:** `--non-interactive --xml` (where it applies), and set
  `LC_ALL=C` so error text can be parsed. Use `--config-option
  servers:global:http-compression=yes` if it helps.
- **Concurrency limiter:** at most around 4 concurrent `svn` processes per
  repository host, so the app doesn't hammer the server. Jobs have a priority:
  the visible view comes first and speculative prefetch goes last.
- **Cancellation:** every job holds a cancellation token. Navigating away from
  a directory, closing a window, or starting a new search kills the stale
  child process (SIGTERM, then SIGKILL). The UI never waits for a process
  whose result is no longer wanted.
- **Streaming:** stdout is parsed incrementally with `quick-xml`, a pull
  parser, and entries are emitted in batches of about 1000. The UI never
  holds a whole 100k-entry XML document as one string.

### 3.2 Known cost of the CLI approach

Every `svn` call over HTTPS starts a fresh process and a fresh TLS/HTTP
session: OPTIONS, then PROPFIND or REPORT. Expect roughly 50–300 ms of fixed
overhead per call, depending on the server. Mitigations:

1. Aggressive in-memory caching keyed on immutable (URL, numeric rev).
2. One `svn list` per directory, never one call per file. Use the metadata
   that `svn list --xml` already returns (kind, size, last-changed
   rev/author/date).
3. Lazy loading: only expanded or visible folders are fetched.
4. Optional prefetch of the selected folder's subdirectories at low
   priority, bounded and cancellable.

If latency is still a problem, the `SvnBackend` trait lets hot paths (list,
log) move to libsvn with a persistent RA session later.

## 4. Repository model and revisions

- Opening a repository runs `svn info --xml URL` to get the **repository
  root**, the **UUID**, and the **HEAD revision number**.
- The browser always works against a **concrete revision number**. "HEAD" is
  resolved to `rN` when the window opens and when the user presses
  **Refresh** (F5). Cache keys therefore stay immutable, and a browsing
  session stays consistent even if someone commits mid-browse.
- All URLs passed to `svn` use **peg revisions** (`URL@N`). This works
  correctly for paths that were deleted or renamed later, and for paths
  containing `@`, which are escaped by appending `@`.
- Revision picker: an entry box (accepts `HEAD`, a number, or `{2024-01-31}`)
  plus a "go to revision" popover. A date is resolved to a number through
  `svn info -r {DATE}`.

## 5. Repository browser window

```
┌ HeaderBar ─────────────────────────────────────────────────────────────┐
│ [◀][▶]  [ https://svn/repo › trunk › src › core      ] [r4520 ▾] [⟳] [☰]│
├─ Tree ──────────┬─ Filter: [__________]  ── 12,431 items ──────────────┤
│ ▾ /             │ Name            Rev   Author   Date       Size       │
│   ▾ trunk       │ 📁 io           4512  alice    2 days     —          │
│     ▾ src       │ 📄 main.c       4520  bob      1 hour     12 KB      │
│       ▸ core    │ 📄 util.c       4100  carol    3 weeks    4 KB       │
│   ▸ branches    │ …                                                    │
│   ▸ tags        │                                                      │
├─────────────────┴──────────────────────────────────────────────────────┤
│ status: listing /trunk/src/core… [Cancel]                               │
└────────────────────────────────────────────────────────────────────────┘
```

### 5.1 Folder tree (sidebar)

- `GtkListView` over a `GtkTreeListModel` with `autoexpand = false`. The
  `create_func` returns a child model **only when a row is expanded**, so
  nothing is ever fetched recursively.
- Children are a `gio::ListStore` of directory entries, filled from the same
  `list` result as the file list. The sidebar shows directories only.
- Expanding a node shows a "Loading…" placeholder row, which is replaced as
  batches arrive.
- Deep nesting: `GtkListView` virtualises rows, so depth costs nothing beyond
  the expanded nodes themselves. Indentation is capped visually (e.g. after 16
  levels, the indent stops growing and a depth badge is shown) so a deep path
  stays readable in a narrow sidebar.

### 5.2 File list (main pane)

- `GtkColumnView` (virtualised) over `GtkSortListModel(GtkFilterListModel(ListStore))`.
- Columns: Name (with icon), Last-changed Rev, Author, Date (relative, with an
  absolute tooltip), Size. Sorting is directories first, then by name;
  clicking a column header changes the sort.
- **Huge directories:** results are appended as batches with
  `ListStore::splice`, one batch per main-loop idle, so the list becomes
  usable before the listing finishes. The item count in the toolbar updates
  live.
- **Type-ahead filter:** `GtkStringFilter` (substring, case-insensitive) on
  the name. Typing anywhere in the list focuses the filter entry. Filtering
  100k in-memory items is fast enough to do synchronously.
- Double-click or Enter on a folder navigates into it, syncing the tree
  selection and path bar. On a file, it opens the file viewer window.
- Context menu: *Show log*, *Blame*, *View*, *Copy URL*, *Copy URL@rev*, *Save as…*, *Bookmark*.

### 5.3 Path bar and navigation

- A breadcrumb bar that turns into an editable `GtkEntry` when clicked or on
  Ctrl+L. It accepts a full URL, or a path relative to the repository root.
- **Jump to a deep path:** the target directory is listed and shown in the
  file list *immediately*. The sidebar tree expands along the path in the
  background, listing each ancestor at low priority, so a jump never waits on
  N round-trips.
- Back and forward history per window (Alt+←/→), and Alt+↑ for the parent
  directory.
- If the path doesn't exist at that revision, show an inline `AdwBanner` and
  offer *Show log of parent* so the user can find when it was deleted.

### 5.4 Bookmarks and recent repositories

- Stored in `~/.config/gsvn/bookmarks.toml` (XDG config dir): a name, URL,
  and optional pinned revision.
- The start window (no repository open) lists recent repositories and
  bookmarks, with a URL entry.
- The ☰ menu and Ctrl+D add the current path as a bookmark.

## 6. Log window

Opened from a path (file or directory) at a revision. Each log opens in its
own window, titled `Log — /trunk/src@4520`.

```
┌ [Search: ______________ ] [Author ▾] [r_____ : r_____] [☐ Stop on copy] ┐
├─ Rev ─ Author ─ Date ─────── Message (first line) ─────────────────────┤
│ 4520   bob      1 hour       Fix crash in io layer                     │
│ 4519   dave     2 hours      Branch for 2.0                            │
│ …      (loads more on scroll)                                          │
├─ Message ──────────────────────────────────────────────────────────────┤
│ full commit message                                                     │
├─ Changed paths ────────────────────────────────────────────────────────┤
│ M /trunk/src/io/io.c                                                    │
│ A /trunk/src/io/new.c (from /branches/x/new.c:4400)                     │
└────────────────────────────────────────────────────────────────────────┘
```

- **Paging:** `svn log --xml -l 200 -r START:1 URL@PEG`. `START` begins at
  the peg revision, and the next page starts at `last_rev - 1`. When the
  scroll position comes within about 50 rows of the end, the next page is
  fetched, with only one fetch in flight at a time. The model is a
  `ListStore`, so memory grows only with what the user has scrolled through.
- **Changed paths fetched lazily:** the page query omits `-v`, because a
  single huge commit (a 50k-file import, say) would otherwise stall a page.
  Selecting a revision runs `svn log -v --xml -r N URL@PEG`. The changed-path
  list is itself a virtualised ColumnView, with action letter, path, and copy
  source.
- **Search:** server-side `svn log --search TEXT`, which matches author, date,
  and message. Multiple terms are AND-ed (`--search-and`). Search results are
  paged the same way. Changing the query cancels the in-flight search.
  An optional revision range narrows it.
- **Stop on copy:** checkbox that adds `--stop-on-copy`. Off by default.
- Actions on a selected revision: *Diff with previous*, and *Browse
  repository at this revision* (opens a browser window at `rN`).
- Selecting **two revisions** (Ctrl-click) enables *Diff between selected
  revisions*.
- Actions on a changed path: *Diff* (that file at `-c N`), *View at rN*,
  *Blame at rN*, *Log of this path*.

## 7. Diff window

Opened from the log, or from the file context menu. One window per diff.

### 7.1 Supported specs

- **Single revision:** `svn diff -c N URL@N`, for a whole commit or one path.
  When the path is known by a newer name (a log or blame of a file that was
  renamed since N), `svn diff -c N URL@PEG` instead, so svn traces the node
  back to N (`DiffSpec::ChangeAt`). Likewise `svn info -r N URL@PEG`
  (`Svn::info_at`) gives a node's URL at N, for browsing or blaming it there.
- **Two revisions of a path:** `svn diff URL@A URL@B`, or the equivalent
  `-r A:B URL@PEG` when the path existed at both revisions.

Both commands are run with `--internal-diff`, so a user-configured external
diff tool can't break the parsing.

### 7.2 Presentation

- Left sidebar: list of files in the diff, with +/− line counts. A header
  line in the parsed output marks where each file starts.
- Main pane: `GtkSourceView` (read-only) using a custom diff style scheme.
- **Unified (default):** the svn diff output, parsed into file, hunk, and
  line, then rendered with line-number gutters for both old and new.
- **Side-by-side (toggle, Ctrl+T):** built from the parsed hunks into two
  synchronised `GtkSourceView`s with aligned filler lines and shared
  vertical scrolling. Inline intra-line highlighting comes from a char/word
  diff (the `similar` crate) on paired changed lines.
- Syntax highlighting of the diff content uses the file's language when
  GtkSourceView can guess it from the filename.
- Property changes (`Property changes on:` blocks) are shown as a separate
  section per file.

### 7.3 Limits

- Binary files: shown as "Binary file changed" with old and new sizes, never
  rendered.
- The output is streamed. Past a threshold (e.g. 5 MB of diff text or 50k
  lines), rendering pauses with an `AdwBanner`: *"This diff is large (N
  files, ~X MB). Load the rest?"* Remaining per-file sections are rendered
  only when selected in the sidebar.

## 8. File viewer and blame window

- **Content:** `svn cat URL@N` into a read-only `GtkSourceView`, using the
  language guessed from the filename and the `svn:mime-type` property.
  Supports line numbers, Ctrl+F search, and go-to-line.
- **Binary detection:** treat as binary if `svn:mime-type` is non-text
  (fetched with `svn propget svn:mime-type`, or `svn info` with `--xml`
  properties) or if there is a NUL byte in the first 8 KB. Binary files show
  metadata and *Save as…* only.
- **Size cap:** the size comes from the list metadata. Above about 2 MB,
  confirm before loading. All loads can be cancelled.
- **Blame:** a toggle in the same window. `svn blame --xml URL@N` is
  streamed, and a gutter shows the revision and author per line, with commit
  groups visually banded. Clicking a revision in the gutter opens a diff for
  that revision, or a log centered on it. Blame on long histories is slow
  server-side, so it shows progress and stays cancellable, and an optional
  "blame since rX" limits it with `-r X:N`.
- *History* button: opens the log window for this path.

## 9. Authentication

1. Every call runs `--non-interactive`, so the existing
   `~/.subversion/auth` cache (including GNOME Keyring via svn's
   `gnome-keyring` / `gpg-agent` providers, if configured) is used
   transparently.
2. When stderr contains an auth failure (`E170001`, `E215004`, or HTTP
   401/403 text), the runner pauses that host's queue and shows an
   **AdwMessageDialog** asking for a username, password, and *Remember*.
3. The job is retried with `--username U --password-from-stdin`
   (svn ≥ 1.10) and the password is written to the child's stdin, so it
   never appears in argv or `/proc`.
4. With *Remember* ticked, the credentials are stored in libsecret through
   the `oo7` crate, under the attributes `{app: gsvn, realm, username}`, and
   looked up before prompting next time.
5. **TLS certificate errors** (`E230001`): show a dialog with the cert
   details (fetched through `--trust-server-cert-failures` diagnostics from
   the error text) and offer *Trust for this session* or *Cancel*. A
   session-trusted host gets `--trust-server-cert-failures=unknown-ca,cn-mismatch,...`
   as applicable. Certificates are never trusted permanently and silently.

## 10. Caching

- `svn-backend::cache`: LRU in-memory caches, session-scoped, never written to
  disk.
  - `list`: key `(url, rev)`, bounded by total entry count (e.g. 2M entries).
  - `log pages`: key `(url, peg, query, start)`.
  - `changed paths`: key `(repo_uuid, rev)`.
  - `cat` / `blame`: key `(url, rev)`, bounded by bytes (e.g. 64 MB).
- Keys always hold **numeric revisions**, so entries never go stale. Refresh
  re-resolves HEAD to a new number, which naturally misses the cache.
- Cache entries are shared across windows through an `Rc` that lives in the
  application.

## 11. Windows and application lifecycle

- A single `adw::Application` (`app-id: io.github.<user>.Gsvn`). Launching
  the app again opens a new window in the existing process.
- Window types: **Start** (recents and bookmarks), **Browser**, **Log**,
  **Diff**, and **Viewer**. Each is an `adw::ApplicationWindow`.
- The CLI accepts a URL: `gsvn https://svn/repo/trunk` opens a browser window
  directly, and `gsvn --log URL` opens a log window.
- When a window closes, all of its jobs are cancelled.
- Window sizes and pane positions are saved in a `GSettings`-free TOML state
  file, `~/.local/state/gsvn/state.toml`, because no schema installation is
  needed under plain `cargo build`.

## 12. Error handling

- svn errors are classified by `E######` code: not found, auth, network,
  cert, cancelled, or other.
- Errors are shown inline in the view that requested them (`AdwStatusPage`
  or `AdwBanner`) with a *Retry* button. Modal dialogs are reserved for
  auth and certificates.
- A *Details* expander shows the exact svn command (without passwords) and
  its stderr. This is useful for debugging server issues.

## 13. Performance budget (targets)

| Scenario | Target |
|---|---|
| Cold start to start window | < 300 ms |
| Idle RSS, one browser window | < 60 MB |
| First rows visible in a 100k-entry directory | ≤ svn latency + 200 ms |
| Scroll / filter in a 100k-entry list | 60 fps, filter < 50 ms |
| Log first page (200 revs) | ≤ svn latency + 100 ms |
| Jump to a path 40 levels deep | file list shown after 1 `list` call |

## 14. Key crates

`gtk4`, `libadwaita`, `sourceview5`, `glib`/`gio`, `quick-xml`,
`async-channel`, `similar`, `oo7` (libsecret), `serde` + `toml`, `url`,
`chrono` (or `jiff`), `lru`, `thiserror`.

## 15. Milestones

1. **Backend core:** runner (spawn, cancel, limiter), `info`/`list`
   streaming XML parsers, error classification, fixture-based tests.
2. **Browser MVP:** start window, browser window with lazy tree and file
   list, batched loading, filter, path bar, Refresh.
3. **Revisions and nav:** revision picker, peg handling, back/forward,
   background tree expansion on deep jumps, bookmarks and recents.
4. **Auth:** prompt dialog, `--password-from-stdin`, libsecret, cert
   dialog.
5. **Log window:** paging, lazy changed paths, search, stop-on-copy.
6. **Diff window:** unified-diff parser, unified view, file sidebar,
   side-by-side, large-diff guard.
7. **Viewer and blame:** cat with binary and size guards, highlighting,
   blame gutter.
8. **Polish:** keyboard shortcuts, state persistence, performance pass
   against the budget in §13.

## 16. Future (post-v1)

- libsvn backend behind `SvnBackend`, with persistent RA sessions, if the
  CLI's per-call latency becomes the bottleneck.
- Checkout and commit (a working-copy browser, with status through
  `svn status --xml`).
- Cross-URL diffs (branch against trunk), merge-info (`-g`) in the log,
  issue-tracker links.
- `svn://` and `svn+ssh://` protocols. These need almost no change, since
  the CLI handles them.
- Flatpak and RPM packaging.
- Optional persistent log cache (SQLite) for offline search.
