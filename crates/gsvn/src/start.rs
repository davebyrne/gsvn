//! Start window: URL entry, recent repositories, bookmarks (DESIGN.md §5.4).

use crate::app::Ctx;
use crate::common;
use crate::settings::Bookmark;
use adw::prelude::*;
use gtk::glib;
use std::rc::Rc;
use svn_backend::url::decode;
use svn_backend::{RepoUrl, RevNum};

pub fn open(ctx: &Rc<Ctx>) {
    open_window(ctx);
}

/// Like [`open`], returning the presented window (e.g. to parent a dialog).
pub fn open_window(ctx: &Rc<Ctx>) -> adw::ApplicationWindow {
    let window = common::new_window(ctx, "start", "gsvn", (640, 560));

    let header = adw::HeaderBar::new();
    header.pack_end(&common::main_menu_button(&common::main_menu()));
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);

    // "Open Repository"
    let url_row = adw::EntryRow::builder()
        .title("Repository URL")
        .input_purpose(gtk::InputPurpose::Url)
        .build();
    let rev_row = adw::EntryRow::builder()
        .title("Revision (empty for HEAD)")
        .input_purpose(gtk::InputPurpose::Digits)
        .build();
    let open_group = adw::PreferencesGroup::builder()
        .title("Open Repository")
        .build();
    open_group.add(&url_row);
    open_group.add(&rev_row);
    let error_label = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .css_classes(["error"])
        .visible(false)
        .build();
    let open_button = gtk::Button::builder()
        .label("_Open")
        .use_underline(true)
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    let open_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    open_box.append(&open_group);
    open_box.append(&error_label);
    open_box.append(&open_button);

    // "Bookmarks" and "Recent"
    let bookmarks_list = boxed_list();
    let bookmarks_group = adw::PreferencesGroup::builder().title("Bookmarks").build();
    bookmarks_group.add(&bookmarks_list);

    let recent_list = boxed_list();
    let clear_button = gtk::Button::builder()
        .label("_Clear")
        .use_underline(true)
        .valign(gtk::Align::Center)
        .tooltip_text("Clear Recent Repositories")
        .css_classes(["flat"])
        .build();
    let recent_group = adw::PreferencesGroup::builder()
        .title("Recent")
        .header_suffix(&clear_button)
        .build();
    recent_group.add(&recent_list);

    let empty = adw::StatusPage::builder()
        .icon_name("folder-remote-symbolic")
        .title("No Repositories Yet")
        .description("Repositories you open or bookmark will appear here")
        .css_classes(["compact"])
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&open_box);
    content.append(&bookmarks_group);
    content.append(&recent_group);
    content.append(&empty);
    let clamp = adw::Clamp::builder().maximum_size(600).child(&content).build();
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&clamp)
        .build();
    view.set_content(Some(&scrolled));
    window.set_content(Some(&view));

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        window: window.downgrade(),
        url_row: url_row.clone(),
        rev_row: rev_row.clone(),
        error_label,
        bookmarks_list,
        bookmarks_group,
        recent_list,
        recent_group,
        empty,
    });
    page.rebuild();

    {
        let page = Rc::downgrade(&page);
        ctx.settings.connect_bookmarks_changed(move || {
            if let Some(page) = page.upgrade() {
                page.rebuild_bookmarks();
                page.update_empty();
            }
        });
    }
    // Widget handlers hold weak references; the window owns the page.
    let weak = Rc::downgrade(&page);
    let with_page = move |f: fn(&Rc<Page>)| {
        let weak = weak.clone();
        move || {
            if let Some(page) = weak.upgrade() {
                f(&page);
            }
        }
    };
    {
        let f = with_page(|page| {
            page.ctx.settings.clear_recent();
            page.rebuild();
        });
        clear_button.connect_clicked(move |_| f());
    }
    for row in [&url_row, &rev_row] {
        let f = with_page(|page| page.open_from_entry());
        row.connect_entry_activated(move |_| f());
        let f = with_page(|page| page.clear_error());
        row.connect_changed(move |_| f());
    }
    {
        let f = with_page(|page| page.open_from_entry());
        open_button.connect_clicked(move |_| f());
    }
    window.connect_destroy(move |_| {
        let _keep_alive = &page;
    });

    window.present();
    url_row.grab_focus();
    window
}

fn boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

/// `https://h/a%20b` → `https://h/a b`, plus ` @ r5` for a pinned revision.
fn display_url(url: &str, rev: Option<RevNum>) -> String {
    let mut s = decode(url);
    if let Some(rev) = rev {
        s.push_str(&format!(" @ r{rev}"));
    }
    s
}

