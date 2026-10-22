# gsvn

A lightweight, read-only Subversion repository browser for GNOME, written
in Rust with GTK 4 and libadwaita.

gsvn is built for large remote repositories: directories with 100k+
entries, very deep trees and histories of 500k+ revisions. It talks to
servers over `http://` and `https://` by running the `svn` command-line
client and parsing its XML output. It never checks out, commits or touches a
working copy.

## Features

**Start window**
- Open a repository by URL, optionally at a fixed revision.
- Reopen recent repositories and bookmarks. Remove single recent entries,
  or clear the whole list.

**Repository browser**
- A folder tree in the sidebar and a sortable file list (name, last-changed
  revision, author, date, size).
- Huge directories stream in, so the list is usable before the listing
  finishes. A type-ahead filter narrows it by name.
- An editable path bar (Ctrl+L) that accepts a full URL or a path relative
  to the repository root, with an optional `@REV`. Jumping to a deep path
  shows it at once while the tree expands in the background.
- Browse at any revision, with back/forward history and Alt+↑ for the
  parent folder.
- Context menu: open, show log, blame, copy URL (with or without the
  revision), save as, bookmark.
- If a repository can't be reached, retry or go back to change the URL.

**Log**
- Pages in 200 revisions at a time as you scroll.
- Server-side search over author, date and message, an optional revision
  range, and *Stop on copy*.
- Changed paths per revision, loaded on selection.
- Diff a revision with its predecessor, diff two selected revisions, or
  browse the repository at a revision.

**Diff**
- Unified view, or side by side (Ctrl+T) with intra-line highlights.
- A file list with +/− line counts, syntax highlighting and property
  changes.
- Binary files are summarised, never rendered. Very large diffs pause and
  ask before loading the rest.
- Follows files that were renamed since the revision being diffed.

**File viewer and blame**
- Syntax-highlighted content, with find and go-to-line.
- Binary detection, and a confirmation before loading files over about 2 MB.
- Streaming blame with a revision/author gutter. Click a revision to open
  its diff or log, or limit blame to recent revisions.

**General**
- Several windows at once. Every load can be cancelled, and closing a
  window cancels its work.
- Authentication reuses your `~/.subversion` credentials. Otherwise gsvn
  asks for a username and password and can store them in the GNOME Keyring.
  Passwords are passed to `svn` on stdin, never on the command line.
- Untrusted TLS certificates are shown in a dialog and can be trusted for
  the current session only.
- Errors appear inline, with a *Details* section that shows the exact `svn`
  command and its output.
- Keyboard shortcuts throughout. Press Ctrl+? to list them.

## Requirements

- Linux. gsvn is built for GNOME on Wayland.
- The Subversion client `svn`, version 1.10 or later, on `PATH`.
- A recent stable Rust toolchain (edition 2024).
- Development packages for GTK 4 (≥ 4.16), libadwaita (≥ 1.8) and
  GtkSourceView 5.

On Fedora:

```sh
sudo dnf install subversion rust cargo gtk4-devel libadwaita-devel gtksourceview5-devel
```

## Building and running

```sh
cargo build --release

target/release/gsvn                                  # start window
target/release/gsvn https://svn.example.org/repo/trunk
target/release/gsvn --rev 1234 https://svn.example.org/repo/trunk
target/release/gsvn --log https://svn.example.org/repo/trunk
```

Command line:

| Command | Opens |
|---|---|
| `gsvn` | the start window |
| `gsvn URL…` | a browser window per URL |
| `gsvn --rev N URL` | a browser at revision `N` (`-r`; a number or `HEAD`) |
| `gsvn --log [--rev N] URL` | a log window |

gsvn is a single-instance application. Launching it again opens the new
windows in the running process.

### Installing for your user

```sh
scripts/install-local.sh              # release build into ~/.local, with desktop entry and icons
scripts/install-local.sh --uninstall
```

## Settings

| File | Contents |
|---|---|
| `~/.config/gsvn/bookmarks.toml` | bookmarks and recent repositories |
| `~/.local/state/gsvn/state.toml` | window sizes and pane positions |

Set `GSVN_CONFIG_DIR` to keep both files in a different directory, for
example a throwaway one when testing.

## Development

```sh
cargo test
cargo clippy --all-targets
```

The code is split into two crates:

- `crates/svn-backend`: runs `svn`, parses its output, handles auth and
  certificates, and caches results. It has no GTK dependency.
- `crates/gsvn`: the GTK application with its start, browser, log, diff and
  viewer windows.

`DESIGN.md` describes the design in detail. Debug builds have scripting
hooks for automated runs; each window's `debug.rs` and `crates/gsvn/src/e2e.rs`
document them.

### Test server

`scripts/test-server.sh up` starts an Apache + mod_dav_svn server in Docker
with generated test repositories (`small`, `wide`, `deep`, `history`). It
serves them anonymously, behind Basic auth, over self-signed HTTPS and
throttled. `test-server/README.md` lists the URLs. With the server running,
`GSVN_TEST_HTTP=1 cargo test` also runs the HTTP tests.

```sh
scripts/test-server.sh up
GSVN_CONFIG_DIR=$(mktemp -d) cargo run -- http://localhost:8080/svn/small
```

`scripts/headless.sh <command>` runs the app in a private headless
compositor, for automated runs and screenshots.

## License

GPL-3.0-or-later.
