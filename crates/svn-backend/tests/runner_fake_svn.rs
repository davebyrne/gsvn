//! Runner tests against a fake `svn` shell script (deterministic: no network,
//! no real svn). The script picks its behaviour from the first path segment
//! of the URL argument, e.g. `http://host/slow/name`.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use svn_backend::*;

const SCRIPT: &str = r#"#!/bin/bash
LOG='@LOG@'
user=""; pwstdin=0; trust=""; url=""; prev=""
for a in "$@"; do
  [ "$prev" = "--username" ] && user="$a"
  case "$a" in
    --password-from-stdin) pwstdin=1 ;;
    --trust-server-cert-failures=*) trust="${a#*=}" ;;
    *://*) url="$a" ;;
  esac
  prev="$a"
done
pw=""
if [ "$pwstdin" = 1 ]; then IFS= read -r pw || true; fi
printf '%s\n' "$*" >> "$LOG/argv"
[ -n "$pw" ] && printf '%s\n' "$pw" >> "$LOG/passwords"
rest="${url#*://}"; path="${rest#*/}"; path="${path%@*}"
scenario="${path%%/*}"; name="${path#*/}"
base="${url%@*}"

info() {
  cat <<XML
<?xml version="1.0" encoding="UTF-8"?>
<info>
<entry path="x" revision="7" kind="dir">
<url>$base</url>
<relative-url>^/</relative-url>
<repository><root>$base</root><uuid>uuid-1</uuid></repository>
<commit revision="7"><author>a</author><date>2026-06-07T08:09:10.000000Z</date></commit>
</entry>
</info>
XML
}

case "$scenario" in
  ok) info ;;
  slow)
    echo "start $name" >> "$LOG/events"
    sleep 0.3
    echo "end $name" >> "$LOG/events"
    info ;;
  sleep)
    sleep 30 &
    echo "$$ $!" > "$LOG/pids.$name"
    wait
    info ;;
  auth)
    if [ "$user" = alice ] && [ "$pw" = secret ]; then
      sleep 0.05; info
    else
      sleep 0.05
      echo "svn: E170013: Unable to connect to a repository at URL '$base'" >&2
      echo "svn: E215004: No more credentials or we tried too many times." >&2
      echo "Authentication failed" >&2
      exit 1
    fi ;;
  cert)
    case ",$trust," in *,unknown-ca,*) case ",$trust," in *,cn-mismatch,*) info; exit 0 ;; esac ;; esac
    echo "svn: E170013: Unable to connect to a repository at URL '$base'" >&2
    echo "svn: E230001: Server SSL certificate verification failed: certificate issued for a different hostname, issuer is not trusted" >&2
    exit 1 ;;
  notfound)
    echo "svn: warning: W170000: URL '$base' non-existent in revision 7" >&2
    echo "" >&2
    echo "svn: E200009: Could not display info for all targets because some targets don't exist" >&2
    exit 1 ;;
  norev) echo "svn: E160006: No such revision 99999999" >&2; exit 1 ;;
  refused)
    echo "svn: E170013: Unable to connect to a repository at URL '$base'" >&2
    echo "svn: E000111: Error running context: Connection refused" >&2
    exit 1 ;;
  badhost)
    echo "svn: E170013: Unable to connect to a repository at URL '$base'" >&2
    echo "svn: E670002: Name or service not known" >&2
    exit 1 ;;
  weird) echo "svn: E999999: Something odd" >&2; exit 1 ;;
  garbage) echo "this is not xml" ;;
  yes) exec yes 0123456789abcdef ;;
  small) printf 'hello\n' ;;
  list)
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo "<lists><list path=\"$base\">"
    for i in $(seq 1 "$name"); do
      echo "<entry kind=\"file\"><name>f$i</name><size>$i</size><commit revision=\"3\"><author>a</author><date>2026-06-07T08:09:10.000000Z</date></commit></entry>"
    done
    echo "</list></lists>" ;;
  *) echo "svn: E000001: unknown scenario $scenario" >&2; exit 1 ;;
esac
"#;

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = SCRIPT.replace("@LOG@", dir.path().to_str().unwrap());
        let bin = dir.path().join("svn");
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        fs::create_dir(dir.path().join("cfg")).unwrap();
        Self { dir }
    }

    fn config(&self) -> SvnConfig {
        SvnConfig {
            svn_binary: self.dir.path().join("svn"),
            config_dir: Some(self.dir.path().join("cfg")),
            interactive: false,
            ..SvnConfig::default()
        }
    }

    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn calls(&self) -> usize {
        self.log("argv").lines().count()
    }
}

