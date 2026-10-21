//! The diff window: file sidebar fed by the streamed diff, and a unified or
//! side-by-side view of the selected file (DESIGN.md §7).

use super::limit::{self, Budget, Incoming, Resume};
use super::model::{self, FileStatus, FileSummary, LARGE_ROWS};
use super::view::{Column, DiffView};
use crate::app::Ctx;
use crate::common;
use adw::prelude::*;
use gtk::{gio, glib, pango};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;
use svn_backend::url::decode;
use svn_backend::{DiffContent, DiffSpec, FileDiff, Priority, RepoUrl, Rev, RevNum, SvnError};

/// Files rendering to more rows than this are built on a worker thread.
const ASYNC_ROWS: usize = 5_000;

/// One sidebar entry (stored in a `glib::BoxedAnyObject`).
pub struct FileItem {
    pub diff: Arc<FileDiff>,
    pub summary: FileSummary,
    /// The user confirmed rendering this large file.
    pub show_large: Cell<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BannerAction {
    /// The diff failed part way: run it again.
    Retry,
    /// Loading paused at the size limit: load the rest.
    LoadRest,
}

/// A built render model, ready to put into the views.
enum Built {
    Unified(model::Rendered),
    Split(model::Split),
}

fn build(diff: &FileDiff, side_by_side: bool) -> Built {
    if side_by_side {
        Built::Split(model::split(diff))
    } else {
        Built::Unified(model::unified(diff))
    }
}

struct Ui {
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    /// "loading" (nothing received yet), "main", "status".
    outer: gtk::Stack,
    outer_status: adw::Bin,
    store: gio::ListStore,
    selection: gtk::SingleSelection,
    list_view: gtk::ListView,
    count_label: gtk::Label,
    spinner: adw::Spinner,
    cancel_button: gtk::Button,
    banner: adw::Banner,
    /// "unified", "split", "status", "loading".
    stack: gtk::Stack,
    status: adw::Bin,
    unified: DiffView,
    unified_sw: gtk::ScrolledWindow,
    left: DiffView,
    right: DiffView,
    left_sw: gtk::ScrolledWindow,
    right_sw: gtk::ScrolledWindow,
}

pub struct DiffWindow {
    ctx: Rc<Ctx>,
    spec: DiffSpec,
    ui: Ui,
    /// The streaming diff; aborting it drops the stream, which kills svn.
    task: RefCell<Option<glib::JoinHandle<()>>>,
    /// Background work for the selected file (render, binary details).
    file_task: RefCell<Option<glib::JoinHandle<()>>>,
    side_by_side: Cell<bool>,
    /// Bumped whenever the shown file or mode changes; stale async results
    /// compare against it.
    generation: Cell<u64>,
    started: Cell<Instant>,
    first_file_at: Cell<Option<Instant>>,
    /// Size of the received diff against the §7.3 limit.
    budget: Cell<Budget>,
    /// The user chose to load the whole diff.
    unlimited: Cell<bool>,
    /// Set while re-running the diff to load the rest after a pause.
    resume: Cell<Option<Resume>>,
    /// What the banner's button does.
    banner_action: Cell<BannerAction>,
    #[cfg(debug_assertions)]
    pub(super) debug: RefCell<Option<super::debug::Hooks>>,
}

/// Display path of a URL: its decoded path after the host.
fn url_path(url: &RepoUrl) -> String {
    let s = url.as_str();
    let rest = s.split_once("://").map_or(s, |(_, r)| r);
    match rest.find('/') {
        Some(i) => decode(&rest[i..]),
        None => "/".to_owned(),
    }
}

fn spec_url(spec: &DiffSpec) -> &RepoUrl {
    match spec {
        DiffSpec::Change { url, .. } | DiffSpec::ChangeAt { url, .. } | DiffSpec::Range { url, .. } => url,
    }
}

/// `r1234 — /trunk/src` or `r10:r20 — /trunk/foo.c`.
fn title_for(spec: &DiffSpec, path: &str) -> String {
    match spec {
        DiffSpec::Change { rev, .. } | DiffSpec::ChangeAt { rev, .. } => format!("r{rev} — {path}"),
        DiffSpec::Range { from, to, .. } => format!("r{from}:r{to} — {path}"),
    }
}

/// URL of a file in the diff. svn prints paths relative to the target;
/// when the target is a file, the path is just its name.
fn file_url(spec: &DiffSpec, path: &str) -> RepoUrl {
    let base = spec_url(spec);
    if path.is_empty() || path == "." || (!path.contains('/') && base.name() == path) {
        base.clone()
    } else {
        base.join(path)
    }
}

/// Old and new revision of the two sides.
fn sides(spec: &DiffSpec) -> (RevNum, RevNum) {
    match *spec {
        DiffSpec::Change { rev, .. } | DiffSpec::ChangeAt { rev, .. } => (rev.saturating_sub(1), rev),
        DiffSpec::Range { from, to, .. } => (from, to),
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn status_page(icon: &str, title: &str, description: &str) -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(glib::markup_escape_text(description))
        .build()
}

fn item_at(model: &impl IsA<gio::ListModel>, pos: u32) -> Option<glib::BoxedAnyObject> {
    model.item(pos).and_downcast::<glib::BoxedAnyObject>()
}

impl DiffWindow {
    pub fn open(ctx: &Rc<Ctx>, spec: DiffSpec) -> Rc<Self> {
        let ui = build_ui(ctx, &spec);
        let this = Rc::new(Self {
            ctx: ctx.clone(),
            spec,
            ui,
            task: RefCell::new(None),
            file_task: RefCell::new(None),
            side_by_side: Cell::new(false),
            generation: Cell::new(0),
            started: Cell::new(Instant::now()),
            first_file_at: Cell::new(None),
            budget: Cell::new(Budget::unlimited()),
            unlimited: Cell::new(false),
            resume: Cell::new(None),
            banner_action: Cell::new(BannerAction::Retry),
            #[cfg(debug_assertions)]
            debug: RefCell::new(None),
        });
        this.connect_signals();
        this.install_actions();
        this.resolve_title();
        this.start();
        this.ui.window.present();
        this
    }

    fn connect_signals(self: &Rc<Self>) {
        let ui = &self.ui;
        // The window owns the state; it is released (and the stream
        // cancelled) when the window is destroyed.
        let keep = RefCell::new(Some(self.clone()));
        ui.window.connect_destroy(move |_| {
            if let Some(this) = keep.take() {
                this.stop_task();
                if let Some(t) = this.file_task.take() {
                    t.abort();
                }
            }
        });

        let weak = Rc::downgrade(self);
        ui.selection.connect_selected_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.show_selected();
            }
        });

