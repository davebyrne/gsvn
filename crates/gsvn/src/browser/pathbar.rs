//! Breadcrumb path bar that turns into an editable entry (DESIGN.md §5.3).

use super::nav;
use gtk::{glib, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;
use svn_backend::RepoUrl;
use svn_backend::url::decode;

pub struct PathBar {
    pub widget: gtk::Stack,
    crumbs: gtk::Box,
    pub entry: gtk::Entry,
    current: RefCell<Option<(RepoUrl, RepoUrl)>>,
    on_crumb: Box<dyn Fn(RepoUrl)>,
}

impl PathBar {
    /// `on_crumb` is called with the URL of a clicked breadcrumb,
    /// `on_entry` with the text of the activated entry.
    pub fn new(on_crumb: impl Fn(RepoUrl) + 'static, on_entry: impl Fn(String) + 'static) -> Rc<Self> {
        let crumbs = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .valign(gtk::Align::Center)
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .hexpand(true)
            .child(&crumbs)
            .build();
        // Clicks on the empty area right of the crumbs start editing.
        let filler = gtk::Box::builder().hexpand(true).build();
        let frame = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .css_classes(["gsvn-pathbar"])
            .tooltip_text("Click or press Ctrl+L to type a location")
            .build();
        frame.append(&scrolled);
        frame.append(&filler);

        let entry = gtk::Entry::builder()
            .hexpand(true)
            .input_purpose(gtk::InputPurpose::Url)
            .placeholder_text("URL or path from the repository root")
            .build();

        let widget = gtk::Stack::builder()
            .hexpand(true)
            .hhomogeneous(true)
            .vhomogeneous(true)
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(100)
            .build();
        widget.add_named(&frame, Some("crumbs"));
        widget.add_named(&entry, Some("entry"));

        // Keep the current (last) segment visible.
        scrolled
            .hadjustment()
            .connect_changed(|adj| adj.set_value(adj.upper() - adj.page_size()));

        let bar = Rc::new(Self {
            widget,
            crumbs,
            entry: entry.clone(),
            current: RefCell::default(),
            on_crumb: Box::new(on_crumb),
        });

        let click = gtk::GestureClick::new();
        {
            let bar = Rc::downgrade(&bar);
            click.connect_released(move |g, _, x, y| {
                let Some(bar) = bar.upgrade() else { return };
                // Only clicks not on a breadcrumb button.
                let target = g.widget().and_then(|w| w.pick(x, y, gtk::PickFlags::DEFAULT));
                if target.is_some_and(|t| t.ancestor(gtk::Button::static_type()).is_some()) {
                    return;
                }
                bar.start_editing();
            });
        }
        frame.add_controller(click);

        {
            let bar = Rc::downgrade(&bar);
            entry.connect_activate(move |e| {
                let Some(bar) = bar.upgrade() else { return };
                let text = e.text().to_string();
                bar.stop_editing();
                on_entry(text);
            });
        }
        let keys = gtk::EventControllerKey::new();
        {
            let bar = Rc::downgrade(&bar);
            keys.connect_key_pressed(move |_, key, _, _| {
                if key == gtk::gdk::Key::Escape
                    && let Some(bar) = bar.upgrade()
                {
                    bar.stop_editing();
                    return glib::Propagation::Stop;
                }
                glib::Propagation::Proceed
            });
        }
        entry.add_controller(keys);
        {
            // Leaving the entry reverts to breadcrumbs.
            let bar = Rc::downgrade(&bar);
            let focus = gtk::EventControllerFocus::new();
            focus.connect_leave(move |_| {
                if let Some(bar) = bar.upgrade()
                    && bar.is_editing()
                {
                    bar.stop_editing();
                }
            });
            entry.add_controller(focus);
        }
        bar
    }

    pub fn is_editing(&self) -> bool {
        self.widget.visible_child_name().as_deref() == Some("entry")
    }

    pub fn start_editing(&self) {
        if let Some((_, url)) = &*self.current.borrow() {
            self.entry.set_text(&decode(url.as_str()));
        }
        self.widget.set_visible_child_name("entry");
        self.entry.grab_focus();
        self.entry.select_region(0, -1);
    }

    pub fn stop_editing(&self) {
        if !self.is_editing() {
            return;
        }
        self.widget.set_visible_child_name("crumbs");
        if let Some(root) = self.widget.root() {
            // Give focus back to the window content.
            root.child_focus(gtk::DirectionType::TabForward);
        }
    }

    /// Show breadcrumbs for `url` below `root`.
    pub fn set_location(self: &Rc<Self>, root: &RepoUrl, url: &RepoUrl) {
        let same = self.current.borrow().as_ref().is_some_and(|(r, u)| r == root && u == url);
        if same {
            return;
        }
        *self.current.borrow_mut() = Some((root.clone(), url.clone()));
        while let Some(child) = self.crumbs.first_child() {
            self.crumbs.remove(&child);
        }
        let crumbs = nav::breadcrumbs(root, url);
        let last = crumbs.len() - 1;
        for (i, (label, target)) in crumbs.into_iter().enumerate() {
            if i > 0 {
                let sep = gtk::Label::builder().label("/").css_classes(["dim-label"]).build();
                self.crumbs.append(&sep);
            }
            let button = gtk::Button::builder()
                .css_classes(["flat", "gsvn-crumb"])
                .tooltip_text(decode(target.as_str()))
                .build();
            let text = gtk::Label::builder()
                .label(&label)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(32)
                .build();
            if i == 0 {
                let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                b.append(&gtk::Image::from_icon_name("folder-remote-symbolic"));
                b.append(&text);
                button.set_child(Some(&b));
            } else {
                button.set_child(Some(&text));
            }
            if i == last {
                button.add_css_class("gsvn-crumb-current");
            }
            let bar = Rc::downgrade(self);
            button.connect_clicked(move |_| {
                if let Some(bar) = bar.upgrade() {
                    (bar.on_crumb)(target.clone());
                }
            });
            self.crumbs.append(&button);
        }
    }
}