fn url(s: &str) -> RepoUrl {
    RepoUrl::parse(s).unwrap()
}

fn wait_for_file(p: &Path) -> String {
    let t = Instant::now();
    loop {
        if let Ok(s) = fs::read_to_string(p)
            && s.ends_with('\n')
        {
            return s;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "timed out waiting for {p:?}");
        thread::sleep(Duration::from_millis(5));
    }
}

/// True if the pid no longer exists or is a zombie.
fn is_dead(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => {
            let state = stat.rsplit(')').next().unwrap_or("").trim_start();
            state.starts_with('Z') || state.starts_with('X')
        }
    }
}

fn assert_dead_soon(pid: u32) {
    let t = Instant::now();
    while !is_dead(pid) {
        assert!(t.elapsed() < Duration::from_secs(1), "process {pid} still alive");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn svn_not_found() {
    let svn = Svn::new(SvnConfig {
        svn_binary: "/nonexistent/bin/svn".into(),
        config_dir: Some(tempfile::tempdir().unwrap().path().to_owned()),
        ..SvnConfig::default()
    });
    let e = svn
        .info(&url("http://h/ok/x"), &Rev::Head, Priority::Normal)
        .wait()
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::SvnNotFound);
    assert!(e.command.starts_with("/nonexistent/bin/svn --non-interactive"));
}

#[test]
fn info_ok_and_argv() {
    let f = Fake::new();
    let svn = Svn::new(f.config());
    let i = svn
        .info(&url("http://h/ok/x"), &Rev::Head, Priority::Normal)
        .wait()
        .unwrap();
    assert_eq!(i.rev, 7);
    let argv = f.log("argv");
    let cfg = f.path("cfg");
    assert_eq!(
        argv.trim(),
        format!("--non-interactive --config-dir {} info --xml http://h/ok/x@HEAD", cfg.display())
    );
    // Rev::Number(7) is now served from the cache (HEAD resolved to 7).
    let i2 = svn
        .info(&url("http://h/ok/x"), &Rev::Number(7), Priority::Normal)
        .wait()
        .unwrap();
    assert_eq!(i, i2);
    assert_eq!(f.calls(), 1);
    // HEAD is never served from the cache.
    svn.info(&url("http://h/ok/x"), &Rev::Head, Priority::Normal)
        .wait()
        .unwrap();
    assert_eq!(f.calls(), 2);
    svn.clear_cache();
    svn.info(&url("http://h/ok/x"), &Rev::Number(7), Priority::Normal)
        .wait()
        .unwrap();
    assert_eq!(f.calls(), 3);
}

#[test]
fn error_classification_from_process() {
    let f = Fake::new();
    let svn = Svn::new(f.config());
    for (path, kind, code) in [
        ("notfound", ErrorKind::NotFound, Some(170000)),
        ("norev", ErrorKind::NotFound, Some(160006)),
        ("refused", ErrorKind::Network, Some(111)),
        ("badhost", ErrorKind::Network, Some(670002)),
        ("weird", ErrorKind::Other, Some(999999)),
        ("auth", ErrorKind::Auth, Some(215004)),
        ("cert", ErrorKind::Cert, Some(230001)),
    ] {
        let u = url(&format!("http://h/{path}/x"));
        let e = svn.info(&u, &Rev::Number(3), Priority::Normal).wait().unwrap_err();
        assert_eq!((e.kind.clone(), e.code), (kind, code), "{path}: {e:?}");
        assert!(e.command.contains(&format!("{u}@3")), "{}", e.command);
        assert!(e.stderr.contains("svn: "), "{path}");
        assert!(!e.message.is_empty());
    }
    let e = svn
        .info(&url("http://h/garbage/x"), &Rev::Number(3), Priority::Normal)
        .wait()
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Parse);
}