        let weak = Rc::downgrade(self);
        ui.cancel_button.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.cancel();
            }
        });

        let weak = Rc::downgrade(self);
        ui.banner.connect_button_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                match this.banner_action.get() {
                    BannerAction::Retry => this.start(),
                    BannerAction::LoadRest => this.load_rest(),
                }
            }
        });
    }

    fn install_actions(self: &Rc<Self>) {
        let window = &self.ui.window;

        let split = gio::SimpleAction::new_stateful("side-by-side", None, &false.to_variant());
        let weak = Rc::downgrade(self);
        split.connect_change_state(move |action, value| {
            let (Some(this), Some(value)) = (weak.upgrade(), value) else {
                return;
            };
            action.set_state(value);
            this.side_by_side.set(value.get::<bool>().unwrap_or(false));
            this.show_selected();
        });
        window.add_action(&split);

        for (name, delta) in [("prev-file", -1i64), ("next-file", 1)] {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.step(delta);
                }
            });
            action.set_enabled(false);
            window.add_action(&action);
        }

        // Window-local shortcuts, in the capture phase so the text views
        // (which bind Alt+Up/Down and Ctrl+PageUp/Down) don't eat them.
        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
        for (trigger, action) in [
            ("<Control>t", "win.side-by-side"),
            ("<Alt>Up|<Control>Page_Up", "win.prev-file"),
            ("<Alt>Down|<Control>Page_Down", "win.next-file"),
        ] {
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(trigger),
                Some(gtk::NamedAction::new(action)),
            ));
        }
        window.add_controller(shortcuts);
    }

    /// Replace the URL path in the title by the repository path once known.
    fn resolve_title(self: &Rc<Self>) {
        let peg = match self.spec {
            DiffSpec::Change { rev, .. } => rev,
            DiffSpec::ChangeAt { peg, .. } | DiffSpec::Range { peg, .. } => peg,
        };
        let job = self
            .ctx
            .svn
            .info(spec_url(&self.spec), &Rev::Number(peg), Priority::Normal);
        let weak = Rc::downgrade(self);
        common::spawn(async move {
            let Ok(info) = job.result().await else { return };
            if let Some(this) = weak.upgrade() {
                let title = title_for(&this.spec, &info.repos_path);
                this.ui.title.set_title(&title);
                this.ui.window.set_title(Some(&title));
            }
        });
    }

    fn stop_task(&self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }

    /// (Re)start the streaming diff from scratch.
    fn start(self: &Rc<Self>) {
        self.ui.store.remove_all();
        self.resume.set(None);
        self.ui.outer.set_visible_child_name("loading");
        self.ui.count_label.set_label("Loading…");
        self.run();
    }

    /// Loading paused at the size limit: run svn again without a limit,
    /// skipping the files already in the list.
    pub(super) fn load_rest(self: &Rc<Self>) {
        self.unlimited.set(true);
        self.resume.set(Some(Resume::new(self.ui.store.n_items())));
        self.ui
            .count_label
            .set_label(&files_label(self.ui.store.n_items(), "…"));
        self.run();
    }

    fn run(self: &Rc<Self>) {
        self.stop_task();
        let ui = &self.ui;
        ui.banner.set_revealed(false);
        ui.spinner.set_visible(true);
        ui.cancel_button.set_visible(true);
        self.started.set(Instant::now());
        self.first_file_at.set(None);
        self.budget.set(if self.unlimited.get() {
            Budget::unlimited()
        } else {
            limit_budget()
        });

        let mut stream = self.ctx.svn.diff(&self.spec, Priority::Interactive);
        let weak = Rc::downgrade(self);
        let task = glib::spawn_future_local(async move {
            let mut error = None;
            let mut paused = false;
            while let Some(batch) = stream.next().await {
                let Some(this) = weak.upgrade() else { return };
                match batch {
                    Ok(files) => {
                        if !this.append(files) {
                            paused = true;
                            break;
                        }
                    }
                    Err(err) => {
                        error = Some(err);
                        break;
                    }
                }
            }
            // Dropping the stream stops svn.
            drop(stream);
            if let Some(this) = weak.upgrade() {
                if paused {
                    this.pause();
                } else {
                    this.finish(error);
                }
            }
        });
        self.task.replace(Some(task));
    }

    /// Add received files to the list; `false` once the size limit is
    /// reached (the stream should stop).
    fn append(self: &Rc<Self>, files: Vec<FileDiff>) -> bool {
        let ui = &self.ui;
        let mut budget = self.budget.get();
        let mut resume = self.resume.get();
        let mut objects = Vec::with_capacity(files.len());
        let mut more = true;
        for f in files {
            if let Some(r) = &mut resume {
                let store = &ui.store;
                let same = |i| {
                    item_at(store, i).is_some_and(|o| o.borrow::<FileItem>().diff.path == f.path)
                };
                match r.next(same) {
                    Incoming::Skip => continue,
                    Incoming::Keep => {}
                    Incoming::Diverged => {
                        // svn's output changed since the first run: drop
                        // the stored files from here on.
                        let kept = r.kept();
                        store.splice(kept, store.n_items() - kept, &[] as &[glib::Object]);
                    }
                }
            }
            if budget.exceeded() {
                more = false;
                break;
            }
            budget.add(&f);
            objects.push(glib::BoxedAnyObject::new(FileItem {
                summary: model::summarize(&f),
                diff: Arc::new(f),
                show_large: Cell::new(false),
            }));
        }
        self.budget.set(budget);
        self.resume.set(resume);
        if objects.is_empty() {
            return more;
        }
        let first = ui.store.n_items() == 0;
        ui.store.extend_from_slice(&objects);
        ui.count_label
            .set_label(&files_label(ui.store.n_items(), "…"));
        if first || self.first_file_at.get().is_none() {
            self.first_file_at.set(Some(Instant::now()));
        }
        if first {
            ui.outer.set_visible_child_name("main");
        }
        self.update_nav();
        more
    }

    /// The size limit was reached and svn stopped (DESIGN.md §7.3).
    fn pause(self: &Rc<Self>) {
        self.task.take();
        let ui = &self.ui;
        ui.spinner.set_visible(false);
        ui.cancel_button.set_visible(false);
        let n = ui.store.n_items();
        ui.count_label.set_label(&files_label(n, " (paused)"));
        ui.banner
            .set_title(&glib::markup_escape_text(&large_diff_title(
                n,
                self.budget.get().bytes,
            )));
        ui.banner.set_button_label(Some("Load the _Rest"));
        self.banner_action.set(BannerAction::LoadRest);
        ui.banner.set_revealed(true);
        #[cfg(debug_assertions)]
        super::debug::finished(self);
    }

    fn finish(self: &Rc<Self>, error: Option<SvnError>) {
        self.task.take();
        let ui = &self.ui;
        ui.spinner.set_visible(false);
        ui.cancel_button.set_visible(false);
        let n = ui.store.n_items();
        ui.count_label.set_label(&files_label(n, ""));
        match error {
            Some(err) if err.is_cancelled() => self.show_stopped(),
            Some(err) if n == 0 => {
                let weak = Rc::downgrade(self);
                let retry: Box<dyn Fn()> = Box::new(move || {
                    if let Some(this) = weak.upgrade() {
                        this.start();
                    }
                });
                ui.outer_status
                    .set_child(Some(&common::error_page(&err, Some(retry))));
                ui.outer.set_visible_child_name("status");
            }
            Some(err) => {
                ui.count_label.set_label(&files_label(n, " (incomplete)"));
                ui.banner.set_button_label(Some("_Retry"));
                self.banner_action.set(BannerAction::Retry);
                ui.banner.set_title(&glib::markup_escape_text(&format!(
                    "The diff is incomplete: {}",
                    err.to_string().lines().next().unwrap_or_default()
                )));
                ui.banner.set_revealed(true);
            }
            None if n == 0 => {
                let page = status_page(
                    "text-x-generic-symbolic",
                    "No Changes",
                    "There are no differences to show",
                );
                ui.outer_status.set_child(Some(&page));
                ui.outer.set_visible_child_name("status");
            }
            None => {}
        }
        #[cfg(debug_assertions)]
        super::debug::finished(self);
    }

    /// The Cancel button.
    fn cancel(self: &Rc<Self>) {
        self.stop_task();
        let ui = &self.ui;
        ui.spinner.set_visible(false);
        ui.cancel_button.set_visible(false);
        self.show_stopped();
    }

    fn show_stopped(self: &Rc<Self>) {
        let ui = &self.ui;
        let n = ui.store.n_items();
        ui.count_label.set_label(&files_label(n, " (stopped)"));
        if n == 0 {
            let page = status_page(
                "process-stop-symbolic",
                "Cancelled",
                "Loading the diff was cancelled",
            );
            let button = gtk::Button::builder()
                .label("_Retry")
                .use_underline(true)
                .halign(gtk::Align::Center)
                .css_classes(["pill", "suggested-action"])
                .build();
            let weak = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.start();
                }
            });
            page.set_child(Some(&button));
            ui.outer_status.set_child(Some(&page));
            ui.outer.set_visible_child_name("status");
        }
    }

    fn update_nav(&self) {
        let n = self.ui.store.n_items();
        let pos = self.ui.selection.selected();
        let has = pos != gtk::INVALID_LIST_POSITION;
        let set = |name: &str, enabled: bool| {
            if let Some(a) = self
                .ui
                .window
                .lookup_action(name)
                .and_downcast::<gio::SimpleAction>()
            {
                a.set_enabled(enabled);
            }
        };
        set("prev-file", has && pos > 0);
        set("next-file", has && pos + 1 < n);
    }

    fn step(&self, delta: i64) {
        let n = i64::from(self.ui.store.n_items());
        let pos = self.ui.selection.selected();
        if pos == gtk::INVALID_LIST_POSITION || n == 0 {
            return;
        }
        let next = (i64::from(pos) + delta).clamp(0, n - 1) as u32;
        if next != pos {
            self.select(next);
        }
    }

    pub(super) fn select(&self, pos: u32) {
        self.ui.list_view.scroll_to(
            pos,
            gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
            None,
        );
        self.ui.selection.set_selected(pos);
    }

    #[cfg(debug_assertions)]
    pub(super) fn set_side_by_side(&self, on: bool) {
        self.ui
            .window
            .change_action_state("side-by-side", &on.to_variant());
    }

    #[cfg(debug_assertions)]
    pub(super) fn gtk_window(&self) -> &adw::ApplicationWindow {
        &self.ui.window
    }

    /// Loading is paused at the size limit.
    #[cfg(debug_assertions)]
    pub(super) fn is_paused(&self) -> bool {
        self.task.borrow().is_none()
            && self.ui.banner.is_revealed()
            && self.banner_action.get() == BannerAction::LoadRest
    }

    #[cfg(debug_assertions)]
    pub(super) fn selected_item(&self) -> Option<glib::BoxedAnyObject> {
        item_at(&self.ui.selection, self.ui.selection.selected())
    }

    /// Time to the first file, total time, number of files.
    #[cfg(debug_assertions)]
    pub(super) fn timings(&self) -> (std::time::Duration, std::time::Duration, u32) {
        let started = self.started.get();
        let first = self
            .first_file_at
            .get()
            .map_or_else(Default::default, |t| t - started);
        (first, started.elapsed(), self.ui.store.n_items())
    }

    /// Render the selected file in the current mode.
    pub(super) fn show_selected(self: &Rc<Self>) {
        self.update_nav();
        self.generation.set(self.generation.get() + 1);
        if let Some(t) = self.file_task.take() {
            t.abort();
        }
        let ui = &self.ui;
        let Some(obj) = item_at(&ui.selection, ui.selection.selected()) else {
            ui.unified.clear();
            ui.left.clear();
            ui.right.clear();
            return;
        };
        let item = obj.borrow::<FileItem>();
        let diff = item.diff.clone();
        let status = item.summary.status;

        if let DiffContent::Binary = diff.content {
            self.show_binary(&diff);
            return;
        }
        if matches!(diff.content, DiffContent::None) && diff.prop_changes.is_empty() {
            let description = match status {
                FileStatus::Added => "An empty file was added",
                FileStatus::Deleted => "An empty file was deleted",
                _ => "This file has no content or property changes",
            };
            self.show_status(status_page(
                "text-x-generic-symbolic",
                "No Changes",
                description,
            ));
            return;
        }
        let rows = model::estimate_rows(&diff);
        if rows > large_rows() && !item.show_large.get() {
            self.show_large_prompt(&obj, rows);
            return;
        }
        drop(item);

        let side_by_side = self.side_by_side.get();
        let started = Instant::now();
        if rows <= ASYNC_ROWS {
            self.apply(&diff, build(&diff, side_by_side), started);
            return;
        }
        // Big file: build the model off the main thread.
        ui.stack.set_visible_child_name("loading");
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        let task = glib::spawn_future_local(async move {
            let d = diff.clone();
            let Ok(built) = gio::spawn_blocking(move || build(&d, side_by_side)).await else {
                return;
            };
            if let Some(this) = weak.upgrade()
                && this.generation.get() == generation
            {
                this.apply(&diff, built, started);
            }
        });
        self.file_task.replace(Some(task));
    }

    fn apply(&self, diff: &FileDiff, built: Built, started: Instant) {
        let ui = &self.ui;
        let language = common::language_for(file_name(&diff.path));
        let rows = match built {
            Built::Unified(r) => {
                let rows = r.rows.len();
                ui.left.clear();
                ui.right.clear();
                ui.unified.set_content(r, language.as_ref());
                ui.stack.set_visible_child_name("unified");
                reset_scroll(&ui.unified_sw);
                rows
            }
            Built::Split(s) => {
                let rows = s.left.rows.len();
                ui.unified.clear();
                ui.left.set_content(s.left, language.as_ref());
                ui.right.set_content(s.right, language.as_ref());
                ui.stack.set_visible_child_name("split");
                reset_scroll(&ui.left_sw);
                ui.right_sw.hadjustment().set_value(0.0);
                rows
            }
        };
        #[cfg(debug_assertions)]
        super::debug::rendered(self, rows, started);
        let _ = (rows, started);
    }

    fn show_status(&self, page: adw::StatusPage) {
        self.ui.status.set_child(Some(&page));
        self.ui.stack.set_visible_child_name("status");
    }

    fn show_large_prompt(self: &Rc<Self>, obj: &glib::BoxedAnyObject, rows: usize) {
        let page = status_page(
            "dialog-warning-symbolic",
            "Large Diff",
            &format!("This file's diff is large ({rows} lines). Show anyway?"),
        );
        let button = gtk::Button::builder()
            .label("_Show Anyway")
            .use_underline(true)
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        let weak = Rc::downgrade(self);
        let obj = obj.downgrade();
        button.connect_clicked(move |_| {
            if let (Some(this), Some(obj)) = (weak.upgrade(), obj.upgrade()) {
                obj.borrow::<FileItem>().show_large.set(true);
                this.show_selected();
            }
        });
        page.set_child(Some(&button));
        self.show_status(page);
    }

    /// "Binary file changed", with sizes and MIME type fetched in the
    /// background (best effort).
    fn show_binary(self: &Rc<Self>, diff: &FileDiff) {
        let page = status_page(
            "application-x-executable-symbolic",
            "Binary File Changed",
            &diff.path,
        );
        let details = gtk::Label::builder()
            .label("Loading details…")
            .justify(gtk::Justification::Center)
            .wrap(true)
            .selectable(true)
            .css_classes(["dim-label"])
            .build();
        page.set_child(Some(&details));
        self.show_status(page);

        let url = file_url(&self.spec, &diff.path);
        let (old_rev, new_rev) = sides(&self.spec);
        let svn = &self.ctx.svn;
        let old = svn.info(&url, &Rev::Number(old_rev), Priority::Normal);
        let new = svn.info(&url, &Rev::Number(new_rev), Priority::Normal);
        let props = svn.proplist(&url, new_rev, Priority::Normal);
        let prop_lines: Vec<String> = diff
            .prop_changes
            .iter()
            .map(|p| {
                format!(
                    "{}: {} → {}",
                    p.name,
                    p.old.as_deref().unwrap_or("(none)"),
                    p.new.as_deref().unwrap_or("(none)")
                )
            })
            .collect();
        let details = details.downgrade();
        let task = glib::spawn_future_local(async move {
            let size = |r: Result<svn_backend::Info, SvnError>| {
                r.ok()
                    .and_then(|i| i.size)
                    .map_or_else(|| "—".to_owned(), common::format_size)
            };
            let old = size(old.result().await);
            let new = size(new.result().await);
            let mime = props.result().await.ok().and_then(|p| {
                p.into_iter()
                    .find(|p| p.name == "svn:mime-type")
                    .map(|p| p.value)
            });
            let Some(details) = details.upgrade() else {
                return;
            };
            let mut text = format!("Old size: {old}  ·  New size: {new}");
            if let Some(mime) = mime {
                text.push_str(&format!("\nMIME type: {}", mime.trim()));
            }
            for line in prop_lines {
                text.push('\n');
                text.push_str(&line);
            }
            details.set_label(&text);
        });
        self.file_task.replace(Some(task));
    }
}

