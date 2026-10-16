//! Sidebar folder tree: a `ListView` over a lazy `TreeListModel`
//! (DESIGN.md §5.1). Children come from the shared [`DirCache`] and are
//! only listed when a row is expanded.

use super::dir::{DirCache, LoadState};
use super::entry::{self, EntryKind, EntryObject};
use super::nav;
use gtk::{gio, glib, prelude::*};
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use svn_backend::Priority;

/// Indentation stops growing after this many levels; deeper rows show a
/// depth badge instead.
pub const INDENT_CAP: u32 = 12;
/// Pixels per indentation level.
const INDENT_PX: i32 = 12;

/// Context menu request: entry, widget clicked, position in it.
pub type ContextHandler = Rc<dyn Fn(&EntryObject, &gtk::Widget, f64, f64)>;

pub struct Tree {
    pub scrolled: gtk::ScrolledWindow,
    pub view: gtk::ListView,
    pub selection: gtk::SingleSelection,
    cache: Rc<RefCell<Weak<DirCache>>>,
}

/// Per-row state kept on the list item while bound.
struct Bound {
    row: gtk::TreeListRow,
    expanded_handler: glib::SignalHandlerId,
    childless: glib::Binding,
}

const BOUND_KEY: &str = "gsvn-tree-bound";

impl Tree {
    pub fn new(on_context: ContextHandler) -> Self {
        let cache: Rc<RefCell<Weak<DirCache>>> = Rc::default();
        let factory = gtk::SignalListItemFactory::new();
        factory.connect_setup(move |_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
            let badge = gtk::Label::builder()
                .css_classes(["gsvn-depth-badge", "numeric"])
                .valign(gtk::Align::Center)
                .visible(false)
                .build();
            let icon = gtk::Image::new();
            let label = gtk::Label::builder()
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .hexpand(true)
                .build();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            content.append(&badge);
            content.append(&icon);
            content.append(&label);
            let expander = gtk::TreeExpander::builder()
                .child(&content)
                .indent_for_depth(false)
                .indent_for_icon(true)
                .build();
            let gesture = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
            {
                let on_context = on_context.clone();
                let item = item.downgrade();
                gesture.connect_pressed(move |g, _, x, y| {
                    let Some(item) = item.upgrade() else { return };
                    let Some(entry) = tree_entry(&item) else { return };
                    if entry.is_placeholder() {
                        return;
                    }
                    g.set_state(gtk::EventSequenceState::Claimed);
                    if let Some(w) = g.widget() {
                        on_context(&entry, &w, x, y);
                    }
                });
            }
            expander.add_controller(gesture);
            item.set_child(Some(&expander));
        });

