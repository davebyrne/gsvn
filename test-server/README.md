# gsvn test server

Apache httpd + mod_dav_svn + mod_ssl in Docker (Fedora 44, Subversion 1.14.5),
serving the generated test repositories from `target/test-repos/`.

## Usage

    scripts/make-test-repos.py [target/test-repos] [--force] [--only small,deep]
    scripts/test-server.sh up        # builds image + generates missing repos first
    scripts/test-server.sh status
    scripts/test-server.sh logs      # access log includes request time in µs
    scripts/test-server.sh down
    scripts/test-server.sh rebuild   # force image rebuild

The container `gsvn-test-svn` runs as your uid (`--user $(id -u):$(id -g)`)
with the repos bind-mounted read-only at `/repos`; ports are bound to
127.0.0.1 only. Override with `GSVN_TEST_REPOS`, `GSVN_HTTP_PORT`,
`GSVN_HTTPS_PORT`.

## URLs

| URL | Access |
| --- | --- |
| `http://localhost:8080/svn/<repo>` | anonymous read |
| `http://localhost:8080/auth/<repo>` | HTTP Basic, user `test` / password `test`, realm `gsvn test` |
| `http://localhost:8080/slow/<repo>` | anonymous, responses throttled to 64 KiB/s (mod_ratelimit) |
| `https://localhost:8443/svn/<repo>` | anonymous, self-signed cert (fails with "issuer is not trusted") |
| `https://localhost:8443/auth/<repo>` | self-signed cert + Basic auth |

`http://localhost:8080/svn/` lists the repositories. Accept the certificate
with `--trust-server-cert-failures=unknown-ca` (non-interactive). The cert
has CN/SAN `localhost`; `https://127.0.0.1:8443/...` additionally fails with
"certificate issued for a different hostname" (svn ignores IP SANs), which
needs `unknown-ca,cn-mismatch`.

httpd is configured with `SVNInMemoryCacheSize 262144` (256 MB). With the
default 16 MB cache, listing `wide/trunk/wide100k` over HTTP takes minutes
(the directory is re-parsed per child); with it, ~2–3 s. Real servers may
behave like the former. Use
`--config-dir $(mktemp -d)` when testing auth so `~/.subversion` is neither
read nor written.

Added latency (as opposed to bandwidth) is not simulated: `tc netem` needs
privileges. The `/slow` location is the closest substitute.

## Repositories

Generation is deterministic (fixed seeds, dates from 2024-01-01, increasing).
Approximate generation times on a busy 12-core machine: small/deep < 1 s,
wide ~60 s (45 MB), history ~4.5 min (320 MB; `--history-revs N` to change).
Repos are `svnadmin pack`ed after loading.

### `small` (r50)
`trunk/ branches/ tags/`, authors alice, bob, carol, dave, jürgen;
multi-line logs, `BUG-nnn` issue IDs, some non-ASCII logs.

| What | Where |
| --- | --- |
| binary file (`svn:mime-type=application/octet-stream`, NULs) | `trunk/assets/logo.png` (r3, changed r29) |
| text file > 2 MB (~2.4 MB) | `trunk/data/big.txt` (r4, changed r30) |
| path with `@` | `trunk/assets/icon@2x.txt` (r5) |
| path with space | `trunk/doc/user guide.md` (added r5 as `trunk/docs/...`) |
| `svn:ignore` / `svn:mergeinfo` on dir | `trunk` (r6 / r25) |
| `svn:keywords` on file | `trunk/src/main.rs` (r6) |
| branch copy | `branches/feature-x` from `trunk@15` (r16) |
| tag copy | `tags/v0.1` from `trunk@20` (r21) |
| branch from tag | `branches/release-0.1` from `tags/v0.1@21` (r26) |
| branch from branch | `branches/feature-x-sub` from `branches/feature-x@30` (r31) |
| file rename (copy+delete) | `trunk/src/util.c` -> `trunk/src/helpers.c` (r22) |
| dir rename | `trunk/docs` -> `trunk/doc` (r40) |
| deleted file | `trunk/scripts/old_deploy.py` (deleted r24) |
| merge (mergeinfo `/branches/feature-x:17-19`) | r25 |
| empty dir | `trunk/src/empty-dir` |

### `wide` (r8)
* `trunk/wide20k/` — 20 000 entries: `fNNNNNN.txt` files plus 100 `subNNNNNN/`
  dirs (each with `inner.txt`); added in r2.
* `trunk/wide100k/` — 100 000 entries (100 subdirs), added in r3..r7.

### `deep` (r3)
`trunk/d000/d001/.../d199/leaf.txt` (200 nested dirs), with
`sibling-NNN.txt` files every 25 levels, `sideNNN/` dirs at levels 10, 60,
110, 160, and `halfway.txt` in `d099` (r3).

### `history` (r200000)
Small edits to 7 files in `trunk/{,src/core,src/ui,docs,tools}`, 1–3 files
per commit, 10 authors, plus:
* r100000: one commit adding 50 001 paths under `trunk/generated/g00..g49/`
  (author `bulkbot`).
* every 20 000th revision: tag `tags/snap-NNNNNN` copied from `trunk`.
