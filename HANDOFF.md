# gsvn — Handoff (updated 2026-10-21)

Read `DESIGN.md` first. This file records the state of the work and how to
pick it up.

## TL;DR

- **Integration branch:** `develop`. **`master`** still holds only the design
  doc; nothing is merged there or pushed anywhere.
- **On `develop`:**
  - the backend, test infrastructure and app shell
  - all four windows: browser, log, diff, viewer/blame
  - the end-to-end pass, the release performance pass and the polish pass
- **Build state:**
  - `cargo test`: 192 pass, and `GSVN_TEST_HTTP=1` passes too (one
    unidentified HTTP test failed once on 2026-10-21 and then passed in
    six reruns)
  - `cargo clippy --all-targets`: zero warnings
  - the release build runs cleanly
- **All HANDOFF next steps from 2026-10-15 are done.** What is left is
  listed under "Open items" below.

## Branches

All of these are merged into `develop` with `--no-ff`, and all agent
worktrees have been removed. The branches are kept only for history.

| Branch | Content |
|---|---|
| `infra/test-repos` | Docker test server and repo generator |
| `backend/parsers` | 6 streaming parsers and fixtures |
| `backend/runner` | runner, limiter, auth/cert, cache |
| `ui/shell` | settings, auth UI, start window, CLI, desktop file |
| `ui/log` | log window §6 |
| `ui/viewer` | viewer and blame §8 |
| `ui/diff` | diff window §7 |
| `ui/browser` | browser §5 |
| `backend/peg` | `DiffSpec::ChangeAt` and `Svn::info_at`, for paths renamed since an older revision |
| `perf/diff` | §7.3 total diff size limit, binary details across renames, diff measurements |
| `e2e/links` | cross-window pass, `GSVN_E2E_SCRIPT` hook, 7 link and menu bugs fixed |
| `perf/release` | release-build §13 measurements, compact browser model and caches |
| `polish/ui` | shortcuts dialog, state persistence, scaffolding removed, header nits |
| `polish/nits` | the five visual nits from 2026-10-21: log Revision alignment, blank details while loading, tree reveal scroll, Makefile assignments, intra-line for a line kept inside the other |

## Process

- **Contract first:** the public API of `svn-backend` is a contract. Change it
  only additively and on purpose.
- **Branches:** one branch per piece of work, merged into `develop` with
  `--no-ff`. Never merge to `master` or push without asking the user.
- **Dependencies:** pin crate versions that are at least 2 weeks old
  (`cargo update -p X --precise V`). libadwaita uses the `v1_8` feature,
  because the shortcuts dialog is `AdwShortcutsDialog`.
- **Formatting:** the tree is **not** `cargo fmt` clean, and a full format
  touches about 20 files. Format only the lines you change, or do a full
  format as its own commit.
- **Commits** end with a `Co-Authored-By:` line for Claude.

## Running and testing

```sh
cargo build && cargo test && cargo clippy --all-targets
GSVN_TEST_HTTP=1 cargo test          # also the HTTP auth/cert/peg tests (needs the test server)
GSVN_CONFIG_DIR=$(mktemp -d) cargo run -- http://localhost:8080/svn/small
cargo run -- --log http://localhost:8080/svn/history/trunk
```

The app ID is single-instance. For automated or headless runs, use
`scripts/headless.sh <binary>`, which starts a private headless mutter and
D-Bus session with a throwaway config dir. For a criticals run, use
`scripts/headless.sh env G_DEBUG=fatal-criticals target/debug/gsvn …`;
setting it for all of `headless.sh` aborts mutter on its own critical.

### Test server (Docker)

Run `scripts/test-server.sh up|down|status|logs|rebuild`; details are in
`test-server/README.md`. The test server was left **running**. The endpoints
are `/svn/` (anonymous), `/auth/` (`test`/`test`), `https://…:8443`
(self-signed) and `/slow/` (64 KiB/s). The repos are `small`, `wide`, `deep`
and `history`.

### Debug hooks (debug builds only)