/// The §7.3 limit (debug builds: `GSVN_DIFF_LIMIT=BYTES[:LINES]`).
fn limit_budget() -> Budget {
    #[cfg(debug_assertions)]
    if let Some(v) = std::env::var("GSVN_DIFF_LIMIT")
        .ok()
        .filter(|v| !v.is_empty())
    {
        let (bytes, lines) = v.split_once(':').unwrap_or((&v, ""));
        return Budget::new(
            bytes.parse().unwrap_or(limit::LIMIT_BYTES),
            lines.parse().unwrap_or(limit::LIMIT_LINES),
        );
    }
    Budget::new(limit::LIMIT_BYTES, limit::LIMIT_LINES)
}

/// Banner text when loading paused at the limit.
fn large_diff_title(files: u32, bytes: u64) -> String {
    format!(
        "This diff is large ({}, ~{}). Load the rest?",
        files_label(files, ""),
        common::format_size(bytes)
    )
}

fn large_rows() -> usize {
    #[cfg(debug_assertions)]
    if let Some(n) = std::env::var("GSVN_DIFF_LARGE_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
    {
        return n;
    }
    LARGE_ROWS
}

fn reset_scroll(sw: &gtk::ScrolledWindow) {
    sw.vadjustment().set_value(0.0);
    sw.hadjustment().set_value(0.0);
}

fn files_label(n: u32, suffix: &str) -> String {
    match n {
        1 => format!("1 file{suffix}"),
        n => format!("{n} files{suffix}"),
    }
}

fn build_ui(ctx: &Rc<Ctx>, spec: &DiffSpec) -> Ui {
    let url = spec_url(spec);
    let title_text = title_for(spec, &url_path(url));
    let window = common::new_window(ctx, "diff", &title_text, (1200, 800));
    let mut subtitle = decode(url.as_str());
    if let DiffSpec::Range { peg, to: rev, .. } | DiffSpec::ChangeAt { peg, rev, .. } = spec
        && peg != rev
    {
        subtitle.push_str(&format!("@{peg}"));
    }
    let title = adw::WindowTitle::new(&title_text, &subtitle);

    // Header bar.
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));
    let nav = gtk::Box::builder().css_classes(["linked"]).build();
    for (icon, action, tip) in [
        ("go-up-symbolic", "win.prev-file", "Previous File (Alt+↑)"),
        ("go-down-symbolic", "win.next-file", "Next File (Alt+↓)"),
    ] {
        nav.append(
            &gtk::Button::builder()
                .icon_name(icon)
                .action_name(action)
                .tooltip_text(tip)
                .build(),
        );
    }
    header.pack_start(&nav);
    let menu = common::main_menu();
    let section = gio::Menu::new();
    section.append(Some("_Side by Side"), Some("win.side-by-side"));
    section.append(Some("_Previous File"), Some("win.prev-file"));
    section.append(Some("_Next File"), Some("win.next-file"));
    menu.prepend_section(None, &section);
    header.pack_end(&common::main_menu_button(&menu));
    header.pack_end(
        &gtk::ToggleButton::builder()
            .icon_name("view-dual-symbolic")
            .action_name("win.side-by-side")
            .tooltip_text("Side by Side (Ctrl+T)")
            .build(),
    );

    // Sidebar: file list + footer.
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::SingleSelection::builder()
        .model(&store)
        .autoselect(true)
        .can_unselect(false)
        .build();
    let list_view = gtk::ListView::builder()
        .model(&selection)
        .factory(&file_row_factory())
        .css_classes(["navigation-sidebar"])
        .build();
    let list_sw = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list_view)
        .build();
    let spinner = adw::Spinner::new();
    let count_label = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(pango::EllipsizeMode::End)
        .css_classes(["dim-label", "caption"])
        .build();
    let cancel_button = gtk::Button::builder()
        .label("Cancel")
        .css_classes(["flat", "caption"])
        .build();
    let footer = gtk::Box::builder()
        .spacing(8)
        .margin_start(12)
        .margin_end(6)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    footer.append(&spinner);
    footer.append(&count_label);
    footer.append(&cancel_button);
    let sidebar = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar.append(&list_sw);
    sidebar.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    sidebar.append(&footer);

    // Content views.
    let unified = DiffView::new();
    unified.add_line_numbers(Column::Old);
    unified.add_line_numbers(Column::New);
    let unified_sw = gtk::ScrolledWindow::builder().child(&unified).build();

    let left = DiffView::new();
    left.add_line_numbers(Column::Old);
    let right = DiffView::new();
    right.add_line_numbers(Column::New);
    // Only the right view shows a vertical scrollbar; both share it.
    let left_sw = gtk::ScrolledWindow::builder()
        .vscrollbar_policy(gtk::PolicyType::External)
        .hexpand(true)
        .child(&left)
        .build();
    let right_sw = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .child(&right)
        .build();
    right_sw.set_vadjustment(Some(&left_sw.vadjustment()));
    let split_box = gtk::Box::builder().homogeneous(true).build();
    split_box.append(&left_sw);
    split_box.append(&right_sw);
    // A separator would break `homogeneous`; draw it as a border instead.
    left_sw.add_css_class("diff-left");
    install_css();

    let status = adw::Bin::new();
    let loading = adw::Spinner::builder()
        .width_request(32)
        .height_request(32)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    let stack = gtk::Stack::builder().vexpand(true).hexpand(true).build();
    stack.add_named(&unified_sw, Some("unified"));
    stack.add_named(&split_box, Some("split"));
    stack.add_named(&status, Some("status"));
    stack.add_named(&loading, Some("loading"));

    let banner = adw::Banner::builder().button_label("_Retry").build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&banner);
    content.append(&stack);

    let paned = gtk::Paned::builder()
        .start_child(&sidebar)
        .end_child(&content)
        .resize_start_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(300)
        .build();

    // Outer stack: waiting for the first file / main / whole-window status.
    let outer_status = adw::Bin::new();
    let first_loading = adw::StatusPage::builder()
        .title("Loading Diff…")
        .paintable(&adw::SpinnerPaintable::new(None::<&gtk::Widget>))
        .build();
    if let Some(p) = first_loading
        .paintable()
        .and_downcast::<adw::SpinnerPaintable>()
    {
        p.set_widget(Some(&first_loading));
    }
    let outer = gtk::Stack::new();
    outer.add_named(&first_loading, Some("loading"));
    outer.add_named(&paned, Some("main"));
    outer.add_named(&outer_status, Some("status"));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&outer));
    window.set_content(Some(&toolbar));

    Ui {
        window,
        title,
        outer,
        outer_status,
        store,
        selection,
        list_view,
        count_label,
        spinner,
        cancel_button,
        banner,
        stack,
        status,
        unified,
        unified_sw,
        left,
        right,
        left_sw,
        right_sw,
    }
}

