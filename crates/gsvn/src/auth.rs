//! Credential and certificate prompts (DESIGN.md §9).
//!
//! [`install`] spawns loops on `Svn::auth_prompts` / `Svn::cert_prompts`
//! that look up the keyring and show dialogs on the active window. Prompts
//! of each kind are handled one at a time.

use crate::app::Ctx;
use adw::prelude::*;
use gtk::glib;
use std::collections::HashSet;
use std::future::Future;
use std::rc::Rc;
use svn_backend::{AuthPrompt, CertPrompt, Credentials};

/// `application` attribute of keyring items written by gsvn.
const KEYRING_APP: &str = "gsvn";

pub fn install(ctx: &Rc<Ctx>) {
    let auth_rx = ctx.svn.auth_prompts();
    let cert_rx = ctx.svn.cert_prompts();

    let app = ctx.app.clone();
    glib::spawn_future_local(async move {
        let store = keyring::SecretStore::new(KEYRING_APP);
        let mut from_keyring = HashSet::new();
        while let Ok(prompt) = auth_rx.recv().await {
            let req = AuthRequest::from_prompt(&prompt);
            let creds = resolve_credentials(&store, &mut from_keyring, &req, |req| {
                let app = app.clone();
                async move { ask_credentials(&app, &req).await }
            })
            .await;
            prompt.respond(creds);
        }
    });

    let app = ctx.app.clone();
    glib::spawn_future_local(async move {
        while let Ok(prompt) = cert_rx.recv().await {
            let trust = ask_cert_trust(&app, &CertRequest::from_prompt(&prompt)).await;
            prompt.respond(trust);
        }
    });

    #[cfg(debug_assertions)]
    debug_prompts(&ctx.app);
}

/// The parts of an [`AuthPrompt`] the dialog and keyring logic need
/// (an `AuthPrompt` itself can only be created by the backend).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRequest {
    pub host_key: String,
    pub url: String,
    pub previous_username: Option<String>,
    pub rejected: bool,
}

impl AuthRequest {
    fn from_prompt(p: &AuthPrompt) -> Self {
        Self {
            host_key: p.host_key.clone(),
            url: p.url.to_string(),
            previous_username: p.previous_username.clone(),
            rejected: p.rejected,
        }
    }
}

/// What the user entered in the credentials dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthAnswer {
    pub credentials: Credentials,
    pub remember: bool,
}

/// Credential storage (the keyring), abstracted so tests can use a fake.
pub trait CredentialStore {
    fn lookup(&self, host_key: &str) -> impl Future<Output = Option<Credentials>>;
    /// Store `credentials` for `host_key`, replacing any existing entry.
    fn store(&self, host_key: &str, credentials: &Credentials) -> impl Future<Output = ()>;
    fn delete(&self, host_key: &str) -> impl Future<Output = ()>;
}

/// Decide the answer to one auth prompt:
///
/// 1. Unless the prompt says earlier credentials were rejected, use the
///    keyring entry for the host if there is one.
/// 2. If credentials that came from the keyring were rejected, delete them.
/// 3. Otherwise ask the user (`ask`), storing the answer if *Remember*.
///
/// `from_keyring` tracks hosts whose current credentials came from (or were
/// saved to) the keyring, so a rejection only deletes entries we used.
pub async fn resolve_credentials<S, F, Fut>(
    store: &S,
    from_keyring: &mut HashSet<String>,
    req: &AuthRequest,
    ask: F,
) -> Option<Credentials>
where
    S: CredentialStore,
    F: FnOnce(AuthRequest) -> Fut,
    Fut: Future<Output = Option<AuthAnswer>>,
{
    if req.rejected {
        if from_keyring.remove(&req.host_key) {
            store.delete(&req.host_key).await;
        }
    } else if let Some(creds) = store.lookup(&req.host_key).await {
        from_keyring.insert(req.host_key.clone());
        return Some(creds);
    }

    let answer = ask(req.clone()).await?;
    if answer.remember {
        store.store(&req.host_key, &answer.credentials).await;
        from_keyring.insert(req.host_key.clone());
    } else {
        from_keyring.remove(&req.host_key);
    }
    Some(answer.credentials)
}

