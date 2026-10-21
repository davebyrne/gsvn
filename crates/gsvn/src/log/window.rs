//! The log window: top bar, paged revision list, details pane.

use super::columns::{self, ContextHandler};
use super::logic::{self, Pager, RevRange};
use super::objects::{PathItem, RevisionItem};
use crate::app::Ctx;
use crate::common;
use crate::viewer::ViewerMode;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;
use svn_backend::url::decode;
use svn_backend::{ChangedPath, LogEntry, Priority, RepoUrl, Rev, RevNum, SvnError};

/// Rows from the end of the list at which the next page is requested.
const PREFETCH_ROWS: f64 = 50.0;
/// Delay before fetching changed paths after the selection changes, so
/// holding an arrow key doesn't start an svn process per row.
const PATHS_DELAY: Duration = Duration::from_millis(120);
/// Debounce for the revision range entries.
const RANGE_DELAY: Duration = Duration::from_millis(500);

pub struct LogWindow {
    pub(super) ctx: Rc<Ctx>,
    pub(super) url: RepoUrl,
    pub(super) peg: RevNum,
    pub(super) window: adw::ApplicationWindow,
    window_title: adw::WindowTitle,
    toasts: adw::ToastOverlay,
    pub(super) content_stack: gtk::Stack,
    actions: gio::SimpleActionGroup,

    pub(super) search: gtk::SearchEntry,
    pub(super) range_newest: gtk::Entry,
    pub(super) range_oldest: gtk::Entry,
    pub(super) stop_on_copy: gtk::CheckButton,

    pub(super) list_stack: gtk::Stack,
    empty_page: adw::StatusPage,
    page_banner: adw::Banner,
    pub(super) store: gio::ListStore,
    pub(super) selection: gtk::MultiSelection,
    pub(super) revisions: gtk::ColumnView,
    pub(super) scroller: gtk::ScrolledWindow,
    spinner: adw::Spinner,
    status: gtk::Label,
    pub(super) rev_menu: gtk::PopoverMenu,

    details_stack: gtk::Stack,
    multi_page: adw::StatusPage,
    meta_rev: gtk::Label,
    meta_info: gtk::Label,
    message: gtk::TextView,
    pub(super) paths_stack: gtk::Stack,
    paths_count: gtk::Label,
    pub(super) paths_filter: gtk::SearchEntry,
    paths_filter_needle: Rc<RefCell<String>>,
    paths_custom_filter: gtk::CustomFilter,
    pub(super) paths_store: gio::ListStore,
    paths_filtered: gtk::FilterListModel,
    pub(super) paths_selection: gtk::SingleSelection,
    pub(super) paths_view: gtk::ColumnView,
    path_menu_button: gtk::MenuButton,
    pub(super) path_popover: gtk::PopoverMenu,

    apply_pending: Cell<bool>,
    pub(super) state: RefCell<State>,
}

/// Mutable state of a log window.
pub(super) struct State {
    /// Repository root, from `svn info` (needed for changed-path URLs).
    pub root: Option<RepoUrl>,
    pub root_error: Option<SvnError>,
    pub info_task: Option<glib::JoinHandle<()>>,

    /// The applied filter.
    pub terms: Vec<String>,
    pub range: RevRange,
    pub stop_on_copy: bool,
    pub range_debounce: Option<glib::SourceId>,

    /// Bumped on every restart; results of older generations are dropped.
    pub generation: u64,
    pub pager: Pager,
    pub page_task: Option<glib::JoinHandle<()>>,
    pub pages_loaded: u32,
    pub page_error: Option<SvnError>,
    /// Search + stop-on-copy: the branch's first revision, once known.
    pub copy_floor: Option<RevNum>,

    /// Revision whose changed paths are shown or being fetched.
    pub paths_rev: Option<RevNum>,
    pub paths_task: Option<glib::JoinHandle<()>>,
    pub paths_delay: Option<glib::SourceId>,
}

impl LogWindow {
    pub fn new(ctx: &Rc<Ctx>, url: RepoUrl, peg: RevNum) -> Rc<Self> {
        let initial_path = decode(url.as_str());
        let window = common::new_window(ctx, "log", &format!("Log — {initial_path}@{peg}"), (1000, 760));
        let window_title = adw::WindowTitle::new("Log", &format!("{initial_path}@{peg}"));

        // Top bar.
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search author, date, message")
            .search_delay(300)
            .hexpand(true)
            .tooltip_text("All words must match; use \"quotes\" for phrases")
            .build();
        let range_newest = range_entry("Newest", "Newest revision of the range (default: the start revision)");
        let range_oldest = range_entry("Oldest", "Oldest revision of the range (default: r0)");
        let range_box = gtk::Box::builder().css_classes(["linked"]).build();
        range_box.append(&range_newest);
        range_box.append(&range_oldest);
        let stop_on_copy = gtk::CheckButton::builder()
            .label("Stop on _Copy")
            .use_underline(true)
            .tooltip_text("Don't follow history across copies (branch or tag creation)")
            .build();
        let filter_bar = gtk::Box::builder()
            .spacing(6)
            .margin_start(6)
            .margin_end(6)
            .margin_bottom(6)
            .build();
        filter_bar.append(&search);
        filter_bar.append(&gtk::Label::builder().label("Range").css_classes(["dim-label"]).margin_start(6).build());
        filter_bar.append(&range_box);
        filter_bar.append(&stop_on_copy);

        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh (F5)")
            .action_name("log.refresh")
            .build();
        let header = adw::HeaderBar::builder().title_widget(&window_title).build();
        header.pack_start(&refresh);
        header.pack_end(&common::main_menu_button(&common::main_menu()));

        // Revision list.
        let store = gio::ListStore::new::<RevisionItem>();
        let selection = gtk::MultiSelection::new(Some(store.clone()));
        let revisions = gtk::ColumnView::builder()
            .model(&selection)
            .show_column_separators(false)
            .show_row_separators(false)
            .reorderable(false)
            .css_classes(["data-table"])
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&revisions)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let page_banner = adw::Banner::builder().button_label("_Retry").build();
        let list_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list_box.append(&page_banner);
        list_box.append(&scroller);

