//! Folder tree with lazily loaded children that follows the active tab.

use crate::config;
use crate::sidebar::OpenFn;
use crate::util;
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

const TREE_ATTRS: &str = "standard::name,standard::display-name,standard::type,standard::is-hidden,standard::is-backup";

pub struct FolderTree {
    pub widget: gtk::Box,
    view: gtk::ListView,
    model: gtk::TreeListModel,
    selection: gtk::SingleSelection,
    filter: gtk::CustomFilter,
    on_open: OpenFn,
    /// Bumped for every reveal so an older, still-running one gives up.
    reveal_gen: Cell<u64>,
}

impl FolderTree {
    pub fn new(on_open: OpenFn) -> Rc<Self> {
        let roots = gio::ListStore::new::<gio::FileInfo>();
        for (path, name, icon) in [
            (glib::home_dir(), "Home", "user-home-symbolic"),
            ("/".into(), "File System", "drive-harddisk-symbolic"),
        ] {
            let info = util::info_from_path(&path, None);
            info.set_display_name(name);
            info.set_icon(&gio::ThemedIcon::new(icon));
            roots.append(&info);
        }

        // Shared by every child model so toggling hidden files updates them all.
        let filter = gtk::CustomFilter::new(|obj| {
            let info = obj.downcast_ref::<gio::FileInfo>().unwrap();
            util::is_dir(info) && (config::with(|c| c.show_hidden) || !util::is_hidden(info))
        });
        let sorter = gtk::CustomSorter::new(|a, b| {
            let name = |o: &glib::Object| o.downcast_ref::<gio::FileInfo>().unwrap().display_name();
            util::natural_cmp(&name(a), &name(b)).into()
        });
        let (f, s) = (filter.clone(), sorter.clone());
        let model = gtk::TreeListModel::new(roots, false, false, move |obj| {
            let info = obj.downcast_ref::<gio::FileInfo>()?;
            if !util::is_dir(info) {
                return None;
            }
            let list = gtk::DirectoryList::new(Some(TREE_ATTRS), Some(&util::file_of(info)));
            let filtered = gtk::FilterListModel::new(Some(list), Some(f.clone()));
            Some(gtk::SortListModel::new(Some(filtered), Some(s.clone())).upcast())
        });
        let selection = gtk::SingleSelection::builder().model(&model).autoselect(false).can_unselect(true).build();

        let factory = gtk::SignalListItemFactory::new();
        let view = gtk::ListView::new(Some(selection.clone()), Some(factory.clone()));
        view.add_css_class("navigation-sidebar");
        let scrolled = gtk::ScrolledWindow::builder()
            .child(&view)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&gtk::Label::builder().label("Folders").xalign(0.0).css_classes(["vq-sidebar-header"]).margin_start(10).build());
        widget.append(&scrolled);

        let tree = Rc::new(FolderTree {
            widget,
            view,
            model,
            selection,
            filter,
            on_open,
            reveal_gen: Cell::new(0),
        });

        let w = Rc::downgrade(&tree);
        factory.connect_setup(move |_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            content.append(&gtk::Image::new());
            content.append(&gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build());
            let expander = gtk::TreeExpander::builder().child(&content).indent_for_icon(true).build();
            item.set_child(Some(&expander));

            // Clicks on the label navigate; the expander arrow only expands.
            let click = gtk::GestureClick::builder().button(0).build();
            let (w, it) = (w.clone(), item.downgrade());
            click.connect_released(move |g, _, _, _| {
                let (Some(tree), Some(item)) = (w.upgrade(), it.upgrade()) else { return };
                let Some(info) = item.item().and_downcast::<gtk::TreeListRow>().and_then(|r| r.item()).and_downcast::<gio::FileInfo>() else { return };
                let new_tab = match g.current_button() {
                    gdk::BUTTON_PRIMARY => false,
                    gdk::BUTTON_MIDDLE => true,
                    _ => return,
                };
                (tree.on_open)(util::file_of(&info), new_tab);
            });
            content.add_controller(click);
        });
        factory.connect_bind(|_, obj| {
            let item = obj.downcast_ref::<gtk::ListItem>().unwrap();
            let row = item.item().and_downcast::<gtk::TreeListRow>().unwrap();
            let info = row.item().and_downcast::<gio::FileInfo>().unwrap();
            let expander = item.child().and_downcast::<gtk::TreeExpander>().unwrap();
            expander.set_list_row(Some(&row));
            let content = expander.child().unwrap();
            let image = content.first_child().and_downcast::<gtk::Image>().unwrap();
            let label = image.next_sibling().and_downcast::<gtk::Label>().unwrap();
            label.set_text(&info.display_name());
            match row.depth() {
                0 => image.set_from_gicon(&info.icon().unwrap()),
                _ => image.set_icon_name(Some("folder-symbolic")),
            }
        });

        let w = Rc::downgrade(&tree);
        tree.view.connect_activate(move |_, pos| {
            let Some(tree) = w.upgrade() else { return };
            if let Some(info) = tree.model.row(pos).and_then(|r| r.item()).and_downcast::<gio::FileInfo>() {
                (tree.on_open)(util::file_of(&info), false);
            }
        });
        tree
    }

    pub fn refilter(&self) {
        self.filter.changed(gtk::FilterChange::Different);
    }

    /// Expand the tree down to `location` and select it. Child folders load
    /// asynchronously, so each level waits for its entry to appear.
    pub fn reveal(self: &Rc<Self>, location: gio::File) {
        let generation = self.reveal_gen.get() + 1;
        self.reveal_gen.set(generation);
        // Inside an archive, show the folder that holds it.
        let Some(target) = location.path().or_else(|| crate::archive::host_dir(&location)) else {
            self.selection.unselect_all();
            return;
        };
        let home = glib::home_dir();
        let (root_index, root_path) = if target.starts_with(&home) { (0, home) } else { (1, "/".into()) };
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(tree) = w.upgrade() else { return };
            let Some(mut row) = tree.model.child_row(root_index) else { return };
            let rel = target.strip_prefix(&root_path).unwrap().to_path_buf();
            let mut current = root_path;
            for comp in rel.components() {
                current.push(comp);
                row.set_expanded(true);
                let mut found = None;
                for _ in 0..100 {
                    if tree.reveal_gen.get() != generation {
                        return;
                    }
                    if let Some(children) = row.children() {
                        found = (0..children.n_items()).find(|&i| {
                            children
                                .item(i)
                                .and_downcast::<gio::FileInfo>()
                                .is_some_and(|info| util::file_of(&info).path().as_deref() == Some(current.as_path()))
                        });
                        if found.is_some() {
                            break;
                        }
                    }
                    glib::timeout_future(Duration::from_millis(20)).await;
                }
                // Hidden or unreadable folder: stop at the deepest visible ancestor.
                let Some(next) = found.and_then(|i| row.child_row(i)) else { break };
                row = next;
            }
            if tree.reveal_gen.get() == generation {
                tree.view.scroll_to(row.position(), gtk::ListScrollFlags::SELECT, None);
            }
        });
    }
}
