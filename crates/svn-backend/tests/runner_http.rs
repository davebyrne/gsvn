//! Tests against the docker Apache test server (anonymous
//! `http://localhost:8080/svn/<repo>`, Basic auth `http://localhost:8080/auth/<repo>`
//! user `test`/`test`, self-signed `https://localhost:8443/svn/<repo>`).
//!
//! Skipped unless `GSVN_TEST_HTTP=1`.

use std::sync::{Arc, Mutex};
use std::thread;
use svn_backend::*;

fn enabled() -> bool {
    std::env::var("GSVN_TEST_HTTP").as_deref() == Ok("1")
}

macro_rules! require_http {
    () => {
        if !enabled() {
            eprintln!("skipped: set GSVN_TEST_HTTP=1");
            return;
        }
    };
}

fn svn(dir: &tempfile::TempDir) -> Svn {
    Svn::new(SvnConfig {
        config_dir: Some(dir.path().to_owned()),
        interactive: true,
        ..SvnConfig::default()
    })
}

fn url(s: &str) -> RepoUrl {
    RepoUrl::parse(s).unwrap()
}

const P: Priority = Priority::Normal;

#[test]
fn anonymous_info_and_list() {
    require_http!();
    let d = tempfile::tempdir().unwrap();
    let svn = svn(&d);
    let u = url("http://localhost:8080/svn/small");
    let i = svn.info(&u, &Rev::Head, P).wait().unwrap();
    assert!(i.rev > 0);
    let entries = svn.list(&u, i.rev, P).collect_blocking().unwrap();
    assert!(!entries.is_empty());
}

#[test]
fn basic_auth_prompt_and_retry() {
    require_http!();
    let d = tempfile::tempdir().unwrap();
    let svn = svn(&d);
    let prompts = svn.auth_prompts();
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let seen = seen.clone();
        thread::spawn(move || {
            while let Ok(p) = prompts.recv_blocking() {
                let n = {
                    let mut s = seen.lock().unwrap();
                    s.push((p.host_key.clone(), p.rejected));
                    s.len()
                };
                let pw = if n == 1 { "bad" } else { "test" };
                p.respond(Some(Credentials {
                    username: "test".into(),
                    password: pw.into(),
                }));
            }
        });
    }
    let jobs: Vec<_> = (0..3)
        .map(|_| svn.info(&url("http://localhost:8080/auth/small"), &Rev::Head, P))
        .collect();
    for j in jobs {
        let i = j.wait().unwrap();
        assert!(i.rev > 0);
    }
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("http://localhost:8080".to_owned(), false),
            ("http://localhost:8080".to_owned(), true)
        ]
    );
    // Nothing was written to the (temporary) auth cache.
    assert!(!d.path().join("auth/svn.simple").exists()
        || std::fs::read_dir(d.path().join("auth/svn.simple")).unwrap().count() == 0);
}

#[test]
fn basic_auth_declined() {
    require_http!();
    let d = tempfile::tempdir().unwrap();
    let svn = svn(&d);
    let prompts = svn.auth_prompts();
    thread::spawn(move || {
        while let Ok(p) = prompts.recv_blocking() {
            p.respond(None);
        }
    });
    let e = svn
        .info(&url("http://localhost:8080/auth/small"), &Rev::Head, P)
        .wait()
        .unwrap_err();
    assert_eq!((e.kind, e.code), (ErrorKind::Auth, Some(215004)));
}

fn cert_case(u: &str, want: &[&str]) {
    let d = tempfile::tempdir().unwrap();
    let svn = svn(&d);
    let prompts = svn.cert_prompts();
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let seen = seen.clone();
        thread::spawn(move || {
            while let Ok(p) = prompts.recv_blocking() {
                seen.lock().unwrap().push((p.host.clone(), p.failures.clone()));
                p.respond(true);
            }
        });
    }
    let i = svn.info(&url(u), &Rev::Head, P).wait().unwrap();
    assert!(i.rev > 0);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, want);
}

#[test]
fn self_signed_cert_prompt() {
    require_http!();
    cert_case("https://localhost:8443/svn/small", &["unknown-ca"]);
    cert_case("https://127.0.0.1:8443/svn/small", &["unknown-ca", "cn-mismatch"]);
}

#[test]
fn self_signed_cert_non_interactive() {
    require_http!();
    let d = tempfile::tempdir().unwrap();
    let svn = Svn::new(SvnConfig {
        config_dir: Some(d.path().to_owned()),
        interactive: false,
        ..SvnConfig::default()
    });
    let e = svn
        .info(&url("https://localhost:8443/svn/small"), &Rev::Head, P)
        .wait()
        .unwrap_err();
    assert_eq!(e.kind, ErrorKind::Cert);
}

#[test]
fn connection_refused_is_network() {
    require_http!();
    let d = tempfile::tempdir().unwrap();
    let svn = svn(&d);
    let e = svn
        .info(&url("http://localhost:1/svn/small"), &Rev::Head, P)
        .wait()
        .unwrap_err();
    assert_eq!((e.kind, e.code), (ErrorKind::Network, Some(111)));
}
