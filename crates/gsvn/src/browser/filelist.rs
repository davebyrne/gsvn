//! The file list: a `ColumnView` over
//! `SortListModel(FilterListModel(ListStore))` (DESIGN.md §5.2).

use super::entry::{self, EntryData, EntryKind, EntryObject};
use crate::common;
use gtk::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::rc::Rc;

pub struct FileList {
    pub scrolled: gtk::ScrolledWindow,
    pub view: gtk::ColumnView,
    pub filter: gtk::StringFilter,
    pub filter_model: gtk::FilterListModel,
    pub sort_model: gtk::SortListModel,
    pub selection: gtk::SingleSelection,
}

/// Sorter from a comparison of entry data.
fn sorter(f: impl Fn(&EntryData, &EntryData) -> Ordering + 'static) -> gtk::CustomSorter {
    gtk::CustomSorter::new(move |a, b| entry::cmp_by(a, b, &f).into())
}

/// `Some` values first in ascending order; missing values last.
fn cmp_opt<T: Ord>(a: &Option<T>, b: &Option<T>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => a.cmp(b),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Context menu request: position in the sorted list, widget clicked,
/// coordinates in it.
pub type ContextHandler = Rc<dyn Fn(u32, &gtk::Widget, f64, f64)>;

impl FileList {
    pub fn new(on_context: ContextHandler) -> Self {
        let filter = gtk::StringFilter::builder()
            .expression(gtk::PropertyExpression::new(
                EntryObject::static_type(),
                None::<&gtk::Expression>,
                "name",
            ))
            .ignore_case(true)
            .match_mode(gtk::StringFilterMatchMode::Substring)
            .build();
        let filter_model = gtk::FilterListModel::builder()
            .filter(&filter)
            .incremental(false)
            .build();

        let view = gtk::ColumnView::builder()
            .show_column_separators(false)
            .show_row_separators(false)
            .reorderable(false)
            .tab_behavior(gtk::ListTabBehavior::Item)
            .css_classes(["gsvn-file-list"])
            .build();

        let name_sorter = sorter(entry::cmp_name);
        let name_col = column("Name", name_factory(on_context.clone()), &name_sorter);
        name_col.set_expand(true);
        name_col.set_resizable(true);
        view.append_column(&name_col);

        let tiebreak = |f: fn(&EntryData, &EntryData) -> Ordering| {
            sorter(move |a, b| f(a, b).then_with(|| entry::cmp_name(a, b)))
        };
        let rev_col = column(
            "Rev",
            label_factory(&on_context, 1.0, true, |d| Some(d.rev.to_string()), |_| None),
            &tiebreak(|a, b| a.rev.cmp(&b.rev)),
        );
        rev_col.set_fixed_width(80);
        view.append_column(&rev_col);

        let author_col = column(
            "Author",
            label_factory(
                &on_context,
                0.0,
                false,
                |d| Some(d.author.clone().unwrap_or_else(|| "—".into())),
                |_| None,
            ),
            &tiebreak(|a, b| cmp_opt(&a.author, &b.author)),
        );
        author_col.set_fixed_width(120);
        author_col.set_resizable(true);
        view.append_column(&author_col);

        let date_col = column(
            "Date",
            label_factory(
                &on_context,
                0.0,
                false,
                |d| Some(d.date.map(common::format_relative).unwrap_or_else(|| "—".into())),
                |d| d.date.map(common::format_datetime),
            ),
            &tiebreak(|a, b| cmp_opt(&a.date, &b.date)),
        );
        date_col.set_fixed_width(140);
        date_col.set_resizable(true);
        view.append_column(&date_col);

        let size_col = column(
            "Size",
            label_factory(
                &on_context,
                1.0,
                true,
                |d| Some(d.size.map(common::format_size).unwrap_or_else(|| "—".into())),
                |d| d.size.map(|s| format!("{s} bytes")),
            ),
            &tiebreak(|a, b| cmp_opt(&a.size, &b.size)),
        );
        size_col.set_fixed_width(96);
        view.append_column(&size_col);

        // Directories always first, whatever the column sort.
        let multi = gtk::MultiSorter::new();
        multi.append(sorter(entry::cmp_dirs_first));
        if let Some(s) = view.sorter() {
            multi.append(s);
        }
        let sort_model = gtk::SortListModel::builder()
            .model(&filter_model)
            .sorter(&multi)
            .incremental(true)
            .build();
        let selection = gtk::SingleSelection::builder()
            .model(&sort_model)
            .autoselect(false)
            .can_unselect(true)
            .build();
        view.set_model(Some(&selection));
        view.sort_by_column(Some(&name_col), gtk::SortType::Ascending);
        // Numeric columns: headers right-aligned like their values.
        align_titles_end(&view, &["Rev", "Size"]);

        let scrolled = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hexpand(true)
            .build();

        Self {
            scrolled,
            view,
            filter,
            filter_model,
            sort_model,
            selection,
        }
    }

    pub fn set_store(&self, store: &gio::ListStore) {
        self.filter_model.set_model(Some(store));
    }

    pub fn item(&self, pos: u32) -> Option<EntryObject> {
        self.sort_model.item(pos).and_downcast()
    }

    pub fn selected(&self) -> Option<EntryObject> {
        self.selection.selected_item().and_downcast()
    }

    /// Select and reveal the entry called `name`. Returns false if it is
    /// not (yet) in the list.
    pub fn select_name(&self, name: &str) -> bool {
        let n = self.sort_model.n_items();
        let pos = (0..n).find(|&i| self.item(i).is_some_and(|e| e.name() == name));
        match pos {
            Some(pos) => {
                self.view.scroll_to(
                    pos,
                    None::<&gtk::ColumnViewColumn>,
                    gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
                    None,
                );
                true
            }
            None => false,
        }
    }
}

/// Right-align the header titles of the named columns. GTK has no API
/// for this: each `GtkColumnViewTitle` holds a box with the label and the
/// sort arrow; the box is moved to the end, the arrow before the label.
fn align_titles_end(view: &gtk::ColumnView, titles: &[&str]) {
    let mut stack = vec![view.clone().upcast::<gtk::Widget>()];
    while let Some(w) = stack.pop() {
        if w.type_().name() == "GtkColumnViewTitle" {
            let Some(content) = w.first_child() else { continue };
            let label = content.first_child().and_downcast::<gtk::Label>();
            if let Some(label) = label.filter(|l| titles.contains(&l.label().as_str()))
                && let Some(content) = content.downcast_ref::<gtk::Box>()
            {
                content.set_halign(gtk::Align::End);
                // Sort arrow before the label, so the text ends where the values do.
                if let Some(arrow) = label.next_sibling() {
                    content.reorder_child_after(&label, Some(&arrow));
                }
            }
            continue;
        }
        let mut child = w.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            // The rows are not needed (and may be many).
            if c.css_name() != "listview" {
                stack.push(c);
            }
        }
    }
}