/// `https://svn.example.com:443` → `svn.example.com` (default ports and the
/// scheme are dropped for display).
pub fn display_host(host_key: &str) -> String {
    let (scheme, rest) = host_key.split_once("://").unwrap_or(("", host_key));
    let default_port = match scheme {
        "https" => Some(":443"),
        "http" => Some(":80"),
        "svn" => Some(":3690"),
        _ => None,
    };
    let rest = rest.trim_end_matches('/');
    match default_port {
        Some(port) => rest.strip_suffix(port).unwrap_or(rest).to_owned(),
        None => rest.to_owned(),
    }
}

fn active_window(app: &adw::Application) -> Option<gtk::Window> {
    app.active_window()
}

/// Show the "Authentication Required" dialog. `None` = cancelled.
pub async fn ask_credentials(app: &adw::Application, req: &AuthRequest) -> Option<AuthAnswer> {
    let host = display_host(&req.host_key);
    let dialog = adw::AlertDialog::builder()
        .heading("Authentication Required")
        .body(format!("Enter a username and password for {host} to access\n{}", req.url))
        .build();
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("login", "_Log In");
    dialog.set_response_appearance("login", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("login"));
    dialog.set_close_response("cancel");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    if req.rejected {
        let error = gtk::Label::builder()
            .label("The username or password was not accepted. Please try again.")
            .wrap(true)
            .css_classes(["error"])
            .build();
        content.append(&error);
    }
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let username = adw::EntryRow::builder()
        .title("Username")
        .activates_default(true)
        .build();
    username.set_text(req.previous_username.as_deref().unwrap_or(""));
    let password = adw::PasswordEntryRow::builder()
        .title("Password")
        .activates_default(true)
        .build();
    let remember = adw::SwitchRow::builder().title("Remember Password").build();
    list.append(&username);
    list.append(&password);
    list.append(&remember);
    content.append(&list);
    dialog.set_extra_child(Some(&content));

    let update_enabled = {
        let dialog = dialog.downgrade();
        move |row: &adw::EntryRow| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_response_enabled("login", !row.text().trim().is_empty());
            }
        }
    };
    update_enabled(&username);
    username.connect_changed(update_enabled);

    if username.text().is_empty() {
        dialog.set_focus(Some(&username));
    } else {
        dialog.set_focus(Some(&password));
    }

    let response = dialog.choose_future(active_window(app).as_ref()).await;
    if response != "login" {
        return None;
    }
    Some(AuthAnswer {
        credentials: Credentials {
            username: username.text().trim().to_owned(),
            password: password.text().to_string(),
        },
        remember: remember.is_active(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertRequest {
    pub host: String,
    pub url: String,
    pub failures: Vec<String>,
    pub message: String,
}

impl CertRequest {
    fn from_prompt(p: &CertPrompt) -> Self {
        Self {
            host: p.host.clone(),
            url: p.url.to_string(),
            failures: p.failures.clone(),
            message: p.message.clone(),
        }
    }
}

/// Plain-English text for an svn `--trust-server-cert-failures` name.
pub fn cert_failure_text(failure: &str) -> String {
    match failure {
        "unknown-ca" => "The certificate is not issued by a trusted authority".to_owned(),
        "cn-mismatch" => "The certificate’s hostname does not match the server".to_owned(),
        "expired" => "The certificate has expired".to_owned(),
        "not-yet-valid" => "The certificate is not yet valid".to_owned(),
        "other" => "The certificate has an unknown problem".to_owned(),
        other => format!("Certificate problem: {other}"),
    }
}

/// Body text of the certificate dialog.
pub fn cert_body(req: &CertRequest) -> String {
    let host = req.host.strip_suffix(":443").unwrap_or(&req.host);
    let mut body = format!("The identity of {host} could not be verified.\n");
    for failure in &req.failures {
        body.push_str("\n• ");
        body.push_str(&cert_failure_text(failure));
    }
    let message = req.message.trim();
    if !message.is_empty() {
        body.push_str("\n\n");
        body.push_str(message);
    }
    body.push_str(
        "\n\nOnly continue if you trust this server. The certificate will be trusted until gsvn quits.",
    );
    body
}

/// Show the "Untrusted Certificate" dialog. Returns true to trust the
/// certificate for this session.
pub async fn ask_cert_trust(app: &adw::Application, req: &CertRequest) -> bool {
    let dialog = adw::AlertDialog::builder()
        .heading("Untrusted Certificate")
        .body(cert_body(req))
        .build();
    dialog.add_response("cancel", "_Cancel");
    dialog.add_response("trust", "_Trust for This Session");
    dialog.set_response_appearance("trust", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.choose_future(active_window(app).as_ref()).await == "trust"
}

/// `GSVN_DEBUG_PROMPTS=1 cargo run` shows the dialogs with fake data (the
/// keyring is not touched) and prints the answers: an auth prompt, a
/// "rejected" auth prompt, then a certificate prompt. `=rejected` or
/// `=cert` starts at that one.
#[cfg(debug_assertions)]
fn debug_prompts(app: &adw::Application) {
    let Ok(start) = std::env::var("GSVN_DEBUG_PROMPTS") else {
        return;
    };
    let app = app.clone();
    glib::spawn_future_local(async move {
        glib::timeout_future(std::time::Duration::from_millis(500)).await;
        let auth_rounds: &[bool] = match start.as_str() {
            "cert" => &[],
            "rejected" => &[true],
            _ => &[false, true],
        };
        for &rejected in auth_rounds {
            let req = AuthRequest {
                host_key: "https://svn.example.com:443".into(),
                url: "https://svn.example.com/repos/project/trunk".into(),
                previous_username: rejected.then(|| "alice".into()),
                rejected,
            };
            let answer = ask_credentials(&app, &req).await;
            eprintln!(
                "debug auth answer: {:?}",
                answer.map(|a| (a.credentials.username, a.remember))
            );
        }
        let req = CertRequest {
            host: "svn.example.com:443".into(),
            url: "https://svn.example.com/repos/project".into(),
            failures: vec!["unknown-ca".into(), "cn-mismatch".into()],
            message: "Server SSL certificate verification failed: issuer is not trusted".into(),
        };
        eprintln!("debug cert answer: {}", ask_cert_trust(&app, &req).await);
    });
}

/// The real keyring, via the Secret Service (`oo7`; GNOME Keyring on
/// GNOME). oo7's D-Bus connection runs on its own internal executor, so
/// its futures can be awaited on the glib main loop.
#[allow(clippy::result_large_err)] // oo7::Error; never on a hot path
pub mod keyring {
    use super::CredentialStore;
    use gtk::glib;
    use std::cell::RefCell;
    use std::rc::Rc;
    use svn_backend::Credentials;

    pub struct SecretStore {
        app: &'static str,
        keyring: RefCell<Option<Rc<oo7::Keyring>>>,
    }

    impl SecretStore {
        /// `app` is the `application` attribute; tests use a distinct one.
        pub fn new(app: &'static str) -> Self {
            Self {
                app,
                keyring: RefCell::new(None),
            }
        }

        async fn keyring(&self) -> Option<Rc<oo7::Keyring>> {
            if let Some(k) = self.keyring.borrow().clone() {
                return Some(k);
            }
            match oo7::Keyring::new().await {
                Ok(k) => {
                    let k = Rc::new(k);
                    *self.keyring.borrow_mut() = Some(k.clone());
                    Some(k)
                }
                Err(e) => {
                    glib::g_warning!("gsvn", "Keyring unavailable: {e}");
                    None
                }
            }
        }

        async fn try_lookup(&self, host_key: &str) -> oo7::Result<Option<Credentials>> {
            let Some(keyring) = self.keyring().await else {
                return Ok(None);
            };
            let items = keyring
                .search_items(&[("application", self.app), ("host", host_key)])
                .await?;
            for item in items {
                if item.is_locked().await? {
                    item.unlock().await?;
                }
                let attrs = item.attributes().await?;
                let Some(username) = attrs.get("username").filter(|u| !u.is_empty()) else {
                    continue;
                };
                let secret = item.secret().await?;
                let Ok(password) = std::str::from_utf8(&secret) else {
                    continue;
                };
                return Ok(Some(Credentials {
                    username: username.clone(),
                    password: password.to_owned(),
                }));
            }
            Ok(None)
        }

        async fn try_store(&self, host_key: &str, credentials: &Credentials) -> oo7::Result<()> {
            let Some(keyring) = self.keyring().await else {
                return Ok(());
            };
            keyring
                .delete(&[("application", self.app), ("host", host_key)])
                .await?;
            keyring
                .create_item(
                    &format!("{}: {host_key}", self.app),
                    &[
                        ("application", self.app),
                        ("host", host_key),
                        ("username", credentials.username.as_str()),
                    ],
                    credentials.password.as_str(),
                    true,
                )
                .await
        }

        async fn try_delete(&self, host_key: &str) -> oo7::Result<()> {
            let Some(keyring) = self.keyring().await else {
                return Ok(());
            };
            keyring
                .delete(&[("application", self.app), ("host", host_key)])
                .await
        }
    }

    impl CredentialStore for SecretStore {
        async fn lookup(&self, host_key: &str) -> Option<Credentials> {
            self.try_lookup(host_key).await.unwrap_or_else(|e| {
                glib::g_warning!("gsvn", "Keyring lookup failed: {e}");
                None
            })
        }

        async fn store(&self, host_key: &str, credentials: &Credentials) {
            if let Err(e) = self.try_store(host_key, credentials).await {
                glib::g_warning!("gsvn", "Cannot save password in keyring: {e}");
            }
        }

        async fn delete(&self, host_key: &str) {
            if let Err(e) = self.try_delete(host_key).await {
                glib::g_warning!("gsvn", "Cannot delete password from keyring: {e}");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Touches the real Secret Service under `application = gsvn-test`
        /// and cleans up. Run with `cargo test -- --ignored keyring`.
        #[test]
        #[ignore]
        fn real_keyring_roundtrip() {
            let ctx = glib::MainContext::new();
            ctx.block_on(async {
                let store = SecretStore::new("gsvn-test");
                let host = "https://gsvn-test.invalid:443";
                let creds = Credentials {
                    username: "tester".into(),
                    password: "s3cret ✓".into(),
                };
                store.delete(host).await;
                assert_eq!(store.lookup(host).await, None);
                store.store(host, &creds).await;
                assert_eq!(store.lookup(host).await, Some(creds.clone()));
                let creds2 = Credentials {
                    username: "other".into(),
                    password: "x".into(),
                };
                store.store(host, &creds2).await;
                assert_eq!(store.lookup(host).await, Some(creds2));
                store.delete(host).await;
                assert_eq!(store.lookup(host).await, None);
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    #[derive(Default)]
    struct FakeStore {
        items: RefCell<HashMap<String, Credentials>>,
        log: RefCell<Vec<String>>,
    }

    impl CredentialStore for FakeStore {
        async fn lookup(&self, host_key: &str) -> Option<Credentials> {
            self.log.borrow_mut().push(format!("lookup {host_key}"));
            self.items.borrow().get(host_key).cloned()
        }
        async fn store(&self, host_key: &str, credentials: &Credentials) {
            self.log.borrow_mut().push(format!("store {host_key}"));
            self.items.borrow_mut().insert(host_key.into(), credentials.clone());
        }
        async fn delete(&self, host_key: &str) {
            self.log.borrow_mut().push(format!("delete {host_key}"));
            self.items.borrow_mut().remove(host_key);
        }
    }

    const HOST: &str = "https://h:443";

    fn creds(u: &str) -> Credentials {
        Credentials {
            username: u.into(),
            password: format!("{u}-pw"),
        }
    }

    fn req(rejected: bool) -> AuthRequest {
        AuthRequest {
            host_key: HOST.into(),
            url: "https://h/r".into(),
            previous_username: None,
            rejected,
        }
    }

    fn run(
        store: &FakeStore,
        seen: &mut HashSet<String>,
        rejected: bool,
        answer: Option<AuthAnswer>,
    ) -> (Option<Credentials>, bool) {
        let asked = std::cell::Cell::new(false);
        let result = glib::MainContext::new().block_on(resolve_credentials(
            store,
            seen,
            &req(rejected),
            |_| {
                asked.set(true);
                std::future::ready(answer)
            },
        ));
        (result, asked.get())
    }

    #[test]
    fn keyring_hit_skips_dialog() {
        let store = FakeStore::default();
        store.items.borrow_mut().insert(HOST.into(), creds("alice"));
        let mut seen = HashSet::new();
        assert_eq!(run(&store, &mut seen, false, None), (Some(creds("alice")), false));
    }

    #[test]
    fn remember_stores_and_rejection_deletes() {
        let store = FakeStore::default();
        let mut seen = HashSet::new();
        let answer = AuthAnswer {
            credentials: creds("bob"),
            remember: true,
        };
        assert_eq!(run(&store, &mut seen, false, Some(answer)), (Some(creds("bob")), true));
        assert_eq!(store.items.borrow().get(HOST), Some(&creds("bob")));

        // Rejected: stored entry is removed and the user asked again; cancel.
        assert_eq!(run(&store, &mut seen, true, None), (None, true));
        assert!(store.items.borrow().is_empty());
        assert_eq!(
            *store.log.borrow(),
            vec![format!("lookup {HOST}"), format!("store {HOST}"), format!("delete {HOST}")]
        );
    }

    #[test]
    fn rejection_of_unsaved_credentials_keeps_keyring() {
        let store = FakeStore::default();
        let mut seen = HashSet::new();
        let answer = AuthAnswer {
            credentials: creds("carol"),
            remember: false,
        };
        assert_eq!(run(&store, &mut seen, false, Some(answer.clone())), (Some(creds("carol")), true));
        assert!(store.items.borrow().is_empty());
        // Someone else saved an entry meanwhile; a rejection of typed
        // credentials must not delete it, and must not use it either.
        store.items.borrow_mut().insert(HOST.into(), creds("dave"));
        assert_eq!(run(&store, &mut seen, true, Some(answer)), (Some(creds("carol")), true));
        assert_eq!(store.items.borrow().get(HOST), Some(&creds("dave")));
    }

    #[test]
    fn host_display() {
        assert_eq!(display_host("https://svn.example.com:443"), "svn.example.com");
        assert_eq!(display_host("http://svn.example.com:80"), "svn.example.com");
        assert_eq!(display_host("https://svn.example.com:8443"), "svn.example.com:8443");
        assert_eq!(display_host("svn.example.com"), "svn.example.com");
    }

    #[test]
    fn cert_texts() {
        assert_eq!(
            cert_failure_text("unknown-ca"),
            "The certificate is not issued by a trusted authority"
        );
        assert_eq!(cert_failure_text("expired"), "The certificate has expired");
        assert_eq!(cert_failure_text("weird"), "Certificate problem: weird");
        let body = cert_body(&CertRequest {
            host: "svn.example.com:443".into(),
            url: "https://svn.example.com/r".into(),
            failures: vec!["unknown-ca".into(), "cn-mismatch".into()],
            message: "issuer is not trusted\n".into(),
        });
        assert!(body.starts_with("The identity of svn.example.com could"), "{body}");
        assert!(body.contains("• The certificate is not issued by a trusted authority\n• The certificate’s hostname"));
        assert!(body.contains("\n\nissuer is not trusted\n\n"), "{body}");
    }
}