        let empty_page = adw::StatusPage::builder()
            .icon_name("edit-find-symbolic")
            .title("No Matching Revisions")
            .css_classes(["compact"])
            .build();
        let loading = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        loading.append(&adw::Spinner::builder().width_request(32).height_request(32).build());
        let list_stack = gtk::Stack::new();
        list_stack.add_named(&loading, Some("loading"));
        list_stack.add_named(&list_box, Some("list"));
        list_stack.add_named(&empty_page, Some("empty"));
        list_stack.set_vexpand(true);

        let spinner = adw::Spinner::builder().visible(false).build();
        let status = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let status_box = gtk::Box::builder().spacing(6).build();
        status_box.append(&spinner);
        status_box.append(&status);
        let action_bar = gtk::ActionBar::new();
        action_bar.pack_start(&status_box);
        let diff_button = gtk::Button::builder()
            .label("_Diff")
            .use_underline(true)
            .tooltip_text("Diff with Previous Revision (Enter)")
            .action_name("log.diff-previous")
            .build();
        let compare_button = gtk::Button::builder()
            .label("Co_mpare")
            .use_underline(true)
            .tooltip_text("Compare the Two Selected Revisions")
            .action_name("log.compare")
            .build();
        let browse_button = gtk::Button::builder()
            .label("_Browse")
            .use_underline(true)
            .tooltip_text("Browse the Repository at This Revision")
            .action_name("log.browse")
            .build();
        action_bar.pack_end(&browse_button);
        action_bar.pack_end(&compare_button);
        action_bar.pack_end(&diff_button);

        let list_pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
        list_pane.append(&list_stack);
        list_pane.append(&action_bar);

        // Details pane.
        let none_page = adw::StatusPage::builder()
            .title("No Revision Selected")
            .description("Select a revision to see its message and changed paths")
            .css_classes(["compact", "dim-label"])
            .build();
        let multi_page = adw::StatusPage::builder()
            .css_classes(["compact"])
            .build();
        let meta_rev = gtk::Label::builder().xalign(0.0).css_classes(["title-4"]).selectable(true).build();
        let meta_info = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .selectable(true)
            .build();
        let meta_box = gtk::Box::builder().spacing(12).build();
        meta_box.append(&meta_rev);
        meta_box.append(&meta_info);
        install_css();
        let message = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .wrap_mode(gtk::WrapMode::WordChar)
            .css_classes(["log-message"])
            .build();
        let message_scroller = gtk::ScrolledWindow::builder()
            .child(&message)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(160)
            .build();

