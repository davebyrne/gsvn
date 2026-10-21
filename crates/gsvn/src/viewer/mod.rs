//! File content and blame window (DESIGN.md §8).
//!
//! One window per file revision. The content is fetched with `svn cat`
//! (after `svn info` for the size and `svn proplist` for `svn:mime-type`)
//! and shown in a read-only GtkSourceView. Blame is a toggle in the same
//! window: `svn blame --xml` is streamed into a [`blame_model::BlameModel`]
//! and drawn by a custom gutter renderer next to the line numbers.

mod blame_model;
#[cfg(debug_assertions)]
pub mod debug;
mod decode;
mod gutter;
mod search;

use crate::app::Ctx;
use crate::common;
use adw::prelude::*;
use crate::common::group_thousands;
use blame_model::BlameModel;
use gtk::{gdk, gio, glib};
use gutter::BlameGutter;
use search::Search;
use sourceview5::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use svn_backend::{DiffSpec, NodeKind, Priority, Property, RepoUrl, Rev, RevNum, SvnError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerMode {
    Content,
    Blame,
}

/// Open `url@rev` in a viewer window. `size` is the file size if already
/// known (from a listing), used for the large-file confirmation.
pub fn open(ctx: &Rc<Ctx>, url: RepoUrl, rev: RevNum, size: Option<u64>, mode: ViewerMode) {
    Viewer::new(ctx, url, rev, size, mode);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContentState {
    Loading,
    Text,
    /// A status page: binary, large-file confirmation, error, cancelled.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlameState {
    Idle,
    Running,
    Done,
    Cancelled,
    Failed,
}

struct Viewer {
    ctx: Rc<Ctx>,
    /// The file's URL at `rev`; changes when Blame Previous Revision
    /// crosses a rename.
    url: RefCell<RepoUrl>,
    name: RefCell<String>,
    rev: Cell<RevNum>,
    size: Cell<Option<u64>>,
    mime: RefCell<Option<String>>,
    mode: Cell<ViewerMode>,
    /// Full file contents once loaded (for Save As).
    data: RefCell<Option<glib::Bytes>>,
    content: Cell<ContentState>,
    text_lines: Cell<usize>,
    blame: gutter::SharedModel,
    blame_state: Cell<BlameState>,
    blame_start: Cell<Option<RevNum>>,
    /// When the running blame started (for the elapsed-time display).
    blame_started: Cell<Option<std::time::Instant>>,
    /// The content is known to be text, so blame may run.
    blame_allowed: Cell<bool>,
    load_task: RefCell<Option<glib::JoinHandle<()>>>,
    blame_task: RefCell<Option<glib::JoinHandle<()>>>,
    save_task: RefCell<Option<glib::JoinHandle<()>>>,

    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    status_bin: adw::Bin,
    view: sourceview5::View,
    buffer: sourceview5::Buffer,
    gutter: BlameGutter,
    line_menu: gtk::PopoverMenu,
    search: Rc<Search>,
    content_button: gtk::ToggleButton,
    blame_button: gtk::ToggleButton,
    goto_button: gtk::MenuButton,
    goto_entry: gtk::Entry,
    status_bar: gtk::CenterBox,
    encoding_label: gtk::Label,
    info_label: gtk::Label,
    blame_box: gtk::Box,
    blame_spinner: adw::Spinner,
    blame_label: gtk::Label,
    blame_cancel: gtk::Button,
    blame_restart: gtk::Button,
    since_button: gtk::MenuButton,
    since_entry: gtk::Entry,
    banner: adw::Banner,
}

impl Viewer {
    fn new(ctx: &Rc<Ctx>, url: RepoUrl, rev: RevNum, size: Option<u64>, mode: ViewerMode) -> Rc<Self> {
        let name = url.name();
        let window = common::new_window(ctx, "viewer", &name, (960, 760));

        // Header bar.
        let content_button = gtk::ToggleButton::builder()
            .label("_Content")
            .use_underline(true)
            .tooltip_text("Show File Content")
            .active(true)
            .build();
        let blame_button = gtk::ToggleButton::builder()
            .label("_Blame")
            .use_underline(true)
            .tooltip_text("Show Who Changed Each Line")
            .group(&content_button)
            .build();
        let mode_box = gtk::Box::builder().css_classes(["linked"]).build();
        mode_box.append(&content_button);
        mode_box.append(&blame_button);
        let history_button = gtk::Button::builder()
            .icon_name("document-open-recent-symbolic")
            .tooltip_text("History")
            .action_name("viewer.history")
            .build();
        let save_button = gtk::Button::builder()
            .icon_name("document-save-as-symbolic")
            .tooltip_text("Save As… (Ctrl+S)")
            .action_name("viewer.save-as")
            .build();
        let search_button = gtk::ToggleButton::builder()
            .icon_name("edit-find-symbolic")
            .tooltip_text("Find (Ctrl+F)")
            .build();
        let goto_entry = gtk::Entry::builder()
            .placeholder_text("Line number")
            .input_purpose(gtk::InputPurpose::Digits)
            .width_chars(12)
            .build();
        let goto_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        goto_box.append(&goto_entry);
        let goto_go = gtk::Button::builder()
            .label("_Go")
            .use_underline(true)
            .css_classes(["suggested-action"])
            .build();
        goto_box.append(&goto_go);
        let goto_button = gtk::MenuButton::builder()
            .icon_name("go-jump-symbolic")
            .tooltip_text("Go to Line (Ctrl+L)")
            .popover(&gtk::Popover::builder().child(&goto_box).build())
            .build();

        let menu = common::main_menu();
        let section = gio::Menu::new();
        section.append(Some("_Find…"), Some("viewer.search"));
        section.append(Some("_Go to Line…"), Some("viewer.goto-line"));
        section.append(Some("_Wrap Lines"), Some("viewer.wrap"));
        section.append(Some("_Copy URL"), Some("viewer.copy-url"));
        menu.prepend_section(None, &section);

        let title = adw::WindowTitle::new(&name, &svn_backend::url::decode(url.as_str()));
        let header = adw::HeaderBar::builder().title_widget(&title).build();
        header.pack_start(&mode_box);
        header.pack_start(&history_button);
        header.pack_end(&common::main_menu_button(&menu));
        header.pack_end(&save_button);
        header.pack_end(&search_button);
        header.pack_end(&goto_button);

        // The source view.
        let buffer = sourceview5::Buffer::new(None);
        buffer.set_highlight_matching_brackets(false);
        buffer.set_enable_undo(false);
        common::follow_style_scheme(&buffer);
        let view = sourceview5::View::builder()
            .buffer(&buffer)
            .editable(false)
            .monospace(true)
            .show_line_numbers(true)
            .highlight_current_line(false)
            .cursor_visible(true)
            .left_margin(6)
            .top_margin(4)
            .bottom_margin(4)
            .build();
        let blame = Rc::new(RefCell::new(BlameModel::default()));
        let gutter = BlameGutter::new(blame.clone());
        gutter.set_visible(false);
        // Left of the line numbers (which sit at -30).
        ViewExt::gutter(&view, gtk::TextWindowType::Left).insert(&gutter, -40);
        let line_menu = gtk::PopoverMenu::from_model(None::<&gio::MenuModel>);
        line_menu.set_has_arrow(true);
        line_menu.set_position(gtk::PositionType::Right);
        let scrolled = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();

        let search = Search::new(&view, &buffer);
        search
            .bar
            .bind_property("search-mode-enabled", &search_button, "active")
            .bidirectional()
            .sync_create()
            .build();

        let status_bin = adw::Bin::new();
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        stack.add_named(&status_bin, Some("status"));
        stack.add_named(&scrolled, Some("text"));

        let banner = adw::Banner::builder().button_label("_Retry").build();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.append(&banner);
        body.append(&stack);
        // Not parented to the text view: there the menu was sized too
        // small and its last item cut off.
        line_menu.set_parent(&body);

        // Status bar: file info on the left, blame progress/controls right.
        let encoding_label = gtk::Label::builder().css_classes(["dim-label", "caption"]).build();
        let info_label = gtk::Label::builder().css_classes(["dim-label", "caption", "numeric"]).build();
        let info_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        info_box.append(&encoding_label);
        info_box.append(&info_label);
        let blame_spinner = adw::Spinner::new();
        let blame_label = gtk::Label::builder()
            .css_classes(["caption", "numeric"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let blame_cancel = gtk::Button::builder()
            .label("Cancel")
            .css_classes(["flat", "caption"])
            .build();
        let blame_restart = gtk::Button::builder()
            .label("Restart")
            .css_classes(["flat", "caption"])
            .build();
        let since_entry = gtk::Entry::builder()
            .placeholder_text("Revision, e.g. 1200")
            .input_purpose(gtk::InputPurpose::Digits)
            .build();
        let since_apply = gtk::Button::builder()
            .label("_Blame")
            .use_underline(true)
            .css_classes(["suggested-action"])
            .build();
        let since_all = gtk::Button::builder().label("_All History").use_underline(true).build();
        let since_buttons = gtk::Box::builder().spacing(6).homogeneous(true).build();
        since_buttons.append(&since_all);
        since_buttons.append(&since_apply);
        let since_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
        since_box.append(
            &gtk::Label::builder()
                .label("Only blame changes since revision:")
                .xalign(0.0)
                .build(),
        );
        since_box.append(&since_entry);
        since_box.append(
            &gtk::Label::builder()
                .label("Older lines are shown as “…”. Faster on long histories.")
                .css_classes(["dim-label", "caption"])
                .wrap(true)
                .max_width_chars(30)
                .xalign(0.0)
                .build(),
        );
        since_box.append(&since_buttons);
        let since_button = gtk::MenuButton::builder()
            .label("All History")
            .always_show_arrow(true)
            .tooltip_text("Blame Since Revision…")
            .css_classes(["flat", "caption"])
            .direction(gtk::ArrowType::Up)
            .popover(&gtk::Popover::builder().child(&since_box).build())
            .build();
        let blame_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        blame_box.append(&blame_spinner);
        blame_box.append(&blame_label);
        blame_box.append(&blame_cancel);
        blame_box.append(&blame_restart);
        blame_box.append(&since_button);
        blame_box.set_visible(false);
        let status_bar = gtk::CenterBox::builder()
            .start_widget(&info_box)
            .end_widget(&blame_box)
            .margin_start(12)
            .margin_end(6)
            .margin_top(2)
            .margin_bottom(2)
            .visible(false)
            .build();

        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header);
        toolbar_view.add_top_bar(&search.bar);
        toolbar_view.add_bottom_bar(&status_bar);
        toolbar_view.set_content(Some(&body));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&toolbar_view));
        window.set_content(Some(&toasts));
        search.bar.set_key_capture_widget(Some(&window));

        let this = Rc::new(Self {
            ctx: ctx.clone(),
            url: RefCell::new(url),
            name: RefCell::new(name),
            rev: Cell::new(rev),
            size: Cell::new(size),
            mime: RefCell::new(None),
            mode: Cell::new(mode),
            data: RefCell::new(None),
            content: Cell::new(ContentState::Loading),
            text_lines: Cell::new(0),
            blame,
            blame_state: Cell::new(BlameState::Idle),
            blame_start: Cell::new(None),
            blame_started: Cell::new(None),
            blame_allowed: Cell::new(false),
            load_task: RefCell::new(None),
            blame_task: RefCell::new(None),
            save_task: RefCell::new(None),
            window: window.clone(),
            title,
            toasts,
            stack,
            status_bin,
            view,
            buffer,
            gutter,
            line_menu,
            search,
            content_button,
            blame_button,
            goto_button,
            goto_entry,
            status_bar,
            encoding_label,
            info_label,
            blame_box,
            blame_spinner,
            blame_label,
            blame_cancel,
            blame_restart,
            since_button,
            since_entry,
            banner,
        });
        this.install_actions();
        this.connect_signals(&goto_go, &since_apply, &since_all);

        // The window owns the viewer; closing it cancels all jobs.
        {
            let this = this.clone();
            window.connect_destroy(move |_| {
                for task in [&this.load_task, &this.blame_task, &this.save_task] {
                    if let Some(t) = task.take() {
                        t.abort();
                    }
                }
                this.line_menu.unparent();
            });
        }

        #[cfg(debug_assertions)]
        crate::e2e::register(&window, Rc::downgrade(&this) as Weak<dyn crate::e2e::Scriptable>);
        this.update_title();
        this.set_mode(mode);
        this.load(false);
        window.present();
        this
    }

    fn weak(self: &Rc<Self>) -> Weak<Self> {
        Rc::downgrade(self)
    }

    /// The file's URL at the revision shown.
    fn url(&self) -> RepoUrl {
        self.url.borrow().clone()
    }

    /// Resolve the file's URL at an older revision `rev` (it may have been
    /// renamed or moved since) and call `f` with it; a toast reports failure.
    fn with_url_at(self: &Rc<Self>, rev: RevNum, f: impl FnOnce(&Rc<Self>, RepoUrl) + 'static) {
        let job = self.ctx.svn.info_at(&self.url(), self.rev.get(), rev, Priority::Interactive);
        let weak = self.weak();
        glib::spawn_future_local(async move {
            let result = job.result().await;
            let Some(this) = weak.upgrade() else { return };
            match result {
                Ok(info) => f(&this, info.url),
                Err(e) => this.toast(&format!("Cannot find {} at r{rev}: {}", this.name.borrow(), e.message)),
            }
        });
    }

    fn install_actions(self: &Rc<Self>) {
        let group = gio::SimpleActionGroup::new();
        let rev_param = u64::static_variant_type();
        let simple = |name: &str, f: fn(&Rc<Self>)| {
            let weak = self.weak();
            gio::ActionEntry::builder(name)
                .activate(move |_: &gio::SimpleActionGroup, _, _| {
                    if let Some(this) = weak.upgrade() {
                        f(&this);
                    }
                })
                .build()
        };
        let with_rev = |name: &str, f: fn(&Rc<Self>, RevNum)| {
            let weak = self.weak();
            gio::ActionEntry::builder(name)
                .parameter_type(Some(&rev_param))
                .activate(move |_: &gio::SimpleActionGroup, _, param| {
                    if let (Some(this), Some(rev)) = (weak.upgrade(), param.and_then(|p| p.get::<u64>())) {
                        f(&this, rev);
                    }
                })
                .build()
        };
        let wrap = {
            let weak = self.weak();
            gio::ActionEntry::builder("wrap")
                .state(false.to_variant())
                .activate(move |_: &gio::SimpleActionGroup, action, _| {
                    let on = !action.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
                    action.set_state(&on.to_variant());
                    if let Some(this) = weak.upgrade() {
                        this.view.set_wrap_mode(if on { gtk::WrapMode::WordChar } else { gtk::WrapMode::None });
                    }
                })
                .build()
        };
        group.add_action_entries([
            simple("save-as", |this| this.save_as()),
            simple("search", |this| {
                if this.content.get() == ContentState::Text {
                    this.search.open();
                }
            }),
            simple("goto-line", |this| {
                if this.content.get() == ContentState::Text {
                    this.goto_button.popup();
                }
            }),
            simple("history", |this| crate::log::open(&this.ctx, this.url(), this.rev.get())),
            simple("copy-url", |this| {
                let text = this.url().with_peg(&Rev::Number(this.rev.get()));
                this.window.clipboard().set_text(&text);
                this.toast("URL copied");
            }),
            with_rev("show-changes", |this, rev| {
                // The file may have had another name at `rev`: peg it at the
                // revision shown and let svn trace it back.
                let spec = DiffSpec::ChangeAt { url: this.url(), peg: this.rev.get(), rev };
                crate::diff::open(&this.ctx, spec);
            }),
            with_rev("show-log", |this, rev| {
                this.with_url_at(rev, move |this, url| crate::log::open(&this.ctx, url, rev));
            }),
            with_rev("blame-previous", |this, rev| {
                this.with_url_at(rev, move |this, url| this.switch_revision(url, rev));
            }),
            with_rev("copy-revision", |this, rev| {
                this.window.clipboard().set_text(&rev.to_string());
                this.toast(&format!("Copied revision {rev}"));
            }),
            wrap,
        ]);
        self.window.insert_action_group("viewer", Some(&group));

        let shortcuts = gtk::ShortcutController::new();
        for (accel, action) in [
            ("<Control>f", "viewer.search"),
            ("<Control>l", "viewer.goto-line"),
            ("<Control>g", "viewer.goto-line"),
            ("<Control>s", "viewer.save-as"),
            ("<Control><Shift>s", "viewer.save-as"),
        ] {
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(accel),
                Some(gtk::NamedAction::new(action)),
            ));
        }
        self.window.add_controller(shortcuts);
    }

    fn connect_signals(self: &Rc<Self>, goto_go: &gtk::Button, since_apply: &gtk::Button, since_all: &gtk::Button) {
        {
            let weak = self.weak();
            self.blame_button.connect_toggled(move |b| {
                if let Some(this) = weak.upgrade() {
                    this.set_mode(if b.is_active() { ViewerMode::Blame } else { ViewerMode::Content });
                }
            });
        }
        {
            let weak = self.weak();
            self.gutter.connect_line_activated(move |line| {
                if let Some(this) = weak.upgrade() {
                    this.popup_line_menu(line);
                }
            });
        }
        // Right-click on a line in blame mode: add the blame actions to the
        // text view's context menu.
        {
            let weak = self.weak();
            let click = gtk::GestureClick::builder()
                .button(gdk::BUTTON_SECONDARY)
                .propagation_phase(gtk::PropagationPhase::Capture)
                .build();
            click.connect_pressed(move |_, _, x, y| {
                let Some(this) = weak.upgrade() else { return };
                let menu = if this.mode.get() == ViewerMode::Blame {
                    let (_, by) = this
                        .view
                        .window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
                    let (iter, _) = this.view.line_at_y(by);
                    this.line_menu_model(iter.line() as usize)
                } else {
                    None
                };
                this.view.set_extra_menu(menu.as_ref());
            });
            self.view.add_controller(click);
        }
        // Go to line.
        {
            let weak = self.weak();
            let go = move || {
                if let Some(this) = weak.upgrade() {
                    this.goto_from_entry();
                }
            };
            let go2 = go.clone();
            self.goto_entry.connect_activate(move |_| go());
            goto_go.connect_clicked(move |_| go2());
            let weak = self.weak();
            self.goto_button.popover().unwrap().connect_show(move |_| {
                if let Some(this) = weak.upgrade() {
                    let n = this.text_lines.get().max(1);
                    this.goto_entry.set_placeholder_text(Some(&format!("Line (1–{n})")));
                    this.goto_entry.remove_css_class("error");
                    this.goto_entry.grab_focus();
                    this.goto_entry.select_region(0, -1);
                }
            });
        }
        // Blame controls.
        {
            let weak = self.weak();
            self.blame_cancel.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.cancel_blame();
                }
            });
            let weak = self.weak();
            self.blame_restart.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.start_blame();
                }
            });
            let weak = self.weak();
            self.banner.connect_button_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.start_blame();
                }
            });
            let weak = self.weak();
            let apply = move || {
                if let Some(this) = weak.upgrade() {
                    this.blame_since_from_entry();
                }
            };
            let apply2 = apply.clone();
            self.since_entry.connect_activate(move |_| apply());
            since_apply.connect_clicked(move |_| apply2());
            let weak = self.weak();
            since_all.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    this.since_button.popdown();
                    this.set_blame_start(None);
                }
            });
        }
    }

    // ----- Loading -----------------------------------------------------

    fn update_title(&self) {
        let title = format!("{} — r{}", self.name.borrow(), self.rev.get());
        self.title.set_title(&title);
        self.window.set_title(Some(&title));
    }

    /// (Re)load the content at the current revision. `force_large` skips
    /// the large-file confirmation.
    fn load(self: &Rc<Self>, force_large: bool) {
        self.reset_blame();
        self.blame_allowed.set(false);
        self.content.set(ContentState::Loading);
        self.data.replace(None);
        self.show_loading();
        let weak = self.weak();
        let task = glib::spawn_future_local(async move {
            match Self::load_content(weak.clone(), force_large).await {
                Ok(()) => {}
                Err(err) if err.is_cancelled() => {}
                Err(err) => {
                    if let Some(this) = weak.upgrade() {
                        this.show_error(&err, force_large);
                    }
                }
            }
        });
        if let Some(old) = self.load_task.replace(Some(task)) {
            old.abort();
        }
    }

    async fn load_content(weak: Weak<Self>, force_large: bool) -> Result<(), SvnError> {
        let Some(this) = weak.upgrade() else { return Ok(()) };
        let svn = this.ctx.svn.clone();
        let url = this.url();
        let rev = this.rev.get();
        let known_size = this.size.get();
        drop(this);

        // Start everything that is certainly needed at once; the jobs run
        // concurrently on worker threads (dropping a job cancels it).
        let info = svn.info(&url, &Rev::Number(rev), Priority::Interactive);
        let props = svn.proplist(&url, rev, Priority::Interactive);
        let small = known_size.is_some_and(|s| s <= common::PREVIEW_CONFIRM_BYTES);
        let cat = small.then(|| svn.cat(&url, rev, None, Priority::Interactive));

        let info = info.result().await?;
        let size = {
            let Some(this) = weak.upgrade() else { return Ok(()) };
            this.title.set_subtitle(&info.repos_path);
            if info.kind == NodeKind::Dir {
                return Err(SvnError::new(
                    svn_backend::ErrorKind::Other,
                    format!("{} is a directory, not a file", info.repos_path),
                ));
            }
            let size = known_size.or(info.size);
            this.size.set(size);
            size.unwrap_or(0)
        };

        let mime = mime_type(&props.result().await?);
        {
            let Some(this) = weak.upgrade() else { return Ok(()) };
            this.mime.replace(mime.clone());
            if common::is_binary(&[], mime.as_deref()) {
                this.show_binary();
                return Ok(());
            }
            if size > common::PREVIEW_CONFIRM_BYTES && !force_large {
                this.show_large(size);
                return Ok(());
            }
            this.blame_allowed.set(true);
            this.maybe_start_blame();
        }

        let cat = cat.unwrap_or_else(|| svn.cat(&url, rev, None, Priority::Interactive));
        let out = cat.result().await?;
        let fetched = std::time::Instant::now();
        let bytes = glib::Bytes::from_owned(out.data);
        let binary = common::is_binary(&bytes, mime.as_deref());
        let decoded = if binary {
            None
        } else {
            let bytes = bytes.clone();
            gio::spawn_blocking(move || decode::decode(&bytes)).await.ok().flatten()
        };
        let Some(this) = weak.upgrade() else { return Ok(()) };
        this.size.set(Some(bytes.len() as u64));
        this.data.replace(Some(bytes));
        match decoded {
            Some(decoded) => this.show_text(decoded, fetched),
            None => {
                this.blame_allowed.set(false);
                this.reset_blame();
                this.show_binary();
            }
        }
        Ok(())
    }

    fn set_status_page(&self, page: &impl IsA<gtk::Widget>) {
        self.status_bin.set_child(Some(page));
        self.stack.set_visible_child_name("status");
        self.status_bar.set_visible(false);
        self.search.bar.set_search_mode(false);
    }

    fn show_loading(self: &Rc<Self>) {
        let page = adw::StatusPage::builder()
            .title("Loading…")
            .description(glib::markup_escape_text(&self.name.borrow()))
            .build();
        page.set_paintable(Some(&adw::SpinnerPaintable::new(Some(&page))));
        let cancel = pill_button("_Cancel", false);
        let weak = self.weak();
        cancel.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.cancel_load();
            }
        });
        page.set_child(Some(&cancel));
        self.set_status_page(&page);
    }

    fn cancel_load(self: &Rc<Self>) {
        if let Some(task) = self.load_task.take() {
            task.abort();
        }
        self.reset_blame();
        self.blame_allowed.set(false);
        self.content.set(ContentState::Other);
        let page = adw::StatusPage::builder()
            .icon_name("process-stop-symbolic")
            .title("Loading Cancelled")
            .description(glib::markup_escape_text(&self.name.borrow()))
            .build();
        let load = pill_button("_Load", true);
        let weak = self.weak();
        load.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.load(true);
            }
        });
        page.set_child(Some(&load));
        self.set_status_page(&page);
    }

    fn show_error(self: &Rc<Self>, err: &SvnError, force_large: bool) {
        self.content.set(ContentState::Other);
        self.blame_allowed.set(false);
        self.reset_blame();
        let weak = self.weak();
        let page = common::error_page(
            err,
            Some(Box::new(move || {
                if let Some(this) = weak.upgrade() {
                    this.load(force_large);
                }
            })),
        );
        self.set_status_page(&page);
    }

    fn show_large(self: &Rc<Self>, size: u64) {
        self.content.set(ContentState::Other);
        let page = adw::StatusPage::builder()
            .icon_name("text-x-generic-symbolic")
            .title(format!("Large File ({})", common::format_size(size)))
            .description("Showing it may take a moment and use a lot of memory")
            .build();
        let load = pill_button("_Load Anyway", true);
        let weak = self.weak();
        load.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.load(true);
            }
        });
        let buttons = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();
        buttons.append(&load);
        buttons.append(&self.save_pill());
        page.set_child(Some(&buttons));
        self.set_status_page(&page);
    }

    fn show_binary(self: &Rc<Self>) {
        self.content.set(ContentState::Other);
        let guessed = {
            // Not loaded (svn:mime-type said binary): guess from the name
            // only; empty data would be guessed as an empty file.
            let data = self.data.borrow();
            let head = data.as_deref().map(|d| &d[..d.len().min(4096)]);
            gio::content_type_guess(Some(self.name.borrow().as_str()), head).0
        };
        let mime = display_mime(
            self.mime.borrow().as_deref(),
            gio::content_type_get_mime_type(&guessed).as_deref(),
        );
        let mut description = String::from("Binary file");
        if let Some(size) = self.size.get() {
            description.push_str(" · ");
            description.push_str(&common::format_size(size));
        }
        description.push_str(" · ");
        description.push_str(&mime);
        let icon = format!("{}-symbolic", gio::content_type_get_generic_icon_name(&guessed).unwrap_or_default());
        let icon_theme = gtk::IconTheme::for_display(&WidgetExt::display(&self.window));
        let icon = if icon_theme.has_icon(&icon) { icon } else { "application-x-executable-symbolic".to_owned() };
        let page = adw::StatusPage::builder()
            .icon_name(icon)
            .title(glib::markup_escape_text(&self.name.borrow()))
            .description(glib::markup_escape_text(&description))
            .child(&self.save_pill())
            .build();
        self.set_status_page(&page);
    }

    fn save_pill(self: &Rc<Self>) -> gtk::Button {
        let save = pill_button("_Save As…", false);
        save.set_action_name(Some("viewer.save-as"));
        save
    }

    fn show_text(self: &Rc<Self>, decoded: decode::Decoded, fetched: std::time::Instant) {
        let t0 = std::time::Instant::now();
        let language = {
            let data = self.data.borrow();
            let head = data.as_deref().map(|d| &d[..d.len().min(4096)]).unwrap_or_default();
            guess_language(&self.name.borrow(), self.mime.borrow().as_deref(), head)
        };
        self.buffer.set_language(language.as_ref());
        self.buffer.begin_irreversible_action();
        self.buffer.set_text(&decoded.text);
        self.buffer.end_irreversible_action();
        self.buffer.place_cursor(&self.buffer.start_iter());
        self.text_lines.set(decoded.lines);

        self.encoding_label.set_text(decoded.encoding.label());
        if decoded.encoding.is_fallback() {
            self.encoding_label.set_tooltip_text(Some(&format!(
                "Not valid UTF-8; shown as {}",
                decoded.encoding.label()
            )));
            self.encoding_label.add_css_class("warning");
        } else {
            self.encoding_label.set_tooltip_text(None);
            self.encoding_label.remove_css_class("warning");
        }
        let mut info = format!(
            "{} {}",
            group_thousands(decoded.lines),
            if decoded.lines == 1 { "line" } else { "lines" }
        );
        if let Some(size) = self.size.get() {
            info.push_str(" · ");
            info.push_str(&common::format_size(size));
        }
        if decoded.crlf {
            info.push_str(" · CRLF");
        }
        if let Some(lang) = &language {
            info.push_str(" · ");
            info.push_str(&lang.name());
        }
        self.info_label.set_text(&info);

        self.content.set(ContentState::Text);
        self.stack.set_visible_child_name("text");
        self.status_bar.set_visible(true);
        self.view.grab_focus();
        self.update_blame_status();

        #[cfg(debug_assertions)]
        debug::log_text_timing(fetched, t0, decoded.text.len());
        #[cfg(not(debug_assertions))]
        let _ = (fetched, t0);
    }

    /// Re-open this window at another revision (Blame Previous Revision).
    /// `url` is the file's URL at `rev`, see [`Self::with_url_at`].
    fn switch_revision(self: &Rc<Self>, url: RepoUrl, rev: RevNum) {
        if url != *self.url.borrow() {
            self.name.replace(url.name());
            self.url.replace(url);
        }
        self.rev.set(rev);
        self.size.set(None);
        if self.blame_start.get().is_some_and(|s| s > rev) {
            self.blame_start.set(None);
            self.since_button.set_label("All History");
        }
        self.update_title();
        self.load(false);
    }

    // ----- Blame -------------------------------------------------------

    fn set_mode(self: &Rc<Self>, mode: ViewerMode) {
        self.mode.set(mode);
        let blame = mode == ViewerMode::Blame;
        if self.blame_button.is_active() != blame {
            if blame {
                self.blame_button.set_active(true);
            } else {
                self.content_button.set_active(true);
            }
        }
        self.gutter.set_visible(blame);
        self.blame_box.set_visible(blame);
        if !blame {
            self.banner.set_revealed(false);
        } else if self.blame_state.get() == BlameState::Failed {
            self.banner.set_revealed(true);
        }
        self.maybe_start_blame();
    }

    fn maybe_start_blame(self: &Rc<Self>) {
        if self.mode.get() == ViewerMode::Blame
            && self.blame_allowed.get()
            && self.blame_state.get() == BlameState::Idle
        {
            self.start_blame();
        }
    }

    fn abort_blame(&self) {
        if let Some(task) = self.blame_task.take() {
            task.abort();
        }
    }

    /// Forget all blame data (content is reloading).
    fn reset_blame(&self) {
        self.abort_blame();
        self.blame.borrow_mut().clear();
        self.blame_state.set(BlameState::Idle);
        self.banner.set_revealed(false);
        self.gutter.model_changed();
        self.update_blame_status();
    }

    fn start_blame(self: &Rc<Self>) {
        if !self.blame_allowed.get() {
            return;
        }
        self.abort_blame();
        self.blame.borrow_mut().clear();
        self.gutter.set_max_rev(self.rev.get());
        self.gutter.set_start_rev(self.blame_start.get());
        self.gutter.model_changed();
        self.blame_state.set(BlameState::Running);
        let started = std::time::Instant::now();
        self.blame_started.set(Some(started));
        self.banner.set_revealed(false);
        self.update_blame_status();
        // svn prints blame lines only once it has walked the history, which
        // can take minutes: tick the elapsed time meanwhile.
        {
            let weak = self.weak();
            glib::timeout_add_seconds_local(1, move || match weak.upgrade() {
                Some(this)
                    if this.blame_state.get() == BlameState::Running
                        && this.blame_started.get() == Some(started) =>
                {
                    this.update_blame_status();
                    glib::ControlFlow::Continue
                }
                _ => glib::ControlFlow::Break,
            });
        }

        let mut stream = self
            .ctx
            .svn
            .blame(&self.url(), self.rev.get(), self.blame_start.get(), Priority::Interactive);
        let weak = self.weak();
        let task = glib::spawn_future_local(async move {
            #[cfg(debug_assertions)]
            let started = std::time::Instant::now();
            loop {
                let batch = stream.next().await;
                let Some(this) = weak.upgrade() else { return };
                match batch {
                    Some(Ok(lines)) => {
                        {
                            let mut model = this.blame.borrow_mut();
                            for line in lines {
                                model.push(line);
                            }
                        }
                        this.gutter.model_changed();
                        this.update_blame_status();
                    }
                    None => {
                        #[cfg(debug_assertions)]
                        eprintln!(
                            "gsvn viewer: blame of {} lines took {:?}",
                            this.blame.borrow().received(),
                            started.elapsed()
                        );
                        this.blame_state.set(BlameState::Done);
                        this.update_blame_status();
                        return;
                    }
                    Some(Err(err)) if err.is_cancelled() => return,
                    Some(Err(err)) => {
                        this.blame_failed(&err);
                        return;
                    }
                }
            }
        });
        self.blame_task.replace(Some(task));
    }

    fn cancel_blame(&self) {
        if self.blame_state.get() == BlameState::Running {
            self.abort_blame();
            self.blame_state.set(BlameState::Cancelled);
            self.update_blame_status();
        }
    }

    fn blame_failed(&self, err: &SvnError) {
        self.blame_state.set(BlameState::Failed);
        self.blame_task.take();
        self.banner
            .set_title(&glib::markup_escape_text(&format!("Blame failed: {}", err.message)));
        self.banner.set_revealed(self.mode.get() == ViewerMode::Blame);
        self.update_blame_status();
    }

    fn set_blame_start(self: &Rc<Self>, start: Option<RevNum>) {
        self.blame_start.set(start);
        self.since_button.set_label(&match start {
            Some(s) => format!("Since r{s}"),
            None => "All History".to_owned(),
        });
        if self.blame_allowed.get() {
            self.start_blame();
        }
    }

    fn blame_since_from_entry(self: &Rc<Self>) {
        let rev = self.rev.get();
        match common::parse_rev_input(&self.since_entry.text()) {
            Ok(start) if start.is_none_or(|s| s <= rev) => {
                self.since_entry.remove_css_class("error");
                self.since_button.popdown();
                self.set_blame_start(start);
            }
            _ => {
                self.since_entry.add_css_class("error");
                self.since_entry.set_tooltip_text(Some(&format!("Enter a revision up to {rev}")));
            }
        }
    }

    fn update_blame_status(&self) {
        let state = self.blame_state.get();
        let model = self.blame.borrow();
        let since = self.blame_start.get().map(|s| format!(" since r{s}")).unwrap_or_default();
        let mut warning = false;
        let text = match state {
            BlameState::Idle => String::new(),
            BlameState::Running => match model.received() {
                0 => {
                    let secs = self.blame_started.get().map_or(0, |t| t.elapsed().as_secs());
                    if secs < 2 {
                        format!("Blaming{since}…")
                    } else {
                        format!("Blaming{since}… {}", format_elapsed(secs))
                    }
                }
                n => format!("Blaming{since}… {} lines", group_thousands(n)),
            },
            BlameState::Done => {
                let n = model.revision_count();
                let mut s = format!("{} {}{since}", group_thousands(n), if n == 1 { "revision" } else { "revisions" });
                if self.content.get() == ContentState::Text
                    && let Some((blame, text)) = model.mismatch(self.text_lines.get())
                {
                    warning = true;
                    s.push_str(&format!(
                        " · blame has {} lines, file {}",
                        group_thousands(blame),
                        group_thousands(text)
                    ));
                }
                s
            }
            BlameState::Cancelled => format!("Blame cancelled after {} lines", group_thousands(model.received())),
            BlameState::Failed => "Blame failed".to_owned(),
        };
        self.blame_label.set_text(&text);
        if warning || state == BlameState::Failed {
            self.blame_label.add_css_class("warning");
        } else {
            self.blame_label.remove_css_class("warning");
        }
        let running = state == BlameState::Running;
        self.blame_spinner.set_visible(running);
        self.blame_cancel.set_visible(running);
        self.blame_restart.set_visible(matches!(state, BlameState::Cancelled | BlameState::Failed));
        self.blame_restart
            .set_label(if state == BlameState::Failed { "Retry" } else { "Restart" });
    }

    /// The blame actions for a 0-based line, if it is attributed to a revision.
    fn line_menu_model(&self, line: usize) -> Option<gio::Menu> {
        let rev = self.blame.borrow().commit(line)?.rev;
        let item = |label: String, action: &str, rev: RevNum| {
            let item = gio::MenuItem::new(Some(&label), None);
            item.set_action_and_target_value(Some(action), Some(&rev.to_variant()));
            item
        };
        let menu = gio::Menu::new();
        let section = gio::Menu::new();
        section.append_item(&item(format!("Show Changes in r{rev}"), "viewer.show-changes", rev));
        section.append_item(&item(format!("Show Log from r{rev}"), "viewer.show-log", rev));
        menu.append_section(None, &section);
        if rev > 1 {
            let section = gio::Menu::new();
            section.append_item(&item(
                format!("Blame Previous Revision (r{})", rev - 1),
                "viewer.blame-previous",
                rev - 1,
            ));
            menu.append_section(None, &section);
        }
        let section = gio::Menu::new();
        section.append_item(&item("Copy Revision Number".to_owned(), "viewer.copy-revision", rev));
        menu.append_section(None, &section);
        Some(menu)
    }

    /// Pop up the blame actions next to a gutter cell.
    fn popup_line_menu(&self, line: u32) {
        let Some(menu) = self.line_menu_model(line as usize) else { return };
        let Some(iter) = self.buffer.iter_at_line(line as i32) else { return };
        let loc = self.view.iter_location(&iter);
        let (_, y) = self
            .view
            .buffer_to_window_coords(gtk::TextWindowType::Widget, 0, loc.y());
        let width = self.gutter.width();
        let Some(parent) = self.line_menu.parent() else { return };
        let Some(at) = self.view.compute_point(&parent, &gtk::graphene::Point::new(0.0, y as f32)) else { return };
        self.line_menu.set_menu_model(Some(&menu));
        self.line_menu.set_pointing_to(Some(&gdk::Rectangle::new(
            at.x() as i32,
            at.y() as i32,
            width,
            loc.height().max(1),
        )));
        self.line_menu.popup();
    }

    // ----- Other actions -----------------------------------------------

    fn goto_from_entry(&self) {
        let n = self.text_lines.get().max(1);
        match self.goto_entry.text().trim().parse::<usize>() {
            Ok(line) if (1..=n).contains(&line) => {
                self.goto_entry.remove_css_class("error");
                self.goto_button.popdown();
                self.goto_line(line);
            }
            _ => self.goto_entry.add_css_class("error"),
        }
    }

    /// Move the cursor to a 1-based line and scroll it into view.
    fn goto_line(&self, line: usize) {
        let Some(iter) = self.buffer.iter_at_line(line.saturating_sub(1) as i32) else { return };
        self.buffer.place_cursor(&iter);
        // An xalign of 0 would put column 0 at the left edge and scroll the
        // left margin out of view; offset it by the margin instead.
        let width = self.view.visible_rect().width().max(1);
        let xalign = (f64::from(self.view.left_margin() + 2) / f64::from(width)).min(1.0);
        scroll_to_insert(&self.view, xalign, 0.3);
        self.view.grab_focus();
    }

    fn toast(&self, text: &str) {
        self.toasts.add_toast(adw::Toast::new(&glib::markup_escape_text(text)));
    }

    fn save_as(self: &Rc<Self>) {
        let dialog = gtk::FileDialog::builder()
            .title("Save As")
            .initial_name(self.name.borrow().as_str())
            .modal(true)
            .build();
        let weak = self.weak();
        let task = glib::spawn_future_local(async move {
            let window = match weak.upgrade() {
                Some(this) => this.window.clone(),
                None => return,
            };
            let Ok(file) = dialog.save_future(Some(&window)).await else { return };
            Self::write_file(weak, file).await;
        });
        if let Some(old) = self.save_task.replace(Some(task)) {
            old.abort();
        }
    }
}