struct Page {
    ctx: Rc<Ctx>,
    window: glib::WeakRef<adw::ApplicationWindow>,
    url_row: adw::EntryRow,
    rev_row: adw::EntryRow,
    error_label: gtk::Label,
    bookmarks_list: gtk::ListBox,
    bookmarks_group: adw::PreferencesGroup,
    recent_list: gtk::ListBox,
    recent_group: adw::PreferencesGroup,
    empty: adw::StatusPage,
}

impl Page {
    fn rebuild(self: &Rc<Self>) {
        self.rebuild_bookmarks();
        self.rebuild_recent();
        self.update_empty();
    }

    fn update_empty(&self) {
        let empty = !self.bookmarks_group.is_visible() && !self.recent_group.is_visible();
        self.empty.set_visible(empty);
    }

    fn rebuild_bookmarks(self: &Rc<Self>) {
        let bookmarks = self.ctx.settings.bookmarks();
        self.bookmarks_list.remove_all();
        self.bookmarks_group.set_visible(!bookmarks.is_empty());
        for bookmark in bookmarks {
            self.bookmarks_list.append(&self.bookmark_row(bookmark));
        }
    }

    fn bookmark_row(self: &Rc<Self>, bookmark: Bookmark) -> adw::ActionRow {
        let row = adw::ActionRow::builder()
            .title(&bookmark.name)
            .subtitle(display_url(&bookmark.url, bookmark.rev))
            .use_markup(false)
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("user-bookmarks-symbolic"));
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove Bookmark")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        row.add_suffix(&remove);
        {
            let ctx = self.ctx.clone();
            let (url, rev) = (bookmark.url.clone(), bookmark.rev);
            // The settings change signal rebuilds the list.
            remove.connect_clicked(move |_| ctx.settings.remove_bookmark(&url, rev));
        }
        {
            let page = Rc::downgrade(self);
            row.connect_activated(move |_| {
                if let Some(page) = page.upgrade() {
                    page.open_saved(&bookmark.url, bookmark.rev);
                }
            });
        }
        row
    }

    fn rebuild_recent(self: &Rc<Self>) {
        let recent = self.ctx.settings.recent();
        self.recent_list.remove_all();
        self.recent_group.set_visible(!recent.is_empty());
        for url in recent {
            let name = RepoUrl::parse(&url).map(|u| u.name()).unwrap_or_default();
            let row = adw::ActionRow::builder()
                .title(if name.is_empty() { decode(&url) } else { name })
                .subtitle(display_url(&url, None))
                .use_markup(false)
                .activatable(true)
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("document-open-recent-symbolic"));
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            let page = Rc::downgrade(self);
            row.connect_activated(move |_| {
                if let Some(page) = page.upgrade() {
                    page.open_saved(&url, None);
                }
            });
            self.recent_list.append(&row);
        }
    }

    fn show_error(&self, row: &adw::EntryRow, message: &str) {
        self.error_label.set_label(message);
        self.error_label.set_visible(true);
        row.add_css_class("error");
        row.grab_focus();
    }

    fn clear_error(&self) {
        self.error_label.set_visible(false);
        self.url_row.remove_css_class("error");
        self.rev_row.remove_css_class("error");
    }

    fn open_from_entry(&self) {
        let url = match common::parse_url_input(&self.url_row.text()) {
            Ok(url) => url,
            Err(e) => return self.show_error(&self.url_row, &e),
        };
        let rev = match common::parse_rev_input(&self.rev_row.text()) {
            Ok(rev) => rev,
            Err(e) => return self.show_error(&self.rev_row, &e),
        };
        self.open(url, rev);
    }

    fn open_saved(&self, url: &str, rev: Option<RevNum>) {
        match RepoUrl::parse(url) {
            Ok(url) => self.open(url, rev),
            Err(e) => {
                let window = self.window.upgrade();
                common::show_message(window.as_ref().map(|w| w.upcast_ref()), "Invalid URL", &e.to_string());
            }
        }
    }

    fn open(&self, url: RepoUrl, rev: Option<RevNum>) {
        self.ctx.settings.add_recent(url.as_str());
        crate::browser::open(&self.ctx, url, rev);
        if let Some(window) = self.window.upgrade() {
            window.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_display() {
        assert_eq!(display_url("https://h/a%20b", None), "https://h/a b");
        assert_eq!(display_url("https://h/r", Some(7)), "https://h/r @ r7");
    }
}