        let paths_count = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["dim-label"]).build();
        let paths_filter = gtk::SearchEntry::builder()
            .placeholder_text("Filter Paths")
            .search_delay(150)
            .width_chars(24)
            .build();
        let path_menu_button = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Path Actions")
            .css_classes(["flat"])
            .build();
        let paths_header = gtk::Box::builder().spacing(6).build();
        paths_header.append(&gtk::Label::builder().label("Changed Paths").css_classes(["heading"]).build());
        paths_header.append(&paths_count);
        paths_header.append(&paths_filter);
        paths_header.append(&path_menu_button);

        let paths_store = gio::ListStore::new::<PathItem>();
        let paths_filter_needle = Rc::new(RefCell::new(String::new()));
        let paths_custom_filter = {
            let needle = paths_filter_needle.clone();
            gtk::CustomFilter::new(move |obj| {
                let needle = needle.borrow();
                needle.is_empty()
                    || obj
                        .downcast_ref::<PathItem>()
                        .is_some_and(|p| p.path().path.to_lowercase().contains(needle.as_str()))
            })
        };
        let paths_filtered = gtk::FilterListModel::new(Some(paths_store.clone()), Some(paths_custom_filter.clone()));
        let paths_selection = gtk::SingleSelection::builder()
            .model(&paths_filtered)
            .autoselect(false)
            .can_unselect(true)
            .build();
        let paths_view = gtk::ColumnView::builder()
            .model(&paths_selection)
            .reorderable(false)
            .css_classes(["data-table"])
            .build();
        let paths_scroller = gtk::ScrolledWindow::builder()
            .child(&paths_view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .css_classes(["card"])
            .build();
        let paths_loading = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        paths_loading.append(&adw::Spinner::builder().width_request(24).height_request(24).build());
        let paths_stack = gtk::Stack::builder().vexpand(true).build();
        paths_stack.add_named(&paths_loading, Some("loading"));
        paths_stack.add_named(&paths_scroller, Some("list"));

        let single = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        single.append(&meta_box);
        single.append(&message_scroller);
        single.append(&gtk::Separator::builder().margin_top(6).margin_bottom(6).build());
        single.append(&paths_header);
        single.append(&paths_stack);

        let details_stack = gtk::Stack::new();
        details_stack.add_named(&none_page, Some("none"));
        details_stack.add_named(&multi_page, Some("multi"));
        details_stack.add_named(&single, Some("single"));

        let paned = gtk::Paned::builder()
            .orientation(gtk::Orientation::Vertical)
            .start_child(&list_pane)
            .end_child(&details_stack)
            .resize_start_child(true)
            .resize_end_child(true)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(360)
            .wide_handle(true)
            .build();

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&paned));
        // "error" (initial load failed) is added on demand.
        let content_stack = gtk::Stack::new();
        content_stack.add_named(&toasts, Some("main"));
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.add_top_bar(&filter_bar);
        view.set_content(Some(&content_stack));
        window.set_content(Some(&view));

        let rev_menu = gtk::PopoverMenu::builder()
            .menu_model(&revision_menu())
            .has_arrow(false)
            .halign(gtk::Align::Start)
            .build();
        rev_menu.set_parent(&revisions);
        let path_popover = gtk::PopoverMenu::builder()
            .menu_model(&path_menu(None))
            .has_arrow(false)
            .halign(gtk::Align::Start)
            .build();
        // Not parented to the column view: there, after a menu model change
        // the popover was sized before the Ctrl+C accel label (found through
        // the view's shortcuts) was added, cutting off the last item.
        path_popover.set_parent(&single);
        path_menu_button.set_menu_model(Some(&path_menu(None)));

        let range = RevRange::new(None, None, peg);
        let this = Rc::new(Self {
            ctx: ctx.clone(),
            url,
            peg,
            window,
            window_title,
            toasts,
            content_stack,
            actions: gio::SimpleActionGroup::new(),
            search,
            range_newest,
            range_oldest,
            stop_on_copy,
            list_stack,
            empty_page,
            page_banner,
            store,
            selection,
            revisions,
            scroller,
            spinner,
            status,
            rev_menu,
            details_stack,
            multi_page,
            meta_rev,
            meta_info,
            message,
            paths_stack,
            paths_count,
            paths_filter,
            paths_filter_needle,
            paths_custom_filter,
            paths_store,
            paths_filtered,
            paths_selection,
            paths_view,
            path_menu_button,
            path_popover,
            apply_pending: Cell::new(false),
            state: RefCell::new(State {
                root: None,
                root_error: None,
                info_task: None,
                terms: Vec::new(),
                range,
                stop_on_copy: false,
                range_debounce: None,
                generation: 0,
                pager: Pager::new(range, false, false),
                page_task: None,
                pages_loaded: 0,
                page_error: None,
                copy_floor: None,
                paths_rev: None,
                paths_task: None,
                paths_delay: None,
            }),
        });
        this.setup_columns();
        this.setup_actions();
        this.connect_signals();
        this.update_actions();
        this.update_details();
        this
    }

    /// Present the window and start loading.
    pub fn start(self: &Rc<Self>) {
        // The window keeps the controller alive; everything else holds weak refs.
        let strong = self.clone();
        self.window.connect_destroy(move |_| strong.shutdown());
        self.window.present();
        self.revisions.grab_focus();
        self.load_info();
        self.restart();
    }

    /// Cancel all jobs (window closed).
    fn shutdown(&self) {
        let mut st = self.state.borrow_mut();
        for task in [st.info_task.take(), st.page_task.take(), st.paths_task.take()].into_iter().flatten() {
            task.abort();
        }
        for source in [st.range_debounce.take(), st.paths_delay.take()].into_iter().flatten() {
            source.remove();
        }
        drop(st);
        self.rev_menu.unparent();
        self.path_popover.unparent();
    }

    fn setup_columns(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let context: ContextHandler = Rc::new(move |pos, widget, x, y| {
            if let Some(this) = weak.upgrade() {
                this.revision_context_menu(pos, widget, x, y);
            }
        });
        columns::revision_columns(&self.revisions, &context);

        let weak = Rc::downgrade(self);
        let context: ContextHandler = Rc::new(move |pos, widget, x, y| {
            if let Some(this) = weak.upgrade() {
                this.path_context_menu(pos, widget, x, y);
            }
        });
        columns::path_columns(&self.paths_view, &context);
    }

    fn setup_actions(self: &Rc<Self>) {
        let add = |name: &str, f: fn(&Rc<Self>)| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
            self.actions.add_action(&action);
        };
        add("refresh", |this| this.refresh());
        add("focus-search", |this| {
            this.search.grab_focus();
        });
        add("diff-previous", |this| this.diff_previous());
        add("compare", |this| this.compare());
        add("browse", |this| this.browse());
        add("copy-rev", |this| this.copy_revisions());
        add("copy-message", |this| this.copy_message());
        add("path-diff", |this| this.path_diff());
        add("path-view", |this| this.path_open_viewer(ViewerMode::Content));
        add("path-blame", |this| this.path_open_viewer(ViewerMode::Blame));
        add("path-log", |this| this.path_log());
        add("path-copy", |this| this.path_copy());
        self.window.insert_action_group("log", Some(&self.actions));

        let shortcuts = gtk::ShortcutController::new();
        shortcuts.set_scope(gtk::ShortcutScope::Managed);
        for (accel, action) in [
            ("F5", "log.refresh"),
            ("<Control>r", "log.refresh"),
            ("<Control>f", "log.focus-search"),
        ] {
            shortcuts.add_shortcut(shortcut(accel, action));
        }
        self.window.add_controller(shortcuts);

        let copy = gtk::ShortcutController::new();
        copy.add_shortcut(shortcut("<Control>c", "log.copy-rev"));
        self.revisions.add_controller(copy);
        let copy = gtk::ShortcutController::new();
        copy.add_shortcut(shortcut("<Control>c", "log.path-copy"));
        self.paths_view.add_controller(copy);
    }

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.search.connect_search_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.apply_filters();
            }
        });
        let weak = Rc::downgrade(self);
        self.search.connect_stop_search(move |entry| {
            if !entry.text().is_empty() {
                entry.set_text("");
            } else if let Some(this) = weak.upgrade() {
                this.revisions.grab_focus();
            }
        });
        for entry in [&self.range_newest, &self.range_oldest] {
            let weak = Rc::downgrade(self);
            entry.connect_changed(move |_| {
                let Some(this) = weak.upgrade() else { return };
                let weak = Rc::downgrade(&this);
                let source = glib::timeout_add_local_once(RANGE_DELAY, move || {
                    if let Some(this) = weak.upgrade() {
                        this.state.borrow_mut().range_debounce = None;
                        this.apply_filters();
                    }
                });
                if let Some(old) = this.state.borrow_mut().range_debounce.replace(source) {
                    old.remove();
                }
            });
            let weak = Rc::downgrade(self);
            entry.connect_activate(move |_| {
                if let Some(this) = weak.upgrade() {
                    if let Some(old) = this.state.borrow_mut().range_debounce.take() {
                        old.remove();
                    }
                    this.apply_filters();
                }
            });
        }
        let weak = Rc::downgrade(self);
        self.stop_on_copy.connect_toggled(move |_| {
            if let Some(this) = weak.upgrade() {
                this.apply_filters();
            }
        });

        // Paging: watch the scroll position and the list growing.
        let adj = self.scroller.vadjustment();
        let weak = Rc::downgrade(self);
        adj.connect_value_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.maybe_load_more();
            }
        });
        let weak = Rc::downgrade(self);
        adj.connect_changed(move |_| {
            if let Some(this) = weak.upgrade() {
                this.maybe_load_more();
            }
        });

        let weak = Rc::downgrade(self);
        self.selection.connect_selection_changed(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.update_actions();
                this.update_details();
            }
        });
        // Items appended by paging don't change the selection, but replacing
        // the model does (restart): covered by selection-changed as well.
        let weak = Rc::downgrade(self);
        self.revisions.connect_activate(move |_, pos| {
            if let Some(this) = weak.upgrade() {
                // Enter/double-click acts on the activated row alone.
                if !this.selection.is_selected(pos) || this.selected_revisions().len() != 1 {
                    this.selection.select_item(pos, true);
                }
                this.diff_previous();
            }
        });

        let weak = Rc::downgrade(self);
        self.page_banner.connect_button_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.state.borrow_mut().page_error = None;
                this.page_banner.set_revealed(false);
                this.load_next_page();
            }
        });

        // Changed paths.
        let weak = Rc::downgrade(self);
        self.paths_filter.connect_search_changed(move |entry| {
            let Some(this) = weak.upgrade() else { return };
            let text = entry.text().to_lowercase();
            let change = {
                let old = this.paths_filter_needle.borrow();
                if text.contains(old.as_str()) {
                    gtk::FilterChange::MoreStrict
                } else if old.contains(text.as_str()) {
                    gtk::FilterChange::LessStrict
                } else {
                    gtk::FilterChange::Different
                }
            };
            *this.paths_filter_needle.borrow_mut() = text;
            this.paths_custom_filter.changed(change);
            this.update_paths_count();
        });
        let weak = Rc::downgrade(self);
        self.paths_selection.connect_selection_changed(move |_, _, _| {
            if let Some(this) = weak.upgrade() {
                this.update_path_actions();
            }
        });
        let weak = Rc::downgrade(self);
        self.paths_view.connect_activate(move |_, pos| {
            if let Some(this) = weak.upgrade() {
                this.paths_selection.set_selected(pos);
                this.path_diff();
            }
        });
    }

    // ---- loading -------------------------------------------------------

    /// Resolve the repository root (for changed-path URLs) and the title.
    fn load_info(self: &Rc<Self>) {
        let job = self.ctx.svn.info(&self.url, &Rev::Number(self.peg), Priority::Interactive);
        let weak = Rc::downgrade(self);
        let task = glib::spawn_future_local(async move {
            let result = job.result().await;
            let Some(this) = weak.upgrade() else { return };
            let mut st = this.state.borrow_mut();
            st.info_task = None;
            match result {
                Ok(info) => {
                    let title = format!("{}@{}", info.repos_path, this.peg);
                    this.window.set_title(Some(&format!("Log — {title}")));
                    this.window_title.set_subtitle(&title);
                    st.root = Some(info.root);
                    st.root_error = None;
                }
                Err(e) if e.is_cancelled() => return,
                Err(e) => st.root_error = Some(e),
            }
            drop(st);
            // Changed paths may be waiting for the root.
            this.load_paths_now();
            this.update_path_actions();
        });
        if let Some(old) = self.state.borrow_mut().info_task.replace(task) {
            old.abort();
        }
    }

    /// Re-read the top bar on the next idle. Coalesces bursts such as the
    /// delete + insert of a programmatic `set_text` (a search entry reports
    /// an empty text immediately, without its delay).
    fn apply_filters(self: &Rc<Self>) {
        if self.apply_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(this) = weak.upgrade() {
                this.apply_pending.set(false);
                this.apply_filters_now();
            }
        });
    }

    /// Read the top bar and restart paging if the filter changed.
    fn apply_filters_now(self: &Rc<Self>) {
        let terms = logic::split_search_terms(&self.search.text());
        let newest = parse_range_entry(&self.range_newest);
        let oldest = parse_range_entry(&self.range_oldest);
        let (Some(newest), Some(oldest)) = (newest, oldest) else {
            return; // invalid input is flagged on the entry
        };
        let range = RevRange::new(newest, oldest, self.peg);
        let stop_on_copy = self.stop_on_copy.is_active();
        {
            let mut st = self.state.borrow_mut();
            if st.terms == terms && st.range == range && st.stop_on_copy == stop_on_copy {
                return;
            }
            if st.stop_on_copy != stop_on_copy || st.range.newest != range.newest {
                st.copy_floor = None;
            }
            st.terms = terms;
            st.range = range;
            st.stop_on_copy = stop_on_copy;
        }
        self.restart();
    }

    fn refresh(self: &Rc<Self>) {
        self.state.borrow_mut().copy_floor = None;
        if self.state.borrow().root.is_none() {
            self.load_info();
        }
        self.restart();
    }

    /// Cancel the in-flight page and start over from the first page.
    pub(super) fn restart(self: &Rc<Self>) {
        {
            let mut st = self.state.borrow_mut();
            st.generation += 1;
            if let Some(task) = st.page_task.take() {
                task.abort();
            }
            st.pager = Pager::new(st.range, !st.terms.is_empty(), st.stop_on_copy);
            st.pages_loaded = 0;
            st.page_error = None;
        }
        self.page_banner.set_revealed(false);
        self.store.remove_all();
        self.list_stack.set_visible_child_name("loading");
        self.content_stack.set_visible_child_name("main");
        if let Some(old) = self.content_stack.child_by_name("error") {
            self.content_stack.remove(&old);
        }
        self.load_next_page();
    }

    /// Request the next page when the view is within [`PREFETCH_ROWS`] of
    /// the end of what is loaded.
    pub(super) fn maybe_load_more(self: &Rc<Self>) {
        {
            let st = self.state.borrow();
            if st.page_task.is_some() || st.page_error.is_some() || st.pager.is_done() || st.pages_loaded == 0 {
                return;
            }
        }
        let n = self.store.n_items();
        let adj = self.scroller.vadjustment();
        if n > 0 && adj.upper() > 0.0 {
            let row_height = adj.upper() / f64::from(n);
            let rows_below = (adj.upper() - adj.value() - adj.page_size()) / row_height;
            if rows_below > PREFETCH_ROWS {
                return;
            }
        }
        self.load_next_page();
    }

    pub(super) fn load_next_page(self: &Rc<Self>) {
        let mut st = self.state.borrow_mut();
        if st.page_task.is_some() || st.page_error.is_some() {
            return;
        }
        // A search can't use `-l` paging, so with stop-on-copy the branch
        // start has to be found first (see logic::Pager).
        if st.stop_on_copy && !st.terms.is_empty() && st.copy_floor.is_none() {
            let task = self.spawn_copy_floor(st.generation, st.range.newest);
            st.page_task = Some(task);
            drop(st);
            self.update_status();
            return;
        }
        let Some(req) = st.pager.next_request() else {
            drop(st);
            self.update_status();
            return;
        };
        let query = req.query(&st.terms, st.stop_on_copy);
        let generation = st.generation;
        let mut stream = self.ctx.svn.log(&self.url, self.peg, &query, Priority::Interactive);
        let weak = Rc::downgrade(self);
        let started = std::time::Instant::now();
        let task = glib::spawn_future_local(async move {
            let mut entries: Vec<LogEntry> = Vec::new();
            let mut error = None;
            while let Some(batch) = stream.next().await {
                match batch {
                    Ok(batch) => entries.extend(batch),
                    Err(e) => {
                        error = Some(e);
                        break;
                    }
                }
            }
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().generation != generation {
                return;
            }
            this.page_done(req, entries, error, started.elapsed());
        });
        st.page_task = Some(task);
        drop(st);
        self.update_status();
    }

    fn page_done(
        self: &Rc<Self>,
        req: logic::PageRequest,
        entries: Vec<LogEntry>,
        error: Option<SvnError>,
        elapsed: Duration,
    ) {
        let mut st = self.state.borrow_mut();
        st.page_task = None;
        if let Some(err) = error {
            if err.is_cancelled() {
                return;
            }
            if st.pages_loaded > 0 && logic::is_end_of_history(err.code) {
                st.pager.finish();
            } else if st.pages_loaded == 0 && self.store.n_items() == 0 {
                drop(st);
                self.show_list_error(&err);
                return;
            } else {
                drop(st);
                self.show_page_error(err);
                return;
            }
        } else {
            let revs: Vec<RevNum> = entries.iter().map(|e| e.rev).collect();
            let skip = st.pager.complete(req, &revs);
            st.pages_loaded += 1;
            let items: Vec<RevisionItem> = entries.into_iter().skip(skip).map(RevisionItem::new).collect();
            if debug_enabled() {
                eprintln!(
                    "[gsvn-log] page r{}:r{} limit {} → {} entries in {} ms",
                    req.start,
                    req.end,
                    req.limit,
                    items.len(),
                    elapsed.as_millis()
                );
            }
            drop(st);
            self.store.extend_from_slice(&items);
            st = self.state.borrow_mut();
        }
        let done = st.pager.is_done();
        let searching = !st.terms.is_empty();
        drop(st);
        if self.store.n_items() > 0 {
            self.list_stack.set_visible_child_name("list");
        } else if done {
            self.empty_page.set_title(if searching { "No Matching Revisions" } else { "No Revisions" });
            self.empty_page.set_description(Some(if searching {
                "Try fewer or different search terms, or a wider revision range"
            } else {
                "This path has no history in the selected range"
            }));
            self.list_stack.set_visible_child_name("empty");
        }
        self.update_status();
        // The list may not fill the view yet (or a search window had no
        // matches): check again once it has been laid out.
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(this) = weak.upgrade() {
                this.maybe_load_more();
            }
        });
    }

    /// A follow-up page failed: keep what is loaded, offer Retry.
    pub(super) fn show_page_error(&self, err: SvnError) {
        self.page_banner.set_title(&format!("Couldn’t load more revisions: {}", err.message));
        self.page_banner.set_revealed(true);
        self.state.borrow_mut().page_error = Some(err);
        self.update_status();
    }

    /// Search + stop-on-copy: find the branch's first revision, which then
    /// bounds the search windows.
    fn spawn_copy_floor(self: &Rc<Self>, generation: u64, newest: RevNum) -> glib::JoinHandle<()> {
        let query = svn_backend::LogQuery {
            start: Rev::Number(0),
            end: Rev::Number(newest),
            limit: 1,
            search: Vec::new(),
            stop_on_copy: true,
            verbose: false,
        };
        let mut stream = self.ctx.svn.log(&self.url, self.peg, &query, Priority::Interactive);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mut floor = None;
            let mut error = None;
            while let Some(batch) = stream.next().await {
                match batch {
                    Ok(batch) => floor = floor.or(batch.first().map(|e| e.rev)),
                    Err(e) => error = Some(e),
                }
            }
            let Some(this) = weak.upgrade() else { return };
            let mut st = this.state.borrow_mut();
            if st.generation != generation {
                return;
            }
            st.page_task = None;
            match error {
                Some(e) if e.is_cancelled() => {}
                Some(e) => {
                    drop(st);
                    this.show_list_error(&e);
                }
                None => {
                    let floor = floor.unwrap_or(0);
                    st.copy_floor = Some(floor);
                    st.pager.set_oldest(floor);
                    drop(st);
                    this.load_next_page();
                }
            }
        })
    }

    fn show_list_error(self: &Rc<Self>, err: &SvnError) {
        let weak = Rc::downgrade(self);
        let page = common::error_page(
            err,
            Some(Box::new(move || {
                if let Some(this) = weak.upgrade() {
                    this.refresh();
                }
            })),
        );
        if let Some(old) = self.content_stack.child_by_name("error") {
            self.content_stack.remove(&old);
        }
        self.content_stack.add_named(&page, Some("error"));
        self.content_stack.set_visible_child_name("error");
        self.update_status();
    }

    pub(super) fn update_status(&self) {
        let st = self.state.borrow();
        let n = self.store.n_items();
        let loading = st.page_task.is_some();
        let searching = !st.terms.is_empty();
        let count = if searching {
            match n {
                1 => "1 matching revision".to_owned(),
                n => format!("{} matching revisions", group_digits(u64::from(n))),
            }
        } else {
            match n {
                1 => "1 revision loaded".to_owned(),
                n => format!("{} revisions loaded", group_digits(u64::from(n))),
            }
        };
        let text = if self.content_stack.visible_child_name().as_deref() == Some("error") {
            "Failed to load the log".to_owned()
        } else if st.page_error.is_some() {
            format!("{count} · loading failed")
        } else if st.pager.is_done() {
            if searching {
                format!("{count} · searched to the start of history")
            } else {
                format!("{count} · end of history")
            }
        } else if loading && searching && st.copy_floor.is_none() && st.stop_on_copy {
            format!("{count} · finding where the branch starts…")
        } else if searching {
            match st.pager.searched_to() {
                Some(r) if st.pages_loaded > 0 => format!("{count} · searched back to r{r}"),
                _ => format!("{count} · searching…"),
            }
        } else if loading {
            format!("{count} · loading…")
        } else {
            count
        };
        self.status.set_text(&text);
        self.spinner.set_visible(loading);
    }

    // ---- selection and details ----------------------------------------

    /// Selected revisions in list order (newest first).
    pub(super) fn selected_revisions(&self) -> Vec<LogEntry> {
        let set = self.selection.selection();
        let n = set.size().min(3) as u32;
        (0..n)
            .filter_map(|i| self.store.item(set.nth(i)).and_downcast::<RevisionItem>())
            .map(|item| item.entry().clone())
            .collect()
    }

    fn selected_count(&self) -> u64 {
        self.selection.selection().size()
    }

    fn update_actions(&self) {
        let selected = self.selected_revisions();
        let count = self.selected_count();
        let revs: Vec<RevNum> = selected.iter().map(|e| e.rev).collect();
        let one = count == 1;
        self.set_enabled("diff-previous", one && logic::revision_diff(&self.url, self.peg, &revs).is_some());
        self.set_enabled("compare", count == 2 && logic::revision_diff(&self.url, self.peg, &revs).is_some());
        self.set_enabled("browse", one);
        self.set_enabled("copy-rev", count > 0);
        self.set_enabled("copy-message", one);
    }

    fn set_enabled(&self, name: &str, enabled: bool) {
        if let Some(action) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
            action.set_enabled(enabled);
        }
    }

    fn update_details(self: &Rc<Self>) {
        let count = self.selected_count();
        if count != 1 {
            self.cancel_paths();
            if count == 0 {
                self.details_stack.set_visible_child_name("none");
            } else {
                self.multi_page.set_title(&format!("{} Revisions Selected", group_digits(count)));
                self.multi_page.set_description(Some(if count == 2 {
                    "Compare shows the changes between the two revisions"
                } else {
                    "Select one revision for details, or two to compare them"
                }));
                self.details_stack.set_visible_child_name("multi");
            }
            return;
        }
        let Some(entry) = self.selected_revisions().into_iter().next() else { return };
        self.details_stack.set_visible_child_name("single");
        self.meta_rev.set_text(&format!("r{}", entry.rev));
        let author = entry.author.as_deref().unwrap_or("(no author)");
        let date = entry.date.map(common::format_datetime).unwrap_or_default();
        self.meta_info.set_text(&if date.is_empty() { author.to_owned() } else { format!("{author} · {date}") });
        let message = entry.message.trim_end();
        if message.is_empty() {
            self.message.buffer().set_text("(no message)");
            self.message.add_css_class("dim-label");
        } else {
            self.message.buffer().set_text(message);
            self.message.remove_css_class("dim-label");
        }

        if self.state.borrow().paths_rev == Some(entry.rev) {
            return;
        }
        self.cancel_paths();
        self.state.borrow_mut().paths_rev = Some(entry.rev);
        self.paths_store.remove_all();
        self.paths_count.set_text("");
        self.paths_stack.set_visible_child_name("loading");
        self.update_path_actions();
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(PATHS_DELAY, move || {
            if let Some(this) = weak.upgrade() {
                this.state.borrow_mut().paths_delay = None;
                this.load_paths_now();
            }
        });
        self.state.borrow_mut().paths_delay = Some(source);
    }

    fn cancel_paths(&self) {
        let mut st = self.state.borrow_mut();
        if let Some(task) = st.paths_task.take() {
            task.abort();
        }
        if let Some(source) = st.paths_delay.take() {
            source.remove();
        }
        st.paths_rev = None;
    }

    /// Fetch the changed paths of `paths_rev` (if not already loaded or
    /// loading and the root is known).
    fn load_paths_now(self: &Rc<Self>) {
        let mut st = self.state.borrow_mut();
        let Some(rev) = st.paths_rev else { return };
        if st.paths_task.is_some() || st.paths_delay.is_some() || self.paths_stack.visible_child_name().as_deref() != Some("loading") {
            return;
        }
        let Some(root) = st.root.clone() else {
            if let Some(err) = st.root_error.clone() {
                drop(st);
                self.show_paths_error(&err);
            }
            return; // waiting for info
        };
        let job = self.ctx.svn.log_entry(&root, rev, Priority::Interactive);
        let weak = Rc::downgrade(self);
        let started = std::time::Instant::now();
        st.paths_task = Some(glib::spawn_future_local(async move {
            let result = job.result().await;
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().paths_rev != Some(rev) {
                return;
            }
            this.state.borrow_mut().paths_task = None;
            match result {
                Ok(entry) => this.show_paths(entry.changed_paths, started),
                Err(e) if e.is_cancelled() => {}
                Err(e) => this.show_paths_error(&e),
            }
        }));
    }

    fn show_paths(&self, paths: Vec<ChangedPath>, started: std::time::Instant) {
        let fetched = started.elapsed();
        let items: Vec<PathItem> = paths.into_iter().map(PathItem::new).collect();
        self.paths_store.splice(0, self.paths_store.n_items(), &items);
        self.paths_stack.set_visible_child_name("list");
        self.update_paths_count();
        self.update_path_actions();
        if debug_enabled() {
            eprintln!(
                "[gsvn-log] changed paths: {} in {} ms (fetch+parse {} ms)",
                items.len(),
                started.elapsed().as_millis(),
                fetched.as_millis()
            );
        }
    }

    fn show_paths_error(self: &Rc<Self>, err: &SvnError) {
        let weak = Rc::downgrade(self);
        let page = common::error_page(
            err,
            Some(Box::new(move || {
                let Some(this) = weak.upgrade() else { return };
                if this.state.borrow().root.is_none() {
                    this.state.borrow_mut().root_error = None;
                    this.load_info();
                }
                this.paths_stack.set_visible_child_name("loading");
                this.load_paths_now();
            })),
        );
        page.add_css_class("compact");
        if let Some(old) = self.paths_stack.child_by_name("error") {
            self.paths_stack.remove(&old);
        }
        self.paths_stack.add_named(&page, Some("error"));
        self.paths_stack.set_visible_child_name("error");
        self.paths_count.set_text("");
    }

    fn update_paths_count(&self) {
        let total = self.paths_store.n_items();
        let shown = self.paths_filtered.n_items();
        let noun = if total == 1 { "path" } else { "paths" };
        let text = if self.paths_filter_needle.borrow().is_empty() {
            format!("{} {noun}", group_digits(u64::from(total)))
        } else {
            format!("{} of {} {noun}", group_digits(u64::from(shown)), group_digits(u64::from(total)))
        };
        self.paths_count.set_text(&text);
    }

    /// The selected changed path and its action targets.
    fn selected_path(&self) -> Option<(ChangedPath, logic::PathTarget)> {
        let item = self.paths_selection.selected_item().and_downcast::<PathItem>()?;
        let st = self.state.borrow();
        let (root, rev) = (st.root.as_ref()?, st.paths_rev?);
        let cp = item.path().clone();
        let target = logic::path_target(root, rev, &cp);
        Some((cp, target))
    }

    fn update_path_actions(&self) {
        let selected = self.selected_path();
        let target = selected.as_ref().map(|(_, t)| t);
        let is_dir = selected.as_ref().is_some_and(|(cp, _)| cp.kind == Some(svn_backend::NodeKind::Dir));
        self.set_enabled("path-diff", target.is_some_and(|t| t.diff.is_some()));
        self.set_enabled("path-view", target.is_some_and(|t| t.viewable));
        self.set_enabled("path-blame", target.is_some_and(|t| t.viewable));
        self.set_enabled("path-log", target.is_some());
        self.set_enabled("path-copy", target.is_some());
        let menu = path_menu(target.map(|t| (t.rev, is_dir)));
        self.path_popover.set_menu_model(Some(&menu));
        self.path_menu_button.set_menu_model(Some(&menu));
        self.path_menu_button.set_sensitive(target.is_some());
    }

    // ---- context menus -------------------------------------------------

    pub(super) fn revision_context_menu(&self, pos: u32, widget: &gtk::Widget, x: f64, y: f64) {
        if !self.selection.is_selected(pos) {
            self.selection.select_item(pos, true);
        }
        popup_at(&self.rev_menu, widget, x, y);
    }

    pub(super) fn path_context_menu(&self, pos: u32, widget: &gtk::Widget, x: f64, y: f64) {
        self.paths_selection.set_selected(pos);
        popup_at(&self.path_popover, widget, x, y);
    }

    // ---- actions -------------------------------------------------------

    fn diff_previous(&self) {
        let revs: Vec<RevNum> = self.selected_revisions().iter().map(|e| e.rev).collect();
        if revs.len() == 1
            && let Some(spec) = logic::revision_diff(&self.url, self.peg, &revs)
        {
            crate::diff::open(&self.ctx, spec);
        }
    }

    fn compare(&self) {
        let revs: Vec<RevNum> = self.selected_revisions().iter().map(|e| e.rev).collect();
        if revs.len() == 2
            && let Some(spec) = logic::revision_diff(&self.url, self.peg, &revs)
        {
            crate::diff::open(&self.ctx, spec);
        }
    }

    /// Browse the path as it was at the selected revision: the log follows
    /// renames, so `url` (named at `peg`) is first resolved to its URL then.
    fn browse(&self) {
        let rev = match self.selected_revisions().as_slice() {
            [entry] => entry.rev,
            _ => return,
        };
        let job = self.ctx.svn.info_at(&self.url, self.peg, rev, Priority::Interactive);
        let (ctx, toasts) = (self.ctx.clone(), self.toasts.clone());
        common::spawn(async move {
            match job.result().await {
                Ok(info) => crate::browser::open(&ctx, info.url, Some(rev)),
                Err(e) => toasts.add_toast(
                    adw::Toast::builder()
                        .title(glib::markup_escape_text(&format!("Cannot browse r{rev}: {}", e.message)))
                        .timeout(5)
                        .build(),
                ),
            }
        });
    }

    fn copy_revisions(&self) {
        let set = self.selection.selection();
        let n = set.size().min(10_000) as u32;
        let revs: Vec<String> = (0..n)
            .filter_map(|i| self.store.item(set.nth(i)).and_downcast::<RevisionItem>())
            .map(|item| item.entry().rev.to_string())
            .collect();
        if !revs.is_empty() {
            self.copy_text(&revs.join(", "), if revs.len() == 1 { "Revision number copied" } else { "Revision numbers copied" });
        }
    }

    fn copy_message(&self) {
        if let [entry] = self.selected_revisions().as_slice() {
            self.copy_text(entry.message.trim_end(), "Message copied");
        }
    }

    fn path_diff(&self) {
        if let Some((_, target)) = self.selected_path()
            && let Some(spec) = target.diff
        {
            crate::diff::open(&self.ctx, spec);
        }
    }

    fn path_open_viewer(&self, mode: ViewerMode) {
        if let Some((_, target)) = self.selected_path()
            && target.viewable
        {
            crate::viewer::open(&self.ctx, target.url, target.rev, None, mode);
        }
    }

    fn path_log(&self) {
        if let Some((_, target)) = self.selected_path() {
            super::open(&self.ctx, target.url, target.rev);
        }
    }

    fn path_copy(&self) {
        if let Some((cp, _)) = self.selected_path() {
            self.copy_text(&cp.path, "Path copied");
        }
    }

    fn copy_text(&self, text: &str, toast: &str) {
        self.window.clipboard().set_text(text);
        self.toasts.add_toast(adw::Toast::builder().title(toast).timeout(2).build());
    }
}

