//! Repository browser window (DESIGN.md §5).
//!
//! Layout: header bar (back/forward, path bar, revision button, refresh,
//! menu), an optional "does not exist" banner, then a sidebar folder tree
//! ([`tree`]) and the file list ([`filelist`]) side by side, and a status
//! line at the bottom. Both views share one listing per directory through
//! the per-window [`dir::DirCache`].
//!
//! `G_MESSAGES_DEBUG=gsvn-browser` logs listing timings (first rows,
//! complete, RSS).

mod dir;
mod entry;
mod filelist;
mod nav;
mod pathbar;
mod store;
mod tree;

#[cfg(debug_assertions)]
mod debug;

use crate::app::Ctx;
use crate::common;
use crate::settings::Bookmark;
use crate::viewer::ViewerMode;
use adw::prelude::*;
use dir::{DirCache, DirModel, LoadState};
use entry::EntryObject;
use filelist::FileList;
use gtk::{gdk, gio, glib};
use nav::{History, Location};
use pathbar::PathBar;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use svn_backend::url::decode;
use svn_backend::{CancelToken, ErrorKind, Info, NodeKind, Priority, RepoUrl, Rev, RevNum, SvnError};
use tree::Tree;

const LOG_DOMAIN: &str = "gsvn-browser";

/// Open a browser window at `url` (any path inside a repository; if it is
/// a file, show its directory with the file selected). `rev = None` means
/// resolve HEAD.
pub fn open(ctx: &Rc<Ctx>, url: RepoUrl, rev: Option<RevNum>) {
    let browser = Browser::new(ctx);
    if let Some(window) = browser.window.upgrade() {
        window.present();
    }
    browser.go_to(url, Some(rev.map_or(Rev::Head, Rev::Number)));
}

struct State {
    cache: Option<Rc<DirCache>>,
    loc: Option<Location>,
    history: History<Location>,
    /// Entry to select in the file list once the current listing is done.
    pending_select: Option<String>,
    /// Background tree expansion: generation and the listing it waits on.
    reveal_gen: u64,
    reveal_loading: Option<(Rc<DirModel>, gtk::TreeListRow)>,
    /// Pending `svn info` for navigation (cancelled when superseded).
    info_gen: u64,
    info_token: Option<CancelToken>,
    /// Entry the context menu was opened for.
    context: Option<EntryObject>,
    /// The tree selection is being changed programmatically.
    syncing_tree: bool,
    /// The current navigation came from selecting a tree row: the row is
    /// selected but not expanded (only the expander arrow expands).
    from_tree: bool,
    /// The user scrolled, clicked or typed in the file list since the
    /// last navigation. Until then the list is kept scrolled to the top
    /// while batches arrive (directories sort first but arrive last).
    list_touched: bool,
}

struct Browser {
    ctx: Rc<Ctx>,
    window: glib::WeakRef<adw::ApplicationWindow>,
    content: gtk::Stack,
    toasts: adw::ToastOverlay,
    banner: adw::Banner,
    pathbar: Rc<PathBar>,
    rev_button: gtk::MenuButton,
    rev_entry: gtk::Entry,
    rev_error: gtk::Label,
    tree: Tree,
    files: FileList,
    filter_entry: gtk::SearchEntry,
    list_stack: gtk::Stack,
    list_error: adw::Bin,
    status_label: gtk::Label,
    status_spinner: adw::Spinner,
    cancel_button: gtk::Button,
    actions: gio::SimpleActionGroup,
    row_actions: gio::SimpleActionGroup,
    bookmarks_menu: gio::Menu,
    file_menu: gtk::PopoverMenu,
    tree_menu: gtk::PopoverMenu,
    state: RefCell<State>,
    /// Debug timing of the open → first browse.
    opened: Cell<Option<std::time::Instant>>,
    /// A deferred [`Browser::keep_top`] is pending.
    keep_top_queued: Cell<bool>,
}

impl Browser {
    fn new(ctx: &Rc<Ctx>) -> Rc<Self> {
        install_css();
        let window = common::new_window(ctx, "browser", "Repository Browser", (1100, 720));

        // Header bar.
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back (Alt+←)")
            .action_name("browser.back")
            .build();
        let forward = gtk::Button::builder()
            .icon_name("go-next-symbolic")
            .tooltip_text("Forward (Alt+→)")
            .action_name("browser.forward")
            .build();
        let nav_box = gtk::Box::builder().css_classes(["linked"]).build();
        nav_box.append(&back);
        nav_box.append(&forward);

        let (rev_popover, rev_entry, rev_error, rev_go) = revision_popover();
        let rev_button = gtk::MenuButton::builder()
            .label("r…")
            .tooltip_text("Revision")
            .popover(&rev_popover)
            .css_classes(["numeric"])
            .build();
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh (F5)")
            .action_name("browser.refresh")
            .build();

        let bookmarks_menu = gio::Menu::new();
        let menu = common::main_menu();
        let section = gio::Menu::new();
        section.append(Some("Show _Log"), Some("browser.show-log"));
        section.append(Some("_Bookmark This Location"), Some("browser.bookmark"));
        section.append_submenu(Some("Book_marks"), &bookmarks_menu);
        menu.prepend_section(None, &section);
        let menu_button = common::main_menu_button(&menu);

        let header = adw::HeaderBar::new();
        header.pack_start(&nav_box);
        header.pack_end(&menu_button);
        header.pack_end(&refresh);
        header.pack_end(&rev_button);

        let banner = adw::Banner::builder().button_label("Show Log of Parent").build();

        // Sidebar and file list.
        let this_cell: Rc<RefCell<std::rc::Weak<Browser>>> = Rc::default();
        let tree = {
            let this_cell = this_cell.clone();
            Tree::new(Rc::new(move |entry, widget, x, y| {
                if let Some(this) = this_cell.borrow().upgrade() {
                    this.popup_menu(entry, true, widget, x, y);
                }
            }))
        };
        let files = {
            let this_cell = this_cell.clone();
            FileList::new(Rc::new(move |pos, widget, x, y| {
                let Some(this) = this_cell.borrow().upgrade() else { return };
                this.files.selection.set_selected(pos);
                if let Some(entry) = this.files.item(pos) {
                    this.popup_menu(&entry, false, widget, x, y);
                }
            }))
        };
        tree.scrolled.set_size_request(180, -1);

        let filter_entry = gtk::SearchEntry::builder()
            .placeholder_text("Filter")
            .hexpand(true)
            .build();
        filter_entry.set_key_capture_widget(Some(&files.view));
        let filter_bar = gtk::Box::builder()
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        filter_bar.append(&filter_entry);

        let list_error = adw::Bin::new();
        let list_stack = gtk::Stack::new();
        list_stack.add_named(&files.scrolled, Some("list"));
        list_stack.add_named(&list_error, Some("error"));

        let main = gtk::Box::new(gtk::Orientation::Vertical, 0);
        main.append(&filter_bar);
        main.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        main.append(&list_stack);

        let paned = gtk::Paned::builder()
            .start_child(&tree.scrolled)
            .end_child(&main)
            .resize_start_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(280)
            .build();
        common::persist_pane(ctx, &window, &paned, "browser.sidebar");

        // Status line.
        let status_spinner = adw::Spinner::builder().visible(false).build();
        let status_label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["dim-label", "caption"])
            .build();
        let cancel_button = gtk::Button::builder()
            .label("Cancel")
            .action_name("browser.cancel")
            .css_classes(["flat", "small"])
            .visible(false)
            .build();
        let status = gtk::Box::builder()
            .spacing(8)
            .css_classes(["gsvn-status"])
            .build();
        status.append(&status_spinner);
        status.append(&status_label);
        status.append(&cancel_button);