impl Viewer {
    /// Write the file contents to `file`, downloading them first if they
    /// were not loaded (binary or large file). Reports with a toast.
    async fn write_file(weak: Weak<Self>, file: gio::File) {
        let Some(this) = weak.upgrade() else { return };
        let data = this.data.borrow().clone();
        let (svn, url, rev, name) = (this.ctx.svn.clone(), this.url(), this.rev.get(), this.name.borrow().clone());
        drop(this);
        let bytes = match data {
            Some(bytes) => bytes,
            None => {
                if let Some(this) = weak.upgrade() {
                    this.toast(&format!("Downloading “{name}”…"));
                }
                match svn.cat(&url, rev, None, Priority::Normal).result().await {
                    Ok(out) => glib::Bytes::from_owned(out.data),
                    Err(err) => {
                        if !err.is_cancelled()
                            && let Some(this) = weak.upgrade()
                        {
                            this.toast(&format!("Could not download “{name}”: {}", err.message));
                        }
                        return;
                    }
                }
            }
        };
        let result = file
            .replace_contents_future(bytes, None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
            .await;
        let Some(this) = weak.upgrade() else { return };
        let shown = file.basename().map(|p| p.display().to_string()).unwrap_or(name);
        match result {
            Ok(_) => this.toast(&format!("Saved “{shown}”")),
            Err((_, err)) => this.toast(&format!("Could not save “{shown}”: {}", err.message())),
        }
    }
}

/// `42 s`, `3:07`.
fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs} s")
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Scroll the cursor into view, and again once the view is idle: while a
/// large buffer is still being validated, line heights are estimates and
/// the first scroll can land far off.
fn scroll_to_insert(view: &sourceview5::View, xalign: f64, yalign: f64) {
    view.scroll_to_mark(&view.buffer().get_insert(), 0.0, true, xalign, yalign);
    let view = view.downgrade();
    glib::idle_add_local_full(glib::Priority::LOW, move || {
        if let Some(view) = view.upgrade() {
            view.scroll_to_mark(&view.buffer().get_insert(), 0.0, true, xalign, yalign);
        }
        glib::ControlFlow::Break
    });
}