- **Per window:**
  - browser: `GSVN_BROWSER_*`
  - log: `GSVN_LOG_SCRIPT`
  - diff: `GSVN_DEBUG_DIFF` and `GSVN_DIFF_*`. `GSVN_DIFF_LIMIT=BYTES[:LINES]`
    and `GSVN_DIFF_LOAD_REST=1` control the size limit; `URL REV@PEG` opens a
    `ChangeAt` diff.
  - viewer: `GSVN_DEBUG_VIEWER*`
  - auth dialogs: `GSVN_DEBUG_PROMPTS`
  - startup timing: `GSVN_DEBUG_STARTUP`
  - Each file's doc comment lists its steps.
- **Across windows:** `GSVN_E2E_SCRIPT` (`crates/gsvn/src/e2e.rs`) sends each
  step to the newest window, or to the one chosen with `win=`. It can:
  - answer the auth and cert dialogs
  - drive the in-process file chooser (`GDK_DEBUG=no-portals`)
  - check titles
  - flag cut-off popovers
  - take screenshots that include popovers

  Use `action=app.quit`, not `quit`, when the window state must be saved.
- **Keyring testing:** start an isolated gnome-keyring with
  `--control-directory` and set `XDG_DATA_HOME` to a scratch dir. Never touch
  the real login keyring.

## Open items

- **Performance (DESIGN §13 now has measured release numbers):**
  - Idle RSS is about 101 MB, within the §13 target of "toolkit baseline +
    10 MB" that the user accepted on 2026-10-21 (the old 60 MB target was
    below GTK's own 92–97 MB).
  - Filtering a 100k list takes 25–110 ms against the 50 ms target. The
    filter itself takes at most 15 ms; the rest is GTK rebuilding the
    visible row widgets. Fewer or lighter widgets per row, such as
    `GtkInscription`, could help.
  - Log rows cost about 310 B each and authors are not interned
    (`RevisionItem`).
- **Style schemes:** source views use gsvn's own `gsvn`/`gsvn-dark`
  schemes (Adwaita plus Makefile assignment names), written to
  `~/.cache/gsvn/styles` on first use; they fall back to Adwaita if that
  fails. GtkSourceView's `use-style` can't refer to the parent scheme, so
  each extra style needs explicit colors.
- **Shortcut change:** the viewer's Go to Line is now **Ctrl+I**; it was
  Ctrl+L and Ctrl+G. Ctrl+G and Ctrl+Shift+G are Next and Previous Match.
- **Not testable headlessly:**
  - window focus
  - real key presses for shortcuts (only their actions were checked)
  - the portal file chooser
  - popovers on inactive windows
  - trusting a certificate in the dialog and then listing (the HTTP tests
    cover the backend side)
- **Next steps:** merge `develop` into `master` once the user has reviewed it
  (ask first). Packaging (Flatpak/RPM) is post-v1, as in DESIGN §16.

## Known issues and gaps

- **Log search** uses revision windows, because svn applies `-l` to
  revisions examined, not matches. A search with no matches across 200k
  revisions takes about 7 s, with progress shown. Search combined with
  stop-on-copy first runs a probe that walks the whole branch.
- **Blame** has no line-count progress: svn sends nothing until it finishes,
  so the window shows elapsed time instead.
- **Auth:**
  - A 403 response is classified as Auth, so it prompts up to 3 times.
  - The auth and cert dialogs can be on screen at the same time.
  - A keyring that doesn't answer within 5 s is treated as unavailable for
    the rest of the session.
- **Diff:** once the total size limit is passed, loading pauses. "Load the
  Rest" runs svn again from the start and skips files already listed.
- **Caches:** the backend list cache is bounded (500k entries) and the
  `log_entry` cache is bounded by changed paths (200k). The browser has no
  per-revision eviction of its own.
- **Large text files:** scrolling during the first ~1.2 s after loading a
  2.4 MB file can land on the wrong line, because GTK estimates line heights
  until it has measured them all. The viewer re-scrolls to work around it.
- **Environment:**
  - `libsecret-devel` is not installed, so the keyring uses the `oo7` crate.
  - svn runs under `LC_ALL=C.UTF-8`; plain `C` breaks non-ASCII arguments.