        let browse = adw::ToolbarView::new();
        browse.add_top_bar(&banner);
        browse.set_content(Some(&paned));
        browse.add_bottom_bar(&status);
        browse.set_bottom_bar_style(adw::ToolbarStyle::Raised);

        let content = gtk::Stack::new();
        let loading = adw::StatusPage::builder()
            .title("Opening Repository…")
            .child(&adw::Spinner::builder().width_request(32).height_request(32).build())
            .build();
        content.add_named(&loading, Some("loading"));
        content.add_named(&browse, Some("browse"));
        content.add_named(&adw::Bin::new(), Some("error"));
        content.set_visible_child_name("loading");

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&content));
        let pathbar = {
            let (c1, c2) = (this_cell.clone(), this_cell.clone());
            PathBar::new(
                move |url| {
                    if let Some(this) = c1.borrow().upgrade() {
                        this.go_to(url, None);
                    }
                },
                move |text| {
                    if let Some(this) = c2.borrow().upgrade() {
                        this.go_to_input(&text);
                    }
                },
            )
        };
        header.set_title_widget(Some(&pathbar.widget));

        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.set_content(Some(&toasts));
        window.set_content(Some(&view));

        let file_menu = gtk::PopoverMenu::from_model(Some(&file_menu_model()));
        file_menu.set_has_arrow(false);
        file_menu.set_halign(gtk::Align::Start);
        file_menu.set_parent(&files.view);
        let tree_menu = gtk::PopoverMenu::from_model(Some(&tree_menu_model()));
        tree_menu.set_has_arrow(false);
        tree_menu.set_halign(gtk::Align::Start);
        tree_menu.set_parent(&tree.view);

        let this = Rc::new(Browser {
            ctx: ctx.clone(),
            window: window.downgrade(),
            content,
            toasts,
            banner,
            pathbar,
            rev_button,
            rev_entry,
            rev_error,
            tree,
            files,
            filter_entry,
            list_stack,
            list_error,
            status_label,
            status_spinner,
            cancel_button,
            actions: gio::SimpleActionGroup::new(),
            row_actions: gio::SimpleActionGroup::new(),
            bookmarks_menu,
            file_menu,
            tree_menu,
            state: RefCell::new(State {
                cache: None,
                loc: None,
                history: History::new(),
                pending_select: None,
                reveal_gen: 0,
                reveal_loading: None,
                info_gen: 0,
                info_token: None,
                context: None,
                syncing_tree: false,
                from_tree: false,
                list_touched: false,
            }),
            opened: Cell::new(Some(std::time::Instant::now())),
            keep_top_queued: Cell::new(false),
        });
        *this_cell.borrow_mut() = Rc::downgrade(&this);

        this.install_actions(&window);
        this.install_shortcuts(&window);
        this.connect_signals(&rev_go);
        this.rebuild_bookmarks();
        {
            let weak = Rc::downgrade(&this);
            ctx.settings.connect_bookmarks_changed(move || {
                if let Some(this) = weak.upgrade() {
                    this.rebuild_bookmarks();
                }
            });
        }

        // The window owns the browser; tear down on close.
        let keep = RefCell::new(Some(this.clone()));
        window.connect_destroy(move |_| {
            if let Some(this) = keep.take() {
                this.shutdown();
            }
        });

        #[cfg(debug_assertions)]
        debug::install(&this, &window);
        #[cfg(debug_assertions)]
        crate::e2e::register(&window, Rc::downgrade(&this) as std::rc::Weak<dyn crate::e2e::Scriptable>);

        this
    }

    /// Cancel everything; called when the window is destroyed.
    fn shutdown(&self) {
        let mut st = self.state.borrow_mut();
        if let Some(cache) = st.cache.take() {
            cache.abort_all();
        }
        if let Some(t) = st.info_token.take() {
            t.cancel();
        }
        st.reveal_gen += 1;
        st.reveal_loading = None;
        drop(st);
        self.file_menu.unparent();
        self.tree_menu.unparent();
    }

    // ----- signals and actions -------------------------------------------------

    fn connect_signals(self: &Rc<Self>, rev_go: &gtk::Button) {
        let weak = Rc::downgrade(self);
        let with = move |f: fn(&Rc<Browser>)| {
            let weak = weak.clone();
            move || {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            }
        };

        {
            let weak = Rc::downgrade(self);
            self.files.view.connect_activate(move |_, pos| {
                if let Some(this) = weak.upgrade()
                    && let Some(entry) = this.files.item(pos)
                {
                    this.open_entry(&entry);
                }
            });
        }
        {
            // Any interaction with the list stops `keep_top`.
            let weak = Rc::downgrade(self);
            let touched = move || {
                if let Some(this) = weak.upgrade() {
                    this.state.borrow_mut().list_touched = true;
                }
            };
            let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
            let t = touched.clone();
            scroll.connect_scroll(move |_, _, _| {
                t();
                glib::Propagation::Proceed
            });
            scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
            self.files.scrolled.add_controller(scroll);
            let click = gtk::GestureClick::builder()
                .button(0)
                .propagation_phase(gtk::PropagationPhase::Capture)
                .build();
            let t = touched.clone();
            click.connect_pressed(move |_, _, _, _| t());
            self.files.scrolled.add_controller(click);
            let keys = gtk::EventControllerKey::new();
            keys.set_propagation_phase(gtk::PropagationPhase::Capture);
            keys.connect_key_pressed(move |_, _, _, _| {
                touched();
                glib::Propagation::Proceed
            });
            self.files.scrolled.add_controller(keys);
        }
        {
            let f = with(|this| this.on_tree_selection());
            self.tree.selection.connect_selection_changed(move |_, _, _| f());
        }
        {
            let f = with(|this| this.update_status());
            self.files.filter_model.connect_items_changed(move |_, _, _, _| f());
        }
        {
            let weak = Rc::downgrade(self);
            self.files.sort_model.connect_pending_notify(move |model| {
                if model.pending() == 0
                    && let Some(this) = weak.upgrade()
                {
                    // The incremental sort just moved the directories (which
                    // arrive last) to the front; the view kept its anchor row.
                    this.keep_top();
                }
                if model.pending() == 0
                    && let Some(this) = weak.upgrade()
                    && let Some(dir) = this.current_dir()
                    && dir.is_done()
                    && let Some(t) = dir.started.get()
                {
                    glib::g_debug!(LOG_DOMAIN, "sorted {} items, {:?} after listing start", model.n_items(), t.elapsed());
                }
            });
        }
        {
            let f = with(|this| {
                let text = this.filter_entry.text();
                this.files.set_filter_text(&text);
            });
            self.filter_entry.connect_search_changed(move |_| f());
        }
        {
            let f = with(|this| {
                this.filter_entry.set_text("");
                this.files.view.grab_focus();
            });
            self.filter_entry.connect_stop_search(move |_| f());
        }
        {
            // Enter in the filter opens the first match.
            let f = with(|this| {
                if this.files.n_shown() > 0 {
                    this.files.view.scroll_to(
                        0,
                        None::<&gtk::ColumnViewColumn>,
                        gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
                        None,
                    );
                }
            });
            self.filter_entry.connect_activate(move |_| f());
        }
        {
            let f = with(|this| {
                let parent = this.state.borrow().loc.as_ref().and_then(|l| l.url.parent());
                if let (Some(parent), Some(loc)) = (parent, this.state.borrow().loc.clone()) {
                    crate::log::open(&this.ctx, parent, loc.rev);
                }
            });
            self.banner.connect_button_clicked(move |_| f());
        }
        {
            let f = with(|this| this.go_to_revision());
            rev_go.connect_clicked(move |_| f());
        }
        {
            let f = with(|this| this.go_to_revision());
            self.rev_entry.connect_activate(move |_| f());
        }
        {
            let f = with(|this| {
                let st = this.state.borrow();
                if let Some(loc) = &st.loc {
                    this.rev_entry.set_text(&if loc.follow_head {
                        "HEAD".to_owned()
                    } else {
                        loc.rev.to_string()
                    });
                }
                this.rev_entry.select_region(0, -1);
                this.rev_error.set_visible(false);
            });
            if let Some(popover) = self.rev_button.popover() {
                popover.connect_show(move |_| f());
            }
        }
    }

    fn install_actions(self: &Rc<Self>, window: &adw::ApplicationWindow) {
        let weak = Rc::downgrade(self);
        let add = |group: &gio::SimpleActionGroup, name: &str, f: fn(&Rc<Browser>)| {
            let action = gio::SimpleAction::new(name, None);
            let weak = weak.clone();
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
            group.add_action(&action);
        };
        let g = &self.actions;
        add(g, "back", |this| this.go_history(false));
        add(g, "forward", |this| this.go_history(true));
        add(g, "up", |this| this.go_up());
        add(g, "refresh", |this| this.refresh());
        add(g, "location", |this| this.pathbar.start_editing());
        add(g, "show-log", |this| {
            if let Some(loc) = this.state.borrow().loc.clone() {
                crate::log::open(&this.ctx, loc.url, loc.rev);
            }
        });
        add(g, "bookmark", |this| {
            if let Some(loc) = this.state.borrow().loc.clone() {
                this.bookmark(&loc.url);
            }
        });
        add(g, "cancel", |this| {
            if let Some(dir) = this.current_dir() {
                dir.cancel();
            }
        });
        add(g, "find", |this| {
            this.filter_entry.grab_focus();
        });
        add(g, "context-menu", |this| this.popup_menu_for_focus());
        add(g, "revision", |this| {
            this.rev_button.popup();
        });
        {
            let action = gio::SimpleAction::new("open-bookmark", Some(glib::VariantTy::STRING));
            let weak = weak.clone();
            action.connect_activate(move |_, param| {
                let (Some(this), Some(param)) = (weak.upgrade(), param.and_then(|p| p.get::<String>())) else {
                    return;
                };
                let (url, rev) = param.split_once('\n').unwrap_or((&param, ""));
                match RepoUrl::parse(url) {
                    Ok(url) => this.go_to(url, Some(rev.parse().map_or(Rev::Head, Rev::Number))),
                    Err(e) => this.toast(&e.to_string()),
                }
            });
            g.add_action(&action);
        }
        window.insert_action_group("browser", Some(&self.actions));

        let r = &self.row_actions;
        add(r, "open", |this| {
            if let Some(e) = this.context_entry() {
                this.open_entry(&e);
            }
        });
        add(r, "log", |this| {
            if let (Some(e), Some(loc)) = (this.context_entry(), this.state.borrow().loc.clone()) {
                crate::log::open(&this.ctx, e.url(), loc.rev);
            }
        });
        add(r, "blame", |this| {
            if let (Some(e), Some(loc)) = (this.context_entry(), this.state.borrow().loc.clone()) {
                crate::viewer::open(&this.ctx, e.url(), loc.rev, e.data().size, ViewerMode::Blame);
            }
        });
        add(r, "copy-url", |this| {
            if let Some(e) = this.context_entry() {
                this.copy(e.url().as_str().to_owned());
            }
        });
        add(r, "copy-url-rev", |this| {
            if let (Some(e), Some(loc)) = (this.context_entry(), this.state.borrow().loc.clone()) {
                this.copy(e.url().with_peg(&Rev::Number(loc.rev)));
            }
        });
        add(r, "save-as", |this| {
            if let Some(e) = this.context_entry() {
                this.save_as(&e);
            }
        });
        add(r, "bookmark", |this| {
            if let Some(e) = this.context_entry() {
                this.bookmark(&e.url());
            }
        });
        window.insert_action_group("row", Some(&self.row_actions));
        self.update_actions();
    }

    fn install_shortcuts(self: &Rc<Self>, window: &adw::ApplicationWindow) {
        let controller = gtk::ShortcutController::new();
        crate::shortcuts::add_to(&controller, &crate::shortcuts::BROWSER);
        window.add_controller(controller);

        // Mouse back/forward buttons.
        let click = gtk::GestureClick::builder()
            .button(0)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |g, _, _, _| {
            let Some(this) = weak.upgrade() else { return };
            match g.current_button() {
                8 => this.go_history(false),
                9 => this.go_history(true),
                _ => return,
            }
            g.set_state(gtk::EventSequenceState::Claimed);
        });
        window.add_controller(click);
    }

    fn update_actions(&self) {
        let st = self.state.borrow();
        let has_loc = st.loc.is_some();
        let enable = |name: &str, on: bool| {
            if let Some(a) = self.actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(on);
            }
        };
        enable("back", st.history.can_go_back());
        enable("forward", st.history.can_go_forward());
        let at_root = match (&st.loc, &st.cache) {
            (Some(loc), Some(cache)) => loc.url == cache.root,
            _ => true,
        };
        enable("up", !at_root);
        for name in ["refresh", "location", "show-log", "bookmark", "find", "revision"] {
            enable(name, has_loc);
        }
        let loading = self.current_dir_of(&st).is_some_and(|d| d.is_loading());
        enable("cancel", loading);
    }

    fn rebuild_bookmarks(&self) {
        let menu = &self.bookmarks_menu;
        menu.remove_all();
        let bookmarks = self.ctx.settings.bookmarks();
        if bookmarks.is_empty() {
            menu.append(Some("No Bookmarks"), Some("browser.none"));
            return;
        }
        for b in bookmarks {
            let label = match b.rev {
                Some(rev) => format!("{} (r{rev})", b.name),
                None => b.name.clone(),
            };
            let target = format!("{}\n{}", b.url, b.rev.map(|r| r.to_string()).unwrap_or_default());
            let item = gio::MenuItem::new(Some(&label.replace('_', "__")), None);
            item.set_action_and_target_value(Some("browser.open-bookmark"), Some(&target.to_variant()));
            menu.append_item(&item);
        }
    }

    // ----- navigation -----------------------------------------------------------

    fn current_dir_of(&self, st: &State) -> Option<Rc<DirModel>> {
        let (cache, loc) = (st.cache.as_ref()?, st.loc.as_ref()?);
        cache.peek(&loc.url)
    }

    fn current_dir(&self) -> Option<Rc<DirModel>> {
        self.current_dir_of(&self.state.borrow())
    }

    fn root(&self) -> Option<RepoUrl> {
        self.state.borrow().cache.as_ref().map(|c| c.root.clone())
    }

    /// Run `svn info` for navigation, replacing any pending one.
    fn info_then(self: &Rc<Self>, url: &RepoUrl, rev: Rev, f: impl FnOnce(&Rc<Self>, Result<Info, SvnError>) + 'static) {
        let job = self.ctx.svn.info(url, &rev, Priority::Interactive);
        let generation = {
            let mut st = self.state.borrow_mut();
            if let Some(t) = st.info_token.replace(job.token()) {
                t.cancel();
            }
            st.info_gen += 1;
            st.info_gen
        };
        let weak = Rc::downgrade(self);
        common::spawn(async move {
            let result = job.result().await;
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().info_gen != generation {
                return;
            }
            this.state.borrow_mut().info_token = None;
            if matches!(&result, Err(e) if e.is_cancelled()) {
                return;
            }
            f(&this, result);
        });
    }

    /// Path bar entry activated.
    fn go_to_input(self: &Rc<Self>, text: &str) {
        let Some(root) = self.root() else { return };
        match nav::parse_location(text, &root) {
            Ok(input) => self.go_to(input.url, input.rev),
            Err(e) => self.toast(&e),
        }
    }

    /// Go to any URL. `rev = None` keeps the current revision (only
    /// meaningful inside the current repository). Inside the current
    /// repository at the current revision the target is listed at once;
    /// otherwise `svn info` resolves the repository root, revision and
    /// node kind first.
    fn go_to(self: &Rc<Self>, url: RepoUrl, rev: Option<Rev>) {
        let (cache, loc) = {
            let st = self.state.borrow();
            (st.cache.clone(), st.loc.clone())
        };
        if let (Some(cache), Some(loc), None) = (&cache, &loc, &rev)
            && let Some(path) = url.path_from(&cache.root)
        {
            let url = cache.root.join(&path);
            let known = cache.known_dir(&url);
            self.navigate(Location { url: url.clone(), ..loc.clone() }, None, true);
            if !known {
                // Might be a file: list right away, check the kind in parallel.
                let loc = loc.clone();
                self.info_then(&url.clone(), Rev::Number(loc.rev), move |this, result| {
                    if let Ok(info) = result
                        && info.kind == NodeKind::File
                        && let Some(parent) = url.parent()
                    {
                        if let Some(cache) = this.state.borrow().cache.clone() {
                            cache.remove(&url);
                        }
                        let loc = Location { url: parent, ..loc };
                        this.navigate_replace(loc, Some(url.name()));
                    }
                });
            }
            return;
        }

        let follow_head = matches!(rev, None | Some(Rev::Head));
        let rev = rev.unwrap_or(Rev::Head);
        if loc.is_none() {
            self.content.set_visible_child_name("loading");
        }
        self.info_then(&url.clone(), rev.clone(), move |this, result| match result {
            Ok(info) => {
                if let Some(t) = this.opened.get() {
                    glib::g_debug!(LOG_DOMAIN, "info {} in {:?}", url, t.elapsed());
                }
                let target = canonical(&info.root, &info.url);
                let (dir, select) = match info.kind {
                    NodeKind::File => (target.parent().unwrap_or(target.clone()), Some(target.name())),
                    NodeKind::Dir => (target, None),
                };
                let same_root = this.root().as_ref() == Some(&info.root);
                if !same_root {
                    this.reset(&info.root, info.rev);
                }
                let loc = Location {
                    url: dir,
                    rev: info.rev,
                    follow_head,
                };
                this.content.set_visible_child_name("browse");
                this.navigate(loc, select, true);
            }
            Err(err) => {
                let in_repo = this.root().is_some_and(|root| nav::is_within(&url, &root));
                if err.kind == ErrorKind::NotFound
                    && in_repo
                    && let Rev::Number(n) = rev
                {
                    // Show the "does not exist" banner for it.
                    let url = canonical(&this.root().expect("root"), &url);
                    this.navigate(Location { url, rev: n, follow_head: false }, None, true);
                } else if this.state.borrow().loc.is_some() {
                    this.toast(&format!("Cannot open {}: {err}", decode(url.as_str())));
                } else {
                    let weak = Rc::downgrade(this);
                    let retry_url = url.clone();
                    let retry_rev = rev.clone();
                    let retry: Box<dyn Fn()> = Box::new(move || {
                        if let Some(this) = weak.upgrade() {
                            this.go_to(retry_url.clone(), Some(retry_rev.clone()));
                        }
                    });
                    // Nothing was ever shown here: back to the start window
                    // to enter a different URL.
                    let weak = Rc::downgrade(this);
                    let change_url = url.clone();
                    let change_rev = match rev {
                        Rev::Number(n) => Some(n),
                        _ => None,
                    };
                    let change: Box<dyn Fn()> = Box::new(move || {
                        let Some(this) = weak.upgrade() else { return };
                        crate::start::open_with(&this.ctx, &change_url, change_rev);
                        if let Some(window) = this.window.upgrade() {
                            window.close();
                        }
                    });
                    let page = common::error_page_with(&err, Some(retry), Some(("_Change URL…", change)));
                    this.set_window_title(&url.name(), "");
                    this.content.remove(&this.content.child_by_name("error").expect("error page"));
                    this.content.add_named(&page, Some("error"));
                    this.content.set_visible_child_name("error");
                }
            }
        });
    }

    /// Fresh directory cache and tree for `root` at `rev`.
    fn reset(self: &Rc<Self>, root: &RepoUrl, rev: RevNum) {
        let listener = dir::weak_listener(self, |this, model| this.on_dir_event(model));
        let cache = Rc::new(DirCache::new(self.ctx.svn.clone(), root.clone(), rev, listener));
        let old = {
            let mut st = self.state.borrow_mut();
            st.reveal_gen += 1;
            st.reveal_loading = None;
            st.cache.replace(cache.clone())
        };
        if let Some(old) = old {
            old.abort_all();
        }
        self.tree.set_cache(&cache);
        if let Some(row) = self.tree.root_row() {
            row.set_expanded(true);
        }
    }

    /// Show the directory `loc`, optionally selecting the entry `select`.
    fn navigate(self: &Rc<Self>, loc: Location, select: Option<String>, push_history: bool) {
        let from_tree = std::mem::take(&mut self.state.borrow_mut().from_tree);
        let needs_reset = {
            let st = self.state.borrow();
            st.cache.as_ref().is_none_or(|c| c.rev != loc.rev || !nav::is_within(&loc.url, &c.root))
        };
        if needs_reset {
            let root = self.root().filter(|r| nav::is_within(&loc.url, r));
            let Some(root) = root else {
                // Different repository: resolve it.
                return self.go_to(loc.url, Some(Rev::Number(loc.rev)));
            };
            self.reset(&root, loc.rev);
        }

        let (cache, previous) = {
            let st = self.state.borrow();
            (st.cache.clone().expect("cache"), self.current_dir_of(&st))
        };
        let dir = cache.get(&loc.url);
        if let Some(prev) = previous
            && !Rc::ptr_eq(&prev, &dir)
            && prev.is_loading()
            && !prev.expanded.get()
        {
            // Nobody needs the old listing any more.
            prev.cancel();
        }
        {
            let mut st = self.state.borrow_mut();
            if push_history {
                st.history.visit(loc.clone());
            } else {
                st.history.replace(loc.clone());
            }
            st.loc = Some(loc.clone());
            st.list_touched = select.is_some();
            st.pending_select = select;
        }
        self.pathbar.set_location(&cache.root, &loc.url);
        let path = loc.url.path_from(&cache.root).unwrap_or_else(|| "/".into());
        self.set_window_title(&cache.root.name(), &path);
        self.rev_button.set_label(&nav::rev_label(loc.rev, loc.follow_head));
        self.banner.set_revealed(false);
        self.filter_entry.set_text("");
        self.files.selection.set_selected(gtk::INVALID_LIST_POSITION);
        self.files.set_store(&dir.store);
        dir.ensure(Priority::Interactive);
        self.on_dir_event(&dir);
        self.reveal_in_tree(loc.url.clone(), !from_tree);
        self.update_actions();
    }

    /// Like navigate, replacing the current history entry.
    fn navigate_replace(self: &Rc<Self>, loc: Location, select: Option<String>) {
        self.navigate(loc, select, false);
    }

    fn go_history(self: &Rc<Self>, forward: bool) {
        let loc = {
            let mut st = self.state.borrow_mut();
            if forward { st.history.go_forward() } else { st.history.go_back() }
        };
        if let Some(loc) = loc {
            self.navigate(loc, None, false);
        }
    }

    fn go_up(self: &Rc<Self>) {
        let Some(loc) = self.state.borrow().loc.clone() else { return };
        let Some(root) = self.root() else { return };
        if loc.url == root {
            return;
        }
        if let Some(parent) = loc.url.parent() {
            let name = loc.url.name();
            self.navigate(Location { url: parent, ..loc }, Some(name), true);
        }
    }

    /// F5: re-resolve HEAD when following it, else reload the directory.
    fn refresh(self: &Rc<Self>) {
        let Some(loc) = self.state.borrow().loc.clone() else { return };
        let Some(root) = self.root() else { return };
        if !loc.follow_head {
            if let Some(dir) = self.current_dir() {
                dir.reload(Priority::Interactive);
                self.on_dir_event(&dir);
            }
            return;
        }
        self.status_label.set_label("Checking for new revisions…");
        self.info_then(&root, Rev::Head, move |this, result| match result {
            Ok(info) if info.rev != loc.rev => {
                this.toast(&format!("Updated to r{}", info.rev));
                this.navigate(Location { rev: info.rev, ..loc }, None, false);
            }
            Ok(_) => {
                if let Some(dir) = this.current_dir() {
                    dir.reload(Priority::Interactive);
                    this.on_dir_event(&dir);
                }
            }
            Err(err) => {
                this.update_status();
                this.toast(&format!("Refresh failed: {err}"));
            }
        });
    }

    /// The revision popover's Go.
    fn go_to_revision(self: &Rc<Self>) {
        let rev = match nav::parse_rev_target(&self.rev_entry.text()) {
            Ok(rev) => rev,
            Err(e) => {
                self.rev_error.set_label(&e);
                self.rev_error.set_visible(true);
                return;
            }
        };
        let (Some(loc), Some(root)) = (self.state.borrow().loc.clone(), self.root()) else { return };
        let follow_head = rev == Rev::Head;
        self.rev_error.set_visible(false);
        self.info_then(&root, rev, move |this, result| match result {
            Ok(info) => {
                this.rev_button.popdown();
                this.navigate(Location { url: loc.url, rev: info.rev, follow_head }, None, true);
            }
            Err(err) => {
                this.rev_error.set_label(&err.to_string());
                this.rev_error.set_visible(true);
            }
        });
    }

    fn on_tree_selection(self: &Rc<Self>) {
        if self.state.borrow().syncing_tree {
            return;
        }
        let Some(entry) = self.tree.selected_entry() else { return };
        if !entry.is_dir() {
            return;
        }
        let Some(loc) = self.state.borrow().loc.clone() else { return };
        let url = entry.url();
        if url != loc.url {
            self.state.borrow_mut().from_tree = true;
            self.navigate(Location { url, ..loc }, None, true);
        }
    }

    /// Activate (double-click / Enter / Open) an entry.
    fn open_entry(self: &Rc<Self>, entry: &EntryObject) {
        let Some(loc) = self.state.borrow().loc.clone() else { return };
        if entry.is_dir() {
            self.navigate(Location { url: entry.url(), ..loc }, None, true);
        } else if !entry.is_placeholder() {
            crate::viewer::open(&self.ctx, entry.url(), loc.rev, entry.data().size, ViewerMode::Content);
        }
    }

    // ----- tree sync --------------------------------------------------------------

    /// Expand the tree along the path to `target` in the background and
    /// select it. Ancestors are listed at background priority, one after
    /// the other; a newer reveal cancels this one. `expand_target` also
    /// expands the target row itself (not wanted when the user just
    /// selected it in the tree).
    fn reveal_in_tree(self: &Rc<Self>, target: RepoUrl, expand_target: bool) {
        let (generation, cache) = {
            let mut st = self.state.borrow_mut();
            st.reveal_gen += 1;
            if let Some((dir, row)) = st.reveal_loading.take()
                && !nav::is_within(&target, &dir.url)
                && dir.is_loading()
            {
                dir.cancel();
                row.set_expanded(false);
            }
            (st.reveal_gen, st.cache.clone())
        };
        let Some(cache) = cache else { return };
        let Some(mut row) = self.tree.root_row() else { return };
        let segments = nav::segments(&cache.root, &target);
        let weak = Rc::downgrade(self);
        let started = std::time::Instant::now();
        common::spawn(async move {
            let mut url = cache.root.clone();
            for seg in &segments {
                let dir = cache.get(&url);
                if !row.is_expanded() {
                    dir.ensure(Priority::Background);
                    row.set_expanded(true);
                }
                if dir.is_loading() {
                    {
                        let Some(this) = weak.upgrade() else { return };
                        this.state.borrow_mut().reveal_loading = Some((dir.clone(), row.clone()));
                    }
                    dir.wait_idle().await;
                    let Some(this) = weak.upgrade() else { return };
                    let mut st = this.state.borrow_mut();
                    if st.reveal_gen != generation {
                        return;
                    }
                    st.reveal_loading = None;
                }
                if !dir.is_done() {
                    return;
                }
                let Some(child) = tree::child_row(&row, seg) else { return };
                row = child;
                url = url.join(seg);
            }
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().reveal_gen != generation {
                return;
            }
            if segments.len() > 1 {
                glib::g_debug!(LOG_DOMAIN, "tree revealed {} levels in {:?}", segments.len(), started.elapsed());
            }
            // Show the target's subdirectories too: same listing as the file list.
            let expanded = expand_target
                && !row.is_expanded()
                && !row.item().and_downcast::<EntryObject>().is_some_and(|e| e.is_childless());
            if expanded {
                row.set_expanded(true);
            }
            let pos = row.position();
            this.state.borrow_mut().syncing_tree = true;
            this.tree.selection.set_selected(pos);
            this.state.borrow_mut().syncing_tree = false;
            // Again over the next frames: rows bound in between can differ
            // from the estimated height and leave the target just off screen.
            this.tree.view.scroll_to(pos, gtk::ListScrollFlags::NONE, None);
            let tick_weak = weak.clone();
            let frames = Cell::new(0);
            this.tree.view.add_tick_callback(move |view, _| {
                frames.set(frames.get() + 1);
                let Some(this) = tick_weak.upgrade() else { return glib::ControlFlow::Break };
                if this.state.borrow().reveal_gen != generation {
                    return glib::ControlFlow::Break;
                }
                view.scroll_to(pos, gtk::ListScrollFlags::NONE, None);
                if frames.get() < 3 { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
            });
            if !expanded {
                return;
            }
            // Once the subdirectories are listed, bring as many of them into
            // view as fit below the target (a target at the bottom of the
            // sidebar would otherwise hide them all): scroll to the last
            // child, then on the next frame back to the target if that
            // scrolled it out. One scroll per frame, after the ones above,
            // as each `scroll_to` replaces the last until the next layout.
            let dir = cache.get(&url);
            drop(this);
            dir.wait_idle().await;
            let Some(this) = weak.upgrade() else { return };
            if this.state.borrow().reveal_gen != generation {
                return;
            }
            let frames = Cell::new(0);
            this.tree.view.add_tick_callback(move |view, _| {
                frames.set(frames.get() + 1);
                let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
                if this.state.borrow().reveal_gen != generation || !row.is_expanded() {
                    return glib::ControlFlow::Break;
                }
                match frames.get() {
                    4 => {
                        let n = row.children().map_or(0, |c| c.n_items());
                        let Some(last) = n.checked_sub(1).and_then(|i| row.child_row(i)) else {
                            return glib::ControlFlow::Break;
                        };
                        view.scroll_to(last.position(), gtk::ListScrollFlags::NONE, None);
                    }
                    5 => {
                        view.scroll_to(row.position(), gtk::ListScrollFlags::NONE, None);
                        return glib::ControlFlow::Break;
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            });
        });
    }

    // ----- listing events ------------------------------------------------------------

    /// A directory model changed (batch arrived, finished, failed...).
    fn on_dir_event(self: &Rc<Self>, dir: &Rc<DirModel>) {
        let is_current = self.current_dir().is_some_and(|d| Rc::ptr_eq(&d, dir));
        if !is_current {
            return;
        }
        match dir.state() {
            LoadState::Done => {
                if let (Some(t), Some(first)) = (dir.started.get(), dir.first_rows.get()) {
                    glib::g_debug!(
                        LOG_DOMAIN,
                        "listed {} ({} items): first rows {:?}, complete {:?}, RSS {}",
                        dir.url,
                        dir.n_items(),
                        first,
                        t.elapsed(),
                        rss()
                    );
                }
                self.list_stack.set_visible_child_name("list");
                self.keep_top();
                self.select_pending();
            }
            LoadState::Failed(err) if err.kind == ErrorKind::NotFound => {
                let loc = self.state.borrow().loc.clone();
                if let (Some(loc), Some(root)) = (loc, self.root()) {
                    let path = loc.url.path_from(&root).unwrap_or_default();
                    self.banner.set_title(&glib::markup_escape_text(&format!(
                        "{path} does not exist in r{}",
                        loc.rev
                    )));
                    self.banner.set_revealed(true);
                }
                self.list_stack.set_visible_child_name("list");
            }
            LoadState::Failed(err) => {
                let weak = Rc::downgrade(self);
                let page = common::error_page(
                    &err,
                    Some(Box::new(move || {
                        if let Some(this) = weak.upgrade()
                            && let Some(dir) = this.current_dir()
                        {
                            dir.reload(Priority::Interactive);
                            this.on_dir_event(&dir);
                        }
                    })),
                );
                self.list_error.set_child(Some(&page));
                self.list_stack.set_visible_child_name("error");
            }
            LoadState::Loading | LoadState::Idle | LoadState::Cancelled => {
                self.list_stack.set_visible_child_name("list");
                self.keep_top();
            }
        }
        self.update_status();
        self.update_actions();
    }

    /// Keep an untouched list at the top while it fills. Deferred to an
    /// idle so it runs after the view has processed the model change (it
    /// keeps its anchor row, which would undo an immediate scroll).
    fn keep_top(self: &Rc<Self>) {
        if self.keep_top_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(this) = weak.upgrade() else { return };
            this.keep_top_queued.set(false);
            if !this.state.borrow().list_touched && this.files.n_shown() > 0 {
                this.files.view.scroll_to(0, None::<&gtk::ColumnViewColumn>, gtk::ListScrollFlags::NONE, None);
            }
        });
    }

    fn select_pending(self: &Rc<Self>) {
        let Some(name) = self.state.borrow().pending_select.clone() else { return };
        if self.files.sort_model.pending() > 0 {
            // Still sorting; try again shortly.
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(std::time::Duration::from_millis(30), move || {
                if let Some(this) = weak.upgrade() {
                    this.select_pending();
                }
            });
            return;
        }
        self.state.borrow_mut().pending_select = None;
        self.files.select_name(&name);
    }

    fn update_status(&self) {
        let Some(dir) = self.current_dir() else { return };
        let total = dir.n_items();
        let shown = self.files.filter_model.n_items();
        let path = self
            .root()
            .and_then(|r| dir.url.path_from(&r))
            .unwrap_or_else(|| "/".into());
        let count = if shown != total && !self.filter_entry.text().is_empty() {
            format!("{} of {} items", fmt_count(shown), fmt_count(total))
        } else {
            items(total)
        };
        let (text, loading) = match dir.state() {
            LoadState::Loading => (format!("Loading {path}… {}", items(total)), true),
            LoadState::Done => (count, false),
            LoadState::Cancelled => (format!("Listing cancelled — {count} (incomplete)"), false),
            LoadState::Failed(err) if err.kind == ErrorKind::NotFound => ("Not found".to_owned(), false),
            LoadState::Failed(_) => ("Listing failed".to_owned(), false),
            LoadState::Idle => (String::new(), false),
        };
        self.status_label.set_label(&text);
        self.status_spinner.set_visible(loading);
        self.cancel_button.set_visible(loading);
    }

    // ----- context menu and row actions ---------------------------------------------

    fn context_entry(&self) -> Option<EntryObject> {
        self.state.borrow().context.clone()
    }

    fn popup_menu(&self, entry: &EntryObject, in_tree: bool, widget: &gtk::Widget, x: f64, y: f64) {
        let is_file = entry.kind() == entry::EntryKind::File;
        for (name, on) in [("blame", is_file), ("save-as", is_file)] {
            if let Some(a) = self.row_actions.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                a.set_enabled(on);
            }
        }
        self.state.borrow_mut().context = Some(entry.clone());
        let menu = if in_tree { &self.tree_menu } else { &self.file_menu };
        let Some(parent) = menu.parent() else { return };
        let point = widget
            .compute_point(&parent, &gtk::graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| gtk::graphene::Point::new(0.0, 0.0));
        menu.set_pointing_to(Some(&gdk::Rectangle::new(point.x() as i32, point.y() as i32, 1, 1)));
        menu.popup();
    }

    /// Menu key / Shift+F10: menu for the selection of the focused view.
    fn popup_menu_for_focus(&self) {
        let Some(window) = self.window.upgrade() else { return };
        let Some(focus) = gtk::prelude::GtkWindowExt::focus(&window) else { return };
        let in_tree = focus.is_ancestor(&self.tree.view) || focus == *self.tree.view.upcast_ref::<gtk::Widget>();
        let entry = if in_tree { self.tree.selected_entry() } else { self.files.selected() };
        let Some(entry) = entry else { return };
        let h = focus.height() as f64;
        self.popup_menu(&entry, in_tree, &focus, 24.0, h / 2.0);
    }

    fn copy(&self, text: String) {
        if let Some(window) = self.window.upgrade() {
            window.clipboard().set_text(&text);
            self.toast("Copied to clipboard");
        }
    }

    fn bookmark(&self, url: &RepoUrl) {
        let Some(loc) = self.state.borrow().loc.clone() else { return };
        let name = bookmark_name(self.root().as_ref(), url);
        let rev = (!loc.follow_head).then_some(loc.rev);
        let settings = &self.ctx.settings;
        if settings.is_bookmarked(url.as_str(), rev) {
            // Ctrl+D again: offer to remove it rather than adding a duplicate.
            let toast = adw::Toast::builder()
                .title(glib::markup_escape_text(&format!("“{name}” is already bookmarked")))
                .button_label("_Remove")
                .timeout(5)
                .build();
            let ctx = self.ctx.clone();
            let url = url.as_str().to_owned();
            toast.connect_button_clicked(move |_| ctx.settings.remove_bookmark(&url, rev));
            self.toasts.add_toast(toast);
            return;
        }
        settings.add_bookmark(Bookmark {
            name: name.clone(),
            url: url.as_str().to_owned(),
            rev,
        });
        self.toast(&format!("Bookmarked “{name}”"));
    }

    fn save_as(self: &Rc<Self>, entry: &EntryObject) {
        let Some(window) = self.window.upgrade() else { return };
        let Some(loc) = self.state.borrow().loc.clone() else { return };
        let dialog = gtk::FileDialog::builder()
            .title("Save File")
            .modal(true)
            .initial_name(entry.name())
            .build();
        let url = entry.url();
        let weak = Rc::downgrade(self);
        common::spawn(async move {
            let Ok(file) = dialog.save_future(Some(&window)).await else { return };
            Self::save_to(weak, url, loc.rev, file).await;
        });
    }

    /// Download `url@rev` into `file`, reporting with toasts.
    async fn save_to(weak: std::rc::Weak<Self>, url: RepoUrl, rev: RevNum, file: gio::File) {
        let Some(this) = weak.upgrade() else { return };
        let name = url.name();
        // Shown until the download ends (not for its full timeout).
        let progress = adw::Toast::builder()
            .title(glib::markup_escape_text(&format!("Downloading “{name}”…")))
            .timeout(0)
            .build();
        this.toasts.add_toast(progress.clone());
        let job = this.ctx.svn.cat(&url, rev, None, Priority::Interactive);
        drop(this);
        let result = job.result().await;
        progress.dismiss();
        let message = match result {
            Ok(out) => match file
                .replace_contents_future(out.data, None, false, gio::FileCreateFlags::REPLACE_DESTINATION)
                .await
            {
                Ok(_) => format!("Saved “{name}”"),
                Err((_, e)) => format!("Cannot save “{name}”: {e}"),
            },
            Err(e) if e.is_cancelled() => return,
            Err(e) => format!("Cannot download “{name}”: {e}"),
        };
        if let Some(this) = weak.upgrade() {
            this.toast(&message);
        }
    }

    fn toast(&self, text: &str) {
        let toast = adw::Toast::builder().title(glib::markup_escape_text(text)).timeout(3).build();
        self.toasts.add_toast(toast);
    }

    fn set_window_title(&self, repo: &str, path: &str) {
        if let Some(window) = self.window.upgrade() {
            let title = if path.is_empty() { repo.to_owned() } else { format!("{repo} — {path}") };
            window.set_title(Some(&title));
        }
    }
}