fn pill_button(label: &str, suggested: bool) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(label)
        .use_underline(true)
        .halign(gtk::Align::Center)
        .css_classes(["pill"])
        .build();
    if suggested {
        button.add_css_class("suggested-action");
    }
    button
}

fn mime_type(props: &[Property]) -> Option<String> {
    props
        .iter()
        .find(|p| p.name == "svn:mime-type")
        .map(|p| p.value.trim().to_owned())
        .filter(|m| !m.is_empty())
}

/// The MIME type to show for a binary file: `svn:mime-type`, unless it is
/// missing or the generic `application/octet-stream` (what svn sets on any
/// binary file it adds), then the type guessed by GIO.
fn display_mime(svn_mime: Option<&str>, guessed: Option<&str>) -> String {
    const GENERIC: &str = "application/octet-stream";
    match (svn_mime.filter(|m| !m.is_empty()), guessed) {
        (Some(m), _) if m != GENERIC => m.to_owned(),
        (_, Some(g)) if !g.is_empty() && g != "application/x-zerosize" => g.to_owned(),
        (Some(m), _) => m.to_owned(),
        _ => GENERIC.to_owned(),
    }
}

/// Language from the file name, then from a text `svn:mime-type`, then from
/// the content type GIO guesses (which also looks at the data, e.g. `#!`).
fn guess_language(name: &str, mime: Option<&str>, head: &[u8]) -> Option<sourceview5::Language> {
    if let Some(lang) = common::language_for(name) {
        return Some(lang);
    }
    let manager = sourceview5::LanguageManager::default();
    if let Some(mime) = mime.filter(|m| m.starts_with("text/") && *m != "text/plain")
        && let Some(lang) = manager.guess_language(None::<&str>, Some(mime))
    {
        return Some(lang);
    }
    let (content_type, _) = gio::content_type_guess(Some(name), head);
    manager.guess_language(Some(name), Some(content_type.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_mime() {
        let generic = Some("application/octet-stream");
        assert_eq!(display_mime(Some("image/x-foo"), Some("image/png")), "image/x-foo");
        assert_eq!(display_mime(generic, Some("image/png")), "image/png");
        assert_eq!(display_mime(None, Some("image/png")), "image/png");
        assert_eq!(display_mime(Some(""), Some("image/png")), "image/png");
        assert_eq!(display_mime(generic, None), "application/octet-stream");
        assert_eq!(display_mime(None, Some("application/x-zerosize")), "application/octet-stream");
        assert_eq!(display_mime(None, None), "application/octet-stream");
    }

    #[test]
    fn elapsed() {
        assert_eq!(format_elapsed(5), "5 s");
        assert_eq!(format_elapsed(187), "3:07");
    }

    #[test]
    fn mime() {
        let p = |n: &str, v: &str| Property {
            name: n.to_owned(),
            value: v.to_owned(),
        };
        assert_eq!(mime_type(&[p("svn:eol-style", "native")]), None);
        assert_eq!(
            mime_type(&[p("svn:mime-type", " image/png\n")]).as_deref(),
            Some("image/png")
        );
        assert_eq!(mime_type(&[p("svn:mime-type", "  ")]), None);
    }
}