/// Window-independent styles (installed once per display).
fn install_css() {
    thread_local! {
        static DONE: Cell<bool> = const { Cell::new(false) };
    }
    if DONE.with(|d| d.replace(true)) {
        return;
    }
    let Some(display) = gdk::Display::default() else { return };
    let provider = gtk::CssProvider::new();
    provider.load_from_string("textview.log-message, textview.log-message > text { background: transparent; }");
    gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
}

fn shortcut(accel: &str, action: &str) -> gtk::Shortcut {
    gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(accel), Some(gtk::NamedAction::new(action)))
}

fn range_entry(placeholder: &str, tooltip: &str) -> gtk::Entry {
    gtk::Entry::builder()
        .placeholder_text(placeholder)
        .tooltip_text(tooltip)
        .width_chars(8)
        .max_width_chars(10)
        .input_purpose(gtk::InputPurpose::Digits)
        .build()
}

/// Parse a range entry: `Some(None)` for empty, `Some(Some(n))` for a
/// revision, `None` (and the entry flagged) for invalid input.
fn parse_range_entry(entry: &gtk::Entry) -> Option<Option<RevNum>> {
    match common::parse_rev_input(&entry.text()) {
        Ok(rev) => {
            entry.remove_css_class("error");
            Some(rev)
        }
        Err(_) => {
            entry.add_css_class("error");
            None
        }
    }
}