/// `url` re-encoded the way [`RepoUrl::join`] does, so URLs from svn and
/// from navigation compare equal.
fn canonical(root: &RepoUrl, url: &RepoUrl) -> RepoUrl {
    match url.path_from(root) {
        Some(path) => root.join(&path),
        None => url.clone(),
    }
}

/// Default bookmark name: the repository name at its root, else the
/// last path segment, else the whole URL.
fn bookmark_name(root: Option<&RepoUrl>, url: &RepoUrl) -> String {
    match (root, url.name()) {
        (Some(root), _) if url == root => root.name(),
        (_, name) if !name.is_empty() => name,
        _ => decode(url.as_str()),
    }
}

/// `12345` → `12,345`.
fn fmt_count(n: u32) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn items(n: u32) -> String {
    if n == 1 { "1 item".to_owned() } else { format!("{} items", fmt_count(n)) }
}

/// Resident set size, for debug logging.
fn rss() -> String {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmRSS:")).map(|l| l[6..].trim().to_owned()))
        .unwrap_or_default()
}

fn revision_popover() -> (gtk::Popover, gtk::Entry, gtk::Label, gtk::Button) {
    let entry = gtk::Entry::builder()
        .placeholder_text("HEAD, 1234 or 2024-01-31")
        .width_chars(22)
        .build();
    let go = gtk::Button::builder()
        .label("_Go")
        .use_underline(true)
        .css_classes(["suggested-action"])
        .build();
    let row = gtk::Box::builder().spacing(6).build();
    row.append(&entry);
    row.append(&go);
    let title = gtk::Label::builder()
        .label("Browse at Revision")
        .xalign(0.0)
        .css_classes(["heading"])
        .build();
    let hint = gtk::Label::builder()
        .label("A revision number, HEAD, or a date like {2024-01-31}")
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(30)
        .css_classes(["dim-label", "caption"])
        .build();
    let error = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(30)
        .css_classes(["error", "caption"])
        .visible(false)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    content.append(&title);
    content.append(&row);
    content.append(&hint);
    content.append(&error);
    let popover = gtk::Popover::builder().child(&content).build();
    popover.set_default_widget(Some(&go));
    (popover, entry, error, go)
}