        {
            let cache = cache.clone();
            factory.connect_bind(move |_, obj| {
                let item = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
                let Some(row) = item.item().and_downcast::<gtk::TreeListRow>() else { return };
                let Some(entry) = row.item().and_downcast::<EntryObject>() else { return };
                let Some(expander) = item.child().and_downcast::<gtk::TreeExpander>() else { return };
                let content = expander.child().expect("content");
                let badge = content.first_child().and_downcast::<gtk::Label>().expect("badge");
                let icon = badge.next_sibling().and_downcast::<gtk::Image>().expect("icon");
                let label = content.last_child().and_downcast::<gtk::Label>().expect("label");

                expander.set_list_row(Some(&row));
                let depth = row.depth();
                let (indent, deep) = nav::tree_indent(depth, INDENT_CAP);
                expander.set_margin_start(indent as i32 * INDENT_PX);
                badge.set_visible(deep);
                if deep {
                    badge.set_label(&depth.to_string());
                    badge.set_tooltip_text(Some(&format!("Depth {depth}")));
                }
                icon.set_from_gicon(&super::filelist::icon_for(&entry));
                label.set_label(entry.name());
                let placeholder = entry.is_placeholder();
                if placeholder {
                    label.add_css_class("dim-label");
                } else {
                    label.remove_css_class("dim-label");
                }
                label.set_tooltip_text(match entry.kind() {
                    EntryKind::Error => Some(entry.name()),
                    _ if !placeholder => Some(entry.name()),
                    _ => None,
                });
                item.set_selectable(!placeholder);
                item.set_activatable(!placeholder);

                let childless = entry
                    .bind_property("childless", &expander, "hide-expander")
                    .sync_create()
                    .transform_to(move |_, v: bool| Some(v || placeholder))
                    .build();
                let weak_cache = cache.clone();
                let expanded_handler = row.connect_expanded_notify(move |row| {
                    on_expanded(&weak_cache, row, false);
                });
                if row.is_expanded() {
                    on_expanded(&cache, &row, true);
                }
                // SAFETY: only ever stored/stolen as `Bound` under this key.
                unsafe {
                    item.set_data(
                        BOUND_KEY,
                        Bound {
                            row,
                            expanded_handler,
                            childless,
                        },
                    );
                }
            });
        }
        factory.connect_unbind(|_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().expect("ListItem");
            // SAFETY: see bind.
            if let Some(bound) = unsafe { item.steal_data::<Bound>(BOUND_KEY) } {
                bound.row.disconnect(bound.expanded_handler);
                bound.childless.unbind();
            }
        });

        let selection = gtk::SingleSelection::builder()
            .autoselect(false)
            .can_unselect(true)
            .build();
        let view = gtk::ListView::builder()
            .model(&selection)
            .factory(&factory)
            .css_classes(["navigation-sidebar", "gsvn-tree"])
            .tab_behavior(gtk::ListTabBehavior::Item)
            .build();
        let scrolled = gtk::ScrolledWindow::builder()
            .child(&view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        Self {
            scrolled,
            view,
            selection,
            cache,
        }
    }

    /// Show a fresh tree for `cache` (new repository or revision).
    pub fn set_cache(&self, cache: &Rc<DirCache>) {
        *self.cache.borrow_mut() = Rc::downgrade(cache);
        let root_store = gio::ListStore::new::<EntryObject>();
        let root = EntryObject::root(&cache.root);
        cache.get(&cache.root).owner.replace(root.downgrade());
        root_store.append(&root);
        let weak = Rc::downgrade(cache);
        let model = gtk::TreeListModel::new(root_store, false, false, move |obj| {
            let entry = obj.downcast_ref::<EntryObject>()?;
            if !entry.is_dir() || entry.is_childless() {
                return None;
            }
            let cache = weak.upgrade()?;
            let dir = cache.get(&entry.url());
            dir.owner.replace(entry.downgrade());
            if dir.is_done() && dir.dirs.n_items() == 0 {
                entry.set_childless(true);
                return None;
            }
            let sorter = gtk::CustomSorter::new(|a, b| {
                entry::cmp_by(a, b, |a, b| {
                    entry::cmp_dirs_first(a, b).then_with(|| entry::cmp_name(a, b))
                })
                .into()
            });
            Some(gtk::SortListModel::new(Some(dir.dirs.clone()), Some(sorter)).upcast())
        });
        self.selection.set_model(Some(&model));
    }

    pub fn model(&self) -> Option<gtk::TreeListModel> {
        self.selection.model().and_downcast()
    }

    pub fn root_row(&self) -> Option<gtk::TreeListRow> {
        self.model()?.row(0)
    }

    pub fn selected_entry(&self) -> Option<EntryObject> {
        self.selection
            .selected_item()
            .and_downcast::<gtk::TreeListRow>()?
            .item()
            .and_downcast()
    }
}

fn tree_entry(item: &gtk::ListItem) -> Option<EntryObject> {
    item.item()
        .and_downcast::<gtk::TreeListRow>()?
        .item()
        .and_downcast()
}

/// A row was expanded or collapsed: list it when expanded. On rebinding
/// an expanded row (`rebind`), only start listings never started, so a
/// cancelled listing is not restarted by scrolling.
fn on_expanded(cache: &Rc<RefCell<Weak<DirCache>>>, row: &gtk::TreeListRow, rebind: bool) {
    let Some(cache) = cache.borrow().upgrade() else { return };
    let Some(entry) = row.item().and_downcast::<EntryObject>() else { return };
    if !entry.is_dir() {
        return;
    }
    let dir = cache.get(&entry.url());
    dir.expanded.set(row.is_expanded());
    if row.is_expanded() && (!rebind || dir.state() == LoadState::Idle) {
        dir.ensure(Priority::Normal);
    }
}

/// Child row of `row` for the subdirectory called `name`.
pub fn child_row(row: &gtk::TreeListRow, name: &str) -> Option<gtk::TreeListRow> {
    let children = row.children()?;
    (0..children.n_items()).find_map(|i| {
        let e = children.item(i).and_downcast::<EntryObject>()?;
        (e.is_dir() && e.name() == name).then(|| row.child_row(i)).flatten()
    })
}