#[test]
fn concurrency_limit_per_host() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        max_concurrent_per_host: 2,
        ..f.config()
    });
    let jobs: Vec<_> = (0..6)
        .map(|i| svn.info(&url(&format!("http://a.test/slow/{i}")), &Rev::Head, Priority::Normal))
        .collect();
    // A different host is not blocked by a.test's queue.
    let t = Instant::now();
    svn.info(&url("http://b.test/ok/x"), &Rev::Head, Priority::Normal)
        .wait()
        .unwrap();
    assert!(t.elapsed() < Duration::from_millis(250));
    for j in jobs {
        j.wait().unwrap();
    }
    let mut running = 0;
    let mut max = 0;
    for line in f.log("events").lines() {
        if line.starts_with("start") {
            running += 1;
            max = max.max(running);
        } else {
            running -= 1;
        }
    }
    assert_eq!(max, 2);
}

#[test]
fn priority_order_then_fifo() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        max_concurrent_per_host: 1,
        ..f.config()
    });
    let blocker = svn.info(&url("http://p.test/slow/blocker"), &Rev::Head, Priority::Normal);
    wait_for_file(&f.path("events"));
    let order = [
        ("bg1", Priority::Background),
        ("n1", Priority::Normal),
        ("i1", Priority::Interactive),
        ("n2", Priority::Normal),
        ("bg2", Priority::Background),
        ("i2", Priority::Interactive),
    ];
    let jobs: Vec<_> = order
        .iter()
        .map(|(n, p)| svn.info(&url(&format!("http://p.test/slow/{n}")), &Rev::Head, *p))
        .collect();
    blocker.wait().unwrap();
    for j in jobs {
        j.wait().unwrap();
    }
    let started: Vec<String> = f
        .log("events")
        .lines()
        .filter_map(|l| l.strip_prefix("start ").map(str::to_owned))
        .collect();
    assert_eq!(started, ["blocker", "i1", "i2", "n1", "n2", "bg1", "bg2"]);
}

#[test]
fn cancel_kills_running_child_promptly() {
    let f = Fake::new();
    let svn = Svn::new(f.config());
    let job = svn.info(&url("http://c.test/sleep/a"), &Rev::Head, Priority::Normal);
    let pids = wait_for_file(&f.path("pids.a"));
    let pids: Vec<u32> = pids.split_whitespace().map(|p| p.parse().unwrap()).collect();
    let t = Instant::now();
    job.cancel();
    let e = job.wait().unwrap_err();
    let took = t.elapsed();
    assert_eq!(e.kind, ErrorKind::Cancelled);
    assert!(took < Duration::from_millis(300), "cancel took {took:?}");
    // Both the fake svn (bash) and its `sleep` grandchild are gone.
    for pid in pids {
        assert_dead_soon(pid);
    }
}

#[test]
fn dropping_stream_kills_child() {
    let f = Fake::new();
    let svn = Svn::new(f.config());
    let stream = svn.list(&url("http://c.test/sleep/b"), 1, Priority::Normal);
    let pids = wait_for_file(&f.path("pids.b"));
    drop(stream);
    for pid in pids.split_whitespace().map(|p| p.parse().unwrap()) {
        assert_dead_soon(pid);
    }
}

#[test]
fn cancel_while_queued() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        max_concurrent_per_host: 1,
        ..f.config()
    });
    let blocker = svn.info(&url("http://q.test/sleep/q"), &Rev::Head, Priority::Normal);
    wait_for_file(&f.path("pids.q"));
    let queued = svn.info(&url("http://q.test/ok/never"), &Rev::Head, Priority::Interactive);
    thread::sleep(Duration::from_millis(30));
    let t = Instant::now();
    queued.cancel();
    assert_eq!(queued.wait().unwrap_err().kind, ErrorKind::Cancelled);
    assert!(t.elapsed() < Duration::from_millis(200));
    drop(blocker);
    assert!(!f.log("argv").contains("never"));
}