fn revision_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let section = gio::Menu::new();
    section.append(Some("_Diff with Previous"), Some("log.diff-previous"));
    section.append(Some("Co_mpare Revisions"), Some("log.compare"));
    section.append(Some("_Browse at This Revision"), Some("log.browse"));
    menu.append_section(None, &section);
    let section = gio::Menu::new();
    section.append(Some("_Copy Revision Number"), Some("log.copy-rev"));
    section.append(Some("Copy _Message"), Some("log.copy-message"));
    menu.append_section(None, &section);
    menu
}

/// Menu for a changed path; `target` is `(revision the node exists at, is_dir)`.
fn path_menu(target: Option<(RevNum, bool)>) -> gio::Menu {
    let at = target.map(|(rev, _)| format!(" at r{rev}")).unwrap_or_default();
    let menu = gio::Menu::new();
    let section = gio::Menu::new();
    section.append(Some("_Diff"), Some("log.path-diff"));
    if !target.is_some_and(|(_, dir)| dir) {
        section.append(Some(&format!("_View{at}")), Some("log.path-view"));
        section.append(Some(&format!("_Blame{at}")), Some("log.path-blame"));
    }
    section.append(Some("Show _Log of This Path"), Some("log.path-log"));
    menu.append_section(None, &section);
    let section = gio::Menu::new();
    let copy = gio::MenuItem::new(Some("_Copy Path"), Some("log.path-copy"));
    // Shown as a hint: the shortcut itself is on the changed-paths list.
    copy.set_attribute_value("accel", Some(&"<Control>c".to_variant()));
    section.append_item(&copy);
    menu.append_section(None, &section);
    menu
}

fn popup_at(popover: &gtk::PopoverMenu, widget: &gtk::Widget, x: f64, y: f64) {
    let Some(parent) = popover.parent() else { return };
    let point = widget
        .compute_point(&parent, &gtk::graphene::Point::new(x as f32, y as f32))
        .unwrap_or_else(|| gtk::graphene::Point::new(x as f32, y as f32));
    popover.set_pointing_to(Some(&gdk::Rectangle::new(point.x() as i32, point.y() as i32, 1, 1)));
    popover.popup();
}

/// `12345` → `12,345`.
pub(super) fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Timing output on stderr (debug builds with `GSVN_LOG_DEBUG` set).
fn debug_enabled() -> bool {
    thread_local! {
        static ENABLED: Cell<Option<bool>> = const { Cell::new(None) };
    }
    ENABLED.with(|e| {
        *e.get().get_or_insert_with(|| cfg!(debug_assertions) && std::env::var_os("GSVN_LOG_DEBUG").is_some())
    })
}

#[cfg(test)]
mod tests {
    use super::group_digits;

    #[test]
    fn digits() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(50_001), "50,001");
        assert_eq!(group_digits(1_234_567), "1,234,567");
    }
}