fn column(title: &str, factory: gtk::SignalListItemFactory, sorter: &gtk::CustomSorter) -> gtk::ColumnViewColumn {
    gtk::ColumnViewColumn::builder()
        .title(title)
        .factory(&factory)
        .sorter(sorter)
        .build()
}

fn list_item(obj: &glib::Object) -> &gtk::ListItem {
    obj.downcast_ref::<gtk::ListItem>().expect("ListItem")
}

/// Right-click on a cell requests the context menu for its row.
fn add_context_gesture(item: &gtk::ListItem, widget: &impl IsA<gtk::Widget>, on_context: &ContextHandler) {
    let gesture = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
    let item = item.downgrade();
    let on_context = on_context.clone();
    gesture.connect_pressed(move |g, _, x, y| {
        let Some(item) = item.upgrade() else { return };
        let pos = item.position();
        if pos == gtk::INVALID_LIST_POSITION {
            return;
        }
        g.set_state(gtk::EventSequenceState::Claimed);
        if let Some(w) = g.widget() {
            on_context(pos, &w, x, y);
        }
    });
    widget.add_controller(gesture);
}

fn name_factory(on_context: ContextHandler) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, obj| {
        let item = list_item(obj);
        let icon = gtk::Image::new();
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .hexpand(true)
            .build();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&icon);
        row.append(&label);
        add_context_gesture(item, &row, &on_context);
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, obj| {
        let item = list_item(obj);
        let Some(entry) = item.item().and_downcast::<EntryObject>() else { return };
        let Some(row) = item.child() else { return };
        let icon = row.first_child().and_downcast::<gtk::Image>().expect("icon");
        let label = row.last_child().and_downcast::<gtk::Label>().expect("label");
        icon.set_from_gicon(&icon_for(&entry));
        label.set_label(entry.name());
        label.set_tooltip_text(Some(entry.name()));
    });
    factory
}

/// A right/left aligned label column. `text` gives the label, `tooltip`
/// an optional tooltip.
fn label_factory(
    on_context: &ContextHandler,
    xalign: f32,
    numeric: bool,
    text: fn(&EntryData) -> Option<String>,
    tooltip: fn(&EntryData) -> Option<String>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let on_context = on_context.clone();
    factory.connect_setup(move |_, obj| {
        let item = list_item(obj);
        let label = gtk::Label::builder()
            .xalign(xalign)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(if numeric { vec!["numeric", "dim-label"] } else { vec!["dim-label"] })
            .build();
        add_context_gesture(item, &label, &on_context);
        item.set_child(Some(&label));
    });
    factory.connect_bind(move |_, obj| {
        let item = list_item(obj);
        let Some(entry) = item.item().and_downcast::<EntryObject>() else { return };
        let Some(label) = item.child().and_downcast::<gtk::Label>() else { return };
        let d = entry.data();
        label.set_label(&text(d).unwrap_or_default());
        label.set_tooltip_text(tooltip(d).as_deref());
    });
    factory
}

thread_local! {
    static ICONS: RefCell<HashMap<String, gio::Icon>> = RefCell::default();
}

/// Themed icon for an entry: folder, or a content-type icon guessed from
/// the file extension (cached per extension).
pub fn icon_for(entry: &EntryObject) -> gio::Icon {
    let generic = || gio::ThemedIcon::new("text-x-generic-symbolic").upcast::<gio::Icon>();
    match entry.kind() {
        EntryKind::Dir => gio::ThemedIcon::new("folder-symbolic").upcast(),
        EntryKind::Loading => gio::ThemedIcon::new("content-loading-symbolic").upcast(),
        EntryKind::Error => gio::ThemedIcon::new("dialog-warning-symbolic").upcast(),
        EntryKind::File => {
            let ext = match entry.name().rsplit_once('.') {
                Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => ext.to_ascii_lowercase(),
                _ => return generic(),
            };
            ICONS.with_borrow_mut(|icons| {
                icons
                    .entry(ext)
                    .or_insert_with_key(|ext| {
                        let (ctype, _) = gio::content_type_guess(Some(&format!("x.{ext}")), None::<&[u8]>);
                        if gio::content_type_is_unknown(&ctype) {
                            generic()
                        } else {
                            gio::content_type_get_symbolic_icon(&ctype)
                        }
                    })
                    .clone()
            })
        }
    }
}