/// App-wide CSS for the diff window, installed once.
fn install_css() {
    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    if INSTALLED.replace(true) {
        return;
    }
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "scrolledwindow.diff-left { border-right: 1px solid alpha(currentColor, 0.15); }\n\
         .diff-file-row .badge { min-width: 1.4em; font-weight: bold; }\n",
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Sidebar rows: status letter, path (ellipsized at the start), +N −M.
fn file_row_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::builder()
            .spacing(8)
            .css_classes(["diff-file-row"])
            .build();
        row.append(
            &gtk::Label::builder()
                .css_classes(["badge", "monospace"])
                .build(),
        );
        row.append(
            &gtk::Label::builder()
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(pango::EllipsizeMode::Start)
                .build(),
        );
        row.append(
            &gtk::Label::builder()
                .css_classes(["success", "caption", "numeric"])
                .build(),
        );
        row.append(
            &gtk::Label::builder()
                .css_classes(["error", "caption", "numeric"])
                .build(),
        );
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let (Some(row), Some(obj)) = (
            item.child(),
            item.item().and_downcast::<glib::BoxedAnyObject>(),
        ) else {
            return;
        };
        let file = obj.borrow::<FileItem>();
        let labels: Vec<gtk::Label> =
            std::iter::successors(row.first_child(), |w| w.next_sibling())
                .filter_map(|w| w.downcast::<gtk::Label>().ok())
                .collect();
        let [badge, path, added, removed] = labels.as_slice() else {
            return;
        };
        let s = file.summary;
        badge.set_label(s.status.letter());
        for class in ["success", "error", "accent", "warning", "dim-label"] {
            badge.remove_css_class(class);
        }
        badge.add_css_class(match s.status {
            FileStatus::Added => "success",
            FileStatus::Deleted => "error",
            FileStatus::Modified => "accent",
            FileStatus::Binary => "warning",
            FileStatus::PropsOnly | FileStatus::Empty => "dim-label",
        });
        path.set_label(&file.diff.path);
        let mut tip = format!("{}\n{}", file.diff.path, s.status.description());
        if s.props > 0 {
            tip.push_str(&format!(
                ", {} property change{}",
                s.props,
                if s.props == 1 { "" } else { "s" }
            ));
        }
        row.set_tooltip_text(Some(&tip));
        added.set_label(&format!("+{}", s.added));
        removed.set_label(&format!("−{}", s.removed));
        added.set_visible(s.added > 0);
        removed.set_visible(s.removed > 0);
    });
    factory
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_and_urls() {
        let url = RepoUrl::parse("https://h/svn/repo/trunk/my%20dir").unwrap();
        assert_eq!(url_path(&url), "/svn/repo/trunk/my dir");
        let change = DiffSpec::Change {
            url: url.clone(),
            rev: 1234,
        };
        assert_eq!(title_for(&change, "/trunk/my dir"), "r1234 — /trunk/my dir");
        let range = DiffSpec::Range {
            url: url.clone(),
            peg: 30,
            from: 10,
            to: 20,
        };
        assert_eq!(title_for(&range, "/trunk/foo.c"), "r10:r20 — /trunk/foo.c");
        assert_eq!(sides(&change), (1233, 1234));
        assert_eq!(sides(&range), (10, 20));

        assert_eq!(
            file_url(&change, "sub/a b.c").as_str(),
            "https://h/svn/repo/trunk/my%20dir/sub/a%20b.c"
        );
        assert_eq!(file_url(&change, "my dir").as_str(), url.as_str());
        assert_eq!(file_url(&change, ".").as_str(), url.as_str());
        let file = DiffSpec::Change {
            url: RepoUrl::parse("https://h/r/Makefile").unwrap(),
            rev: 2,
        };
        assert_eq!(file_url(&file, "Makefile").as_str(), "https://h/r/Makefile");

        assert_eq!(files_label(1, ""), "1 file");
        assert!(large_diff_title(3, 6_000_000).starts_with("This diff is large (3 files, ~6"));
        assert_eq!(files_label(3, "…"), "3 files…");
        assert_eq!(file_name("a/b/c.rs"), "c.rs");
    }
}