#[test]
fn cat_truncates_and_kills_endless_output() {
    let f = Fake::new();
    let svn = Svn::new(f.config());
    let t = Instant::now();
    let out = svn
        .cat(&url("http://h/yes/x"), 5, Some(1000), Priority::Normal)
        .wait()
        .unwrap();
    assert!(out.truncated);
    assert_eq!(out.data.len(), 1000);
    assert!(out.data.starts_with(b"0123456789abcdef\n"));
    assert!(t.elapsed() < Duration::from_secs(2));

    let out = svn
        .cat(&url("http://h/small/x"), 5, Some(1000), Priority::Normal)
        .wait()
        .unwrap();
    assert_eq!((out.data.as_slice(), out.truncated), (&b"hello\n"[..], false));
    let calls = f.calls();
    // Cache hit, also when asking for less.
    let out = svn
        .cat(&url("http://h/small/x"), 5, Some(3), Priority::Normal)
        .wait()
        .unwrap();
    assert_eq!((out.data.as_slice(), out.truncated), (&b"hel"[..], true));
    let out = svn
        .cat(&url("http://h/small/x"), 5, None, Priority::Normal)
        .wait()
        .unwrap();
    assert_eq!(out.data, b"hello\n");
    assert_eq!(f.calls(), calls);
    // Exactly max_bytes is not truncated.
    let out = svn
        .cat(&url("http://h/small/y"), 5, Some(6), Priority::Normal)
        .wait()
        .unwrap();
    assert!(!out.truncated);
}

#[test]
fn list_batches_and_cache() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        batch_size: 1000,
        ..f.config()
    });
    let u = url("http://h/list/2500");
    let mut s = svn.list(&u, 9, Priority::Normal);
    let mut total = Vec::new();
    while let Some(b) = s.next_blocking() {
        let b = b.unwrap();
        assert!(!b.is_empty() && b.len() <= 1000);
        total.extend(b);
    }
    assert_eq!(total.len(), 2500);
    assert_eq!(total[2499].name, "f2500");
    assert_eq!(f.calls(), 1);
    let again = svn.list(&u, 9, Priority::Normal).collect_blocking().unwrap();
    assert_eq!(again, total);
    assert_eq!(f.calls(), 1);
    // A different revision is a different key.
    svn.list(&u, 10, Priority::Normal).collect_blocking().unwrap();
    assert_eq!(f.calls(), 2);
}

#[test]
fn list_cache_respects_entry_bound() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        list_cache_entries: 100,
        ..f.config()
    });
    let big = url("http://h/list/150");
    assert_eq!(svn.list(&big, 1, Priority::Normal).collect_blocking().unwrap().len(), 150);
    svn.list(&big, 1, Priority::Normal).collect_blocking().unwrap();
    assert_eq!(f.calls(), 2, "listing larger than the cache must not be cached");
}

// ---------------------------------------------------------------------------
// Auth

fn creds(u: &str, p: &str) -> Credentials {
    Credentials {
        username: u.into(),
        password: p.into(),
    }
}

#[test]
fn auth_single_prompt_for_concurrent_jobs_and_retry() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        interactive: true,
        ..f.config()
    });
    let prompts = svn.auth_prompts();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = {
        let seen = seen.clone();
        thread::spawn(move || {
            while let Ok(p) = prompts.recv_blocking() {
                let n = {
                    let mut s = seen.lock().unwrap();
                    s.push((p.host_key.clone(), p.rejected, p.previous_username.clone()));
                    s.len()
                };
                // Let the other jobs pile up behind this prompt.
                thread::sleep(Duration::from_millis(100));
                if n == 1 {
                    p.respond(Some(creds("alice", "wrong")));
                } else {
                    p.respond(Some(creds("alice", "secret")));
                }
            }
        })
    };
    let jobs: Vec<_> = (0..5)
        .map(|i| svn.info(&url(&format!("http://auth.test:8080/auth/{i}")), &Rev::Head, Priority::Normal))
        .collect();
    for j in jobs {
        j.wait().unwrap();
    }
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            ("http://auth.test:8080".to_owned(), false, None),
            ("http://auth.test:8080".to_owned(), true, Some("alice".to_owned())),
        ]
    );
    // Credentials are remembered for the host: no new prompt.
    svn.info(&url("http://auth.test:8080/auth/later"), &Rev::Head, Priority::Normal)
        .wait()
        .unwrap();
    // The password only ever went through stdin.
    let argv = f.log("argv");
    assert!(argv.contains("--username alice --password-from-stdin --no-auth-cache"));
    assert!(!argv.contains("secret") && !argv.contains("wrong"));
    assert!(f.log("passwords").contains("secret"));
    drop(svn);
    drop(handler);
}