fn file_menu_model() -> gio::Menu {
    let menu = gio::Menu::new();
    let s = gio::Menu::new();
    s.append(Some("_Open"), Some("row.open"));
    menu.append_section(None, &s);
    let s = gio::Menu::new();
    s.append(Some("Show _Log"), Some("row.log"));
    s.append(Some("_Blame"), Some("row.blame"));
    menu.append_section(None, &s);
    let s = gio::Menu::new();
    s.append(Some("_Copy URL"), Some("row.copy-url"));
    s.append(Some("Copy URL with _Revision"), Some("row.copy-url-rev"));
    menu.append_section(None, &s);
    let s = gio::Menu::new();
    s.append(Some("_Save As…"), Some("row.save-as"));
    s.append(Some("Book_mark"), Some("row.bookmark"));
    menu.append_section(None, &s);
    menu
}

fn tree_menu_model() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some("Show _Log"), Some("row.log"));
    menu.append(Some("_Copy URL"), Some("row.copy-url"));
    menu.append(Some("Book_mark"), Some("row.bookmark"));
    menu
}

fn install_css() {
    thread_local! {
        static INSTALLED: Cell<bool> = const { Cell::new(false) };
    }
    if INSTALLED.replace(true) {
        return;
    }
    let Some(display) = gdk::Display::default() else { return };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "
        .gsvn-pathbar {
            background-color: alpha(currentColor, 0.08);
            border-radius: 6px;
            min-height: 34px;
            padding: 0 2px;
        }
        .gsvn-pathbar button.gsvn-crumb {
            padding: 2px 6px;
            min-height: 26px;
            font-weight: normal;
        }
        .gsvn-pathbar button.gsvn-crumb-current {
            font-weight: bold;
        }
        .gsvn-depth-badge {
            font-size: 0.8em;
            font-weight: bold;
            padding: 0 5px;
            border-radius: 999px;
            background-color: alpha(currentColor, 0.12);
        }
        .gsvn-status {
            padding: 3px 12px;
            min-height: 28px;
        }
        ",
    );
    gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1,000");
        assert_eq!(fmt_count(100_000), "100,000");
        assert_eq!(fmt_count(1_234_567), "1,234,567");
        assert_eq!(items(1), "1 item");
        assert_eq!(items(12345), "12,345 items");
    }

    #[test]
    fn bookmark_names() {
        let root = RepoUrl::parse("http://h/svn/small").unwrap();
        assert_eq!(bookmark_name(Some(&root), &root), "small");
        assert_eq!(bookmark_name(Some(&root), &root.join("trunk/doc dir")), "doc dir");
        assert_eq!(bookmark_name(None, &root.join("trunk")), "trunk");
    }

    #[test]
    fn canonical_urls() {
        let root = RepoUrl::parse("http://h/r").unwrap();
        let svn_style = RepoUrl::parse("http://h/r/a%20b/c%40d").unwrap();
        assert_eq!(canonical(&root, &svn_style), root.join("a b/c@d"));
        let other = RepoUrl::parse("http://h/x").unwrap();
        assert_eq!(canonical(&root, &other), other);
    }
}