#[test]
fn auth_declined_fails_all_waiting_jobs() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        interactive: true,
        ..f.config()
    });
    let prompts = svn.auth_prompts();
    let count = Arc::new(AtomicUsize::new(0));
    {
        let count = count.clone();
        thread::spawn(move || {
            while let Ok(p) = prompts.recv_blocking() {
                count.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(100));
                p.respond(None);
            }
        });
    }
    let jobs: Vec<_> = (0..4)
        .map(|i| svn.info(&url(&format!("http://decline.test/auth/{i}")), &Rev::Head, Priority::Normal))
        .collect();
    for j in jobs {
        let e = j.wait().unwrap_err();
        assert_eq!(e.kind, ErrorKind::Auth);
        assert!(e.stderr.contains("E215004"));
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[test]
fn auth_dropped_prompt_counts_as_cancel() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        interactive: true,
        ..f.config()
    });
    let prompts = svn.auth_prompts();
    thread::spawn(move || {
        while let Ok(p) = prompts.recv_blocking() {
            drop(p);
        }
    });
    let e = svn
        .info(&url("http://drop.test/auth/x"), &Rev::Head, Priority::Normal)
        .wait()
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
}

#[test]
fn auth_prompt_wait_is_cancellable() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        interactive: true,
        ..f.config()
    });
    let prompts = svn.auth_prompts();
    let job = svn.info(&url("http://cancel.test/auth/x"), &Rev::Head, Priority::Normal);
    let p = prompts.recv_blocking().unwrap();
    let t = Instant::now();
    job.cancel();
    assert_eq!(job.wait().unwrap_err().kind, ErrorKind::Cancelled);
    assert!(t.elapsed() < Duration::from_millis(200));
    drop(p);
}

#[test]
fn auth_non_interactive_fails_immediately_and_hides_password() {
    let f = Fake::new();
    let svn = Svn::new(f.config());
    let prompts = svn.auth_prompts();
    let u = url("http://ni.test/auth/x");
    let e = svn.info(&u, &Rev::Head, Priority::Normal).wait().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(prompts.is_empty());

    svn.set_credentials(&u.host_key(), creds("alice", "hunter2"));
    let e = svn.info(&u, &Rev::Head, Priority::Normal).wait().unwrap_err();
    assert_eq!(e.kind, ErrorKind::Auth);
    assert!(e.command.contains("--username alice --password-from-stdin"));
    assert!(!e.command.contains("hunter2"));
    assert!(!format!("{e:?}").contains("hunter2"));

    svn.set_credentials(&u.host_key(), creds("alice", "secret"));
    svn.info(&u, &Rev::Head, Priority::Normal).wait().unwrap();
    assert!(prompts.is_empty());
}

// ---------------------------------------------------------------------------
// Certificates

#[test]
fn cert_single_prompt_trust_and_retry() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        interactive: true,
        ..f.config()
    });
    let prompts = svn.cert_prompts();
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let seen = seen.clone();
        thread::spawn(move || {
            while let Ok(p) = prompts.recv_blocking() {
                seen.lock()
                    .unwrap()
                    .push((p.host.clone(), p.failures.clone(), p.message.clone()));
                thread::sleep(Duration::from_millis(100));
                p.respond(true);
            }
        });
    }
    let jobs: Vec<_> = (0..4)
        .map(|i| svn.info(&url(&format!("https://cert.test/cert/{i}")), &Rev::Head, Priority::Normal))
        .collect();
    for j in jobs {
        j.wait().unwrap();
    }
    svn.info(&url("https://cert.test/cert/later"), &Rev::Head, Priority::Normal)
        .wait()
        .unwrap();
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, "cert.test:443");
    assert_eq!(seen[0].1, vec!["unknown-ca".to_owned(), "cn-mismatch".to_owned()]);
    assert!(seen[0].2.contains("different hostname"));
    assert!(f.log("argv").contains("--trust-server-cert-failures=unknown-ca,cn-mismatch"));
}

#[test]
fn cert_declined_and_non_interactive() {
    let f = Fake::new();
    let svn = Svn::new(SvnConfig {
        interactive: true,
        ..f.config()
    });
    let prompts = svn.cert_prompts();
    thread::spawn(move || {
        while let Ok(p) = prompts.recv_blocking() {
            p.respond(false);
        }
    });
    let e = svn
        .info(&url("https://nocert.test/cert/x"), &Rev::Head, Priority::Normal)
        .wait()
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cert);

    let svn = Svn::new(f.config());
    let e = svn
        .info(&url("https://nocert.test/cert/x"), &Rev::Head, Priority::Normal)
        .wait()
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cert);
    assert!(svn.cert_prompts().is_empty());
}
