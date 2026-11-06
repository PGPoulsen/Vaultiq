//! One explorer tab: a folder (or search result) listing shown in one of
//! three views, with its own back/forward history.

use crate::config::{self, ViewMode};
use crate::util::{self, History};
use crate::views::{self, CellKind};
use crate::search;
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub enum TabEvent {
    LocationChanged,
    SelectionChanged,
    ItemsChanged,
    /// Double-click / Enter on items.
    Open(Vec<gio::FileInfo>),
    OpenInNewTab(gio::File),
    /// Right click; coordinates are relative to `widget`.
    ContextMenu { widget: gtk::Widget, x: f64, y: f64 },
    Drop { files: Vec<gio::File>, dest: gio::File, copy: bool },
}

/// Receives every event a tab emits.
pub type EventFn = Rc<dyn Fn(&Tab, TabEvent)>;

pub struct SearchState {
    pub query: String,
    pub running: bool,
    cancel: Arc<AtomicBool>,
}

struct State {
    location: gio::File,
    history: History<gio::File>,
    view: ViewMode,
    search: Option<SearchState>,
    /// File name to select once it shows up in the listing.
    pending_select: Option<String>,
    /// Names of the selected files. GIO change events replace items in the
    /// model, which drops their selection; this restores it.
    kept_selection: HashSet<String>,
}

pub struct Tab {
    pub widget: gtk::Box,
    dir_list: gtk::DirectoryList,
    filter_model: gtk::FilterListModel,
    hidden_filter: gtk::CustomFilter,
    pub selection: gtk::MultiSelection,
    stack: gtk::Stack,
    empty_label: gtk::Label,
    error_page: adw::StatusPage,
    column_view: gtk::ColumnView,
    list_view: gtk::ListView,
    grid_view: gtk::GridView,
    sort_columns: Vec<(&'static str, gtk::ColumnViewColumn)>,
    location_column: gtk::ColumnViewColumn,
    state: RefCell<State>,
    events: EventFn,
}

impl Tab {
    pub fn new(location: gio::File, events: EventFn) -> Rc<Tab> {
        let tab = Rc::new_cyclic(|weak: &Weak<Tab>| Self::build(location.clone(), weak.clone(), events));
        tab.connect_signals();
        tab.set_view(config::with(|c| c.default_view));
        tab.load(location, None);
        tab
    }

    fn build(location: gio::File, weak: Weak<Tab>, events: EventFn) -> Tab {
        let dir_list = gtk::DirectoryList::new(Some(util::ATTRS), None::<&gio::File>);
        dir_list.set_monitored(true);

        let hidden_filter = gtk::CustomFilter::new(|obj| {
            config::with(|c| c.show_hidden) || !util::is_hidden(obj.downcast_ref::<gio::FileInfo>().unwrap())
        });
        let filter_model = gtk::FilterListModel::new(Some(dir_list.clone()), Some(hidden_filter.clone()));

        // Details view; its header sorter drives the shared model's order.
        let column_view = gtk::ColumnView::builder().reorderable(false).show_column_separators(false).build();
        column_view.add_css_class("data-table");
        let mut sort_columns = Vec::new();
        // `width` 0 means "fill the remaining space" until the user sizes it.
        let mk = |key: &'static str, title: &str, kind: CellKind, sorter: Option<gtk::Sorter>, width: i32| {
            let col = gtk::ColumnViewColumn::new(Some(title), Some(views::factory(kind, weak.clone())));
            col.set_resizable(true);
            col.set_sorter(sorter.as_ref());
            let saved = config::with(|c| c.column_widths.get(key).copied());
            match saved.or((width > 0).then_some(width)) {
                Some(w) => col.set_fixed_width(w),
                None => col.set_expand(true),
            }
            // Dragging a header edge sets a fixed width. A filling column
            // would grow straight back, so it stops filling once sized;
            // double-clicking the edge (width -1) restores the default.
            col.connect_fixed_width_notify(move |col| {
                let w = col.fixed_width();
                if width == 0 {
                    col.set_expand(w < 0);
                }
                if w < 0 && width > 0 {
                    // Back to the default width. The nested notify saves it as
                    // a user width; forget it again afterwards.
                    col.set_fixed_width(width);
                    config::update_deferred(|c| {
                        c.column_widths.remove(key);
                    });
                    return;
                }
                config::update_deferred(|c| {
                    if w > 0 {
                        c.column_widths.insert(key.to_string(), w);
                    } else {
                        c.column_widths.remove(key);
                    }
                });
            });
            col
        };
        let info_cmp = |f: fn(&gio::FileInfo, &gio::FileInfo) -> std::cmp::Ordering| {
            gtk::CustomSorter::new(move |a, b| {
                f(a.downcast_ref().unwrap(), b.downcast_ref().unwrap()).into()
            })
            .upcast::<gtk::Sorter>()
        };
        let name_col = mk(
            "name",
            "Name",
            CellKind::Name,
            Some(info_cmp(|a, b| util::natural_cmp(&a.display_name(), &b.display_name()))),
            0,
        );
        let date_col = mk(
            "date",
            "Date modified",
            CellKind::Date,
            Some(info_cmp(|a, b| {
                let t = |i: &gio::FileInfo| i.modification_date_time().map(|d| d.to_unix()).unwrap_or(0);
                t(a).cmp(&t(b))
            })),
            150,
        );
        let type_col = mk(
            "type",
            "Type",
            CellKind::Type,
            Some(info_cmp(|a, b| util::content_type(a).cmp(&util::content_type(b)))),
            170,
        );
        let size_col = mk("size", "Size", CellKind::Size, Some(info_cmp(|a, b| a.size().cmp(&b.size()))), 90);
        let location_column = mk("location", "Folder", CellKind::Location, None, 220);
        location_column.set_visible(false);
        for col in [&name_col, &location_column, &date_col, &type_col, &size_col] {
            column_view.append_column(col);
        }
        sort_columns.push(("name", name_col.clone()));
        sort_columns.push(("date", date_col));
        sort_columns.push(("type", type_col));
        sort_columns.push(("size", size_col));

        let folders_first = gtk::CustomSorter::new(|a, b| {
            let d = |o: &glib::Object| !util::is_dir(o.downcast_ref().unwrap());
            d(a).cmp(&d(b)).into()
        });
        let sorter = gtk::MultiSorter::new();
        sorter.append(folders_first);
        if let Some(s) = column_view.sorter() {
            sorter.append(s);
        }
        let sort_model = gtk::SortListModel::new(Some(filter_model.clone()), Some(sorter));
        let selection = gtk::MultiSelection::new(Some(sort_model));
        column_view.set_model(Some(&selection));
        column_view.sort_by_column(Some(&name_col), gtk::SortType::Ascending);

        let list_view = gtk::ListView::new(Some(selection.clone()), Some(views::factory(CellKind::Row, weak.clone())));
        let grid_view = gtk::GridView::new(Some(selection.clone()), Some(views::factory(CellKind::Grid, weak.clone())));
        grid_view.set_max_columns(64);

        let stack = gtk::Stack::new();
        let scrolled = |w: &gtk::Widget| gtk::ScrolledWindow::builder().child(w).vexpand(true).hexpand(true).build();
        stack.add_named(&scrolled(column_view.upcast_ref()), Some("details"));
        stack.add_named(&scrolled(list_view.upcast_ref()), Some("list"));
        stack.add_named(&scrolled(grid_view.upcast_ref()), Some("icons"));
        let error_page = adw::StatusPage::builder().icon_name("dialog-error-symbolic").build();
        stack.add_named(&error_page, Some("error"));

        let empty_label = gtk::Label::builder()
            .label("This folder is empty.")
            .css_classes(["dim-label"])
            .valign(gtk::Align::Start)
            .margin_top(48)
            .can_target(false)
            .visible(false)
            .build();
        let overlay = gtk::Overlay::builder().child(&stack).build();
        overlay.add_overlay(&empty_label);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.append(&overlay);

        Tab {
            widget,
            dir_list,
            filter_model,
            hidden_filter,
            selection,
            stack,
            empty_label,
            error_page,
            column_view,
            list_view,
            grid_view,
            sort_columns,
            location_column,
            state: RefCell::new(State {
                location,
                history: History::default(),
                view: ViewMode::Details,
                search: None,
                pending_select: None,
                kept_selection: HashSet::new(),
            }),
            events,
        }
    }

    fn weak(self: &Rc<Self>) -> Weak<Self> {
        Rc::downgrade(self)
    }

    fn emit(&self, ev: TabEvent) {
        (self.events)(self, ev)
    }

    fn connect_signals(self: &Rc<Self>) {
        let w = self.weak();
        let activate = move |pos: u32| {
            if let Some(t) = w.upgrade() {
                t.activate(pos)
            }
        };
        let a = activate.clone();
        self.column_view.connect_activate(move |_, pos| a(pos));
        let a = activate.clone();
        self.list_view.connect_activate(move |_, pos| a(pos));
        self.grid_view.connect_activate(move |_, pos| activate(pos));

        let w = self.weak();
        self.selection.connect_selection_changed(move |_, _, _| {
            if let Some(t) = w.upgrade() {
                let names = t.selected().iter().map(|i| i.name().to_string_lossy().into_owned()).collect();
                t.state.borrow_mut().kept_selection = names;
                t.emit(TabEvent::SelectionChanged);
            }
        });
        let w = self.weak();
        self.selection.connect_items_changed(move |sel, pos, _, added| {
            if let Some(t) = w.upgrade() {
                let kept = t.state.borrow().kept_selection.clone();
                if !kept.is_empty() {
                    for i in pos..pos + added {
                        let name = sel.item(i).and_downcast::<gio::FileInfo>().map(|f| f.name());
                        if name.is_some_and(|n| kept.contains(n.to_string_lossy().as_ref())) && !sel.is_selected(i) {
                            sel.select_item(i, false);
                        }
                    }
                }
                t.apply_pending_select();
                t.update_empty();
                t.emit(TabEvent::ItemsChanged);
            }
        });
        let w = self.weak();
        self.dir_list.connect_loading_notify(move |_| {
            if let Some(t) = w.upgrade() {
                t.update_empty();
                t.emit(TabEvent::ItemsChanged);
            }
        });
        let w = self.weak();
        self.dir_list.connect_error_notify(move |dl| {
            let Some(t) = w.upgrade() else { return };
            if let Some(err) = dl.error() {
                t.error_page.set_title("Can't open this folder");
                t.error_page.set_description(Some(&glib::markup_escape_text(err.message())));
                t.stack.set_visible_child_name("error");
            } else {
                t.stack.set_visible_child_name(t.view().as_str());
            }
        });

        // Clicks on empty space: left clears the selection, right opens the
        // background menu. Item cells claim their own clicks first.
        let overlay = self.widget.first_child().unwrap();
        let click = gtk::GestureClick::builder().button(0).build();
        let w = self.weak();
        click.connect_pressed(move |g, _, x, y| {
            let Some(t) = w.upgrade() else { return };
            let widget = g.widget().unwrap();
            let on_item = widget
                .pick(x, y, gtk::PickFlags::DEFAULT)
                .is_some_and(|p| {
                    std::iter::successors(Some(p), |w| w.parent())
                        .take_while(|w| w != &widget)
                        .any(|w| w.has_css_class(views::CELL_CLASS) || w.has_css_class("vq-row"))
                });
            match g.current_button() {
                gdk::BUTTON_PRIMARY if !on_item => {
                    // Only on true blank space (below rows), not headers/scrollbars.
                    if widget.pick(x, y, gtk::PickFlags::DEFAULT).is_some_and(|p| {
                        p.is::<gtk::ColumnView>() || p.is::<gtk::ListView>() || p.is::<gtk::GridView>()
                    }) {
                        t.selection.unselect_all();
                    }
                }
                gdk::BUTTON_SECONDARY => {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    t.selection.unselect_all();
                    t.emit(TabEvent::ContextMenu { widget, x, y });
                }
                _ => {}
            }
        });
        overlay.add_controller(click);

        let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY | gdk::DragAction::MOVE);
        let w = self.weak();
        drop.connect_drop(move |target, value, _, _| {
            let Some(t) = w.upgrade() else { return false };
            if t.is_searching() {
                return false;
            }
            let Ok(list) = value.get::<gdk::FileList>() else { return false };
            t.drop_files(list.files(), t.location(), target.current_event_state());
            true
        });
        overlay.add_controller(drop);

        // File shortcuts only apply while focus is inside the file view, so
        // the same keys keep working normally in text entries.
        let shortcuts = gtk::ShortcutController::new();
        for (accel, action) in [
            ("Delete", "win.trash"),
            ("<Shift>Delete", "win.delete"),
            ("F2", "win.rename"),
            ("<Control>c", "win.copy"),
            ("<Control>x", "win.cut"),
            ("<Control>v", "win.paste"),
            ("BackSpace", "win.back"),
            ("<Alt>Return", "win.properties"),
            ("<Control><Shift>c", "win.copy-path"),
            ("<Control><Shift>n", "win.new-folder"),
            ("<Shift>F10", "win.context-menu"),
            ("Menu", "win.context-menu"),
        ] {
            shortcuts.add_shortcut(gtk::Shortcut::new(
                gtk::ShortcutTrigger::parse_string(accel),
                Some(gtk::NamedAction::new(action)),
            ));
        }
        self.widget.add_controller(shortcuts);
    }

    // ---- navigation -------------------------------------------------------

    pub fn location(&self) -> gio::File {
        self.state.borrow().location.clone()
    }

    pub fn navigate(&self, file: gio::File) {
        {
            let mut st = self.state.borrow_mut();
            if st.search.is_none() && file.equal(&st.location) {
                return;
            }
            let old = st.location.clone();
            st.history.visit(old);
        }
        self.load(file, None);
    }

    pub fn go_back(&self) {
        let prev = {
            let mut st = self.state.borrow_mut();
            let cur = st.location.clone();
            st.history.back(cur)
        };
        if let Some(prev) = prev {
            let came_from = self.location().basename();
            self.load(prev, came_from.map(|b| b.to_string_lossy().into_owned()));
        }
    }

    pub fn go_forward(&self) {
        let next = {
            let mut st = self.state.borrow_mut();
            let cur = st.location.clone();
            st.history.forward(cur)
        };
        if let Some(next) = next {
            self.load(next, None);
        }
    }

    pub fn go_up(&self) {
        let cur = self.location();
        if let Some(parent) = cur.parent() {
            self.navigate(parent);
            self.select_name(cur.basename().map(|b| b.to_string_lossy().into_owned()));
        }
    }

    pub fn can_back(&self) -> bool {
        self.state.borrow().history.can_back()
    }

    pub fn can_forward(&self) -> bool {
        self.state.borrow().history.can_forward()
    }

    pub fn refresh(self: &Rc<Self>) {
        if self.is_searching() {
            let q = self.state.borrow().search.as_ref().map(|s| s.query.clone()).unwrap();
            self.start_search(&q);
            return;
        }
        let loc = self.location();
        self.dir_list.set_file(None::<&gio::File>);
        self.dir_list.set_file(Some(&loc));
    }

    fn load(&self, file: gio::File, select: Option<String>) {
        self.cancel_search();
        {
            let mut st = self.state.borrow_mut();
            st.location = file.clone();
            st.pending_select = select;
            st.kept_selection.clear();
        }
        self.location_column.set_visible(false);
        self.filter_model.set_model(Some(&self.dir_list));
        self.dir_list.set_file(Some(&file));
        if self.stack.visible_child_name().as_deref() == Some("error") {
            self.stack.set_visible_child_name(self.view().as_str());
        }
        self.emit(TabEvent::LocationChanged);
    }

    /// Select (and scroll to) the item called `name` once it appears.
    pub fn select_name(&self, name: Option<String>) {
        self.state.borrow_mut().pending_select = name;
        self.apply_pending_select();
    }

    fn apply_pending_select(&self) {
        let Some(name) = self.state.borrow().pending_select.clone() else { return };
        let n = self.selection.n_items();
        let found = (0..n).find(|&i| {
            self.selection
                .item(i)
                .and_downcast::<gio::FileInfo>()
                .is_some_and(|info| info.name().as_os_str() == name.as_str())
        });
        if let Some(pos) = found {
            self.state.borrow_mut().pending_select = None;
            let flags = gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS;
            match self.view() {
                ViewMode::Details => self.column_view.scroll_to(pos, None, flags, None),
                ViewMode::List => self.list_view.scroll_to(pos, flags, None),
                ViewMode::Icons => self.grid_view.scroll_to(pos, flags, None),
            }
        }
    }

    fn update_empty(&self) {
        let searching = self.state.borrow().search.as_ref().map(|s| s.running);
        let loading = match searching {
            Some(running) => running,
            None => self.dir_list.is_loading(),
        };
        let show = !loading && self.selection.n_items() == 0 && self.dir_list.error().is_none();
        self.empty_label.set_label(if searching.is_some() { "No items match your search." } else { "This folder is empty." });
        self.empty_label.set_visible(show);
    }

    pub fn is_loading(&self) -> bool {
        match &self.state.borrow().search {
            Some(s) => s.running,
            None => self.dir_list.is_loading(),
        }
    }

    // ---- views & selection ------------------------------------------------

    pub fn view(&self) -> ViewMode {
        self.state.borrow().view
    }

    pub fn set_view(&self, view: ViewMode) {
        self.state.borrow_mut().view = view;
        if self.stack.visible_child_name().as_deref() != Some("error") {
            self.stack.set_visible_child_name(view.as_str());
        }
    }

    pub fn focus_view(&self) {
        match self.view() {
            ViewMode::Details => self.column_view.grab_focus(),
            ViewMode::List => self.list_view.grab_focus(),
            ViewMode::Icons => self.grid_view.grab_focus(),
        };
    }

    /// Current sort as (key, ascending).
    pub fn sort(&self) -> (&'static str, bool) {
        let sorter = self.column_view.sorter().and_downcast::<gtk::ColumnViewSorter>();
        let col = sorter.as_ref().and_then(|s| s.primary_sort_column());
        let key = self
            .sort_columns
            .iter()
            .find(|(_, c)| Some(c) == col.as_ref())
            .map(|(k, _)| *k)
            .unwrap_or("name");
        let asc = sorter.is_none_or(|s| s.primary_sort_order() == gtk::SortType::Ascending);
        (key, asc)
    }

    pub fn set_sort(&self, key: &str, ascending: bool) {
        if let Some((_, col)) = self.sort_columns.iter().find(|(k, _)| *k == key) {
            let order = if ascending { gtk::SortType::Ascending } else { gtk::SortType::Descending };
            self.column_view.sort_by_column(Some(col), order);
        }
    }

    pub fn refilter(&self) {
        self.hidden_filter.changed(gtk::FilterChange::Different);
    }

    pub fn n_items(&self) -> u32 {
        self.selection.n_items()
    }

    pub fn selected(&self) -> Vec<gio::FileInfo> {
        let bits = self.selection.selection();
        (0..bits.size())
            .filter_map(|i| self.selection.item(bits.nth(i as u32)).and_downcast())
            .collect()
    }

    pub fn selected_files(&self) -> Vec<gio::File> {
        self.selected().iter().map(util::file_of).collect()
    }

    fn info_at(&self, pos: u32) -> Option<gio::FileInfo> {
        self.selection.item(pos).and_downcast()
    }

    fn activate(&self, pos: u32) {
        let infos = if self.selection.is_selected(pos) {
            self.selected()
        } else {
            self.info_at(pos).into_iter().collect()
        };
        if !infos.is_empty() {
            self.emit(TabEvent::Open(infos));
        }
    }

    pub(crate) fn item_context_menu(&self, pos: u32, widget: &gtk::Widget, x: f64, y: f64) {
        if !self.selection.is_selected(pos) {
            self.selection.select_item(pos, true);
        }
        self.emit(TabEvent::ContextMenu { widget: widget.clone(), x, y });
    }

    pub(crate) fn item_middle_click(&self, pos: u32) {
        if let Some(info) = self.info_at(pos).filter(util::is_dir) {
            self.emit(TabEvent::OpenInNewTab(util::file_of(&info)));
        }
    }

    pub(crate) fn drag_files(&self, pos: u32) -> Vec<gio::File> {
        if self.selection.is_selected(pos) {
            self.selected_files()
        } else {
            self.info_at(pos).map(|i| util::file_of(&i)).into_iter().collect()
        }
    }

    pub(crate) fn drop_files(&self, files: Vec<gio::File>, dest: gio::File, state: gdk::ModifierType) {
        let files: Vec<_> = files.into_iter().filter(|f| !f.equal(&dest)).collect();
        if files.is_empty() {
            return;
        }
        let copy = if state.contains(gdk::ModifierType::CONTROL_MASK) {
            true
        } else if state.contains(gdk::ModifierType::SHIFT_MASK) {
            false
        } else {
            !same_device(&files[0], &dest)
        };
        // A plain move back into the folder the files already live in is a no-op.
        if !copy && files.iter().all(|f| f.parent().is_some_and(|p| p.equal(&dest))) {
            return;
        }
        self.emit(TabEvent::Drop { files, dest, copy });
    }

    // ---- search -----------------------------------------------------------

    pub fn is_searching(&self) -> bool {
        self.state.borrow().search.is_some()
    }

    pub fn search(&self) -> Option<(String, bool)> {
        self.state.borrow().search.as_ref().map(|s| (s.query.clone(), s.running))
    }

    pub fn start_search(self: &Rc<Self>, query: &str) {
        self.cancel_search();
        self.state.borrow_mut().kept_selection.clear();
        let Some(root) = self.location().path() else { return };
        let store = gio::ListStore::new::<gio::FileInfo>();
        let (rx, cancel) = search::spawn(root, query.to_string(), config::with(|c| c.show_hidden));
        self.state.borrow_mut().search = Some(SearchState {
            query: query.to_string(),
            running: true,
            cancel: cancel.clone(),
        });
        self.location_column.set_visible(true);
        self.filter_model.set_model(Some(&store));
        self.emit(TabEvent::LocationChanged);

        let w = self.weak();
        glib::spawn_future_local(async move {
            while let Ok(batch) = rx.recv().await {
                if cancel.load(Ordering::Relaxed) {
                    return;
                }
                let infos: Vec<gio::FileInfo> =
                    batch.iter().map(|(p, md)| util::info_from_path(p, Some(md))).collect();
                store.splice(store.n_items(), 0, &infos);
            }
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if let Some(t) = w.upgrade() {
                if let Some(s) = t.state.borrow_mut().search.as_mut() {
                    s.running = false;
                }
                t.update_empty();
                t.emit(TabEvent::ItemsChanged);
            }
        });
    }

    fn cancel_search(&self) {
        if let Some(s) = self.state.borrow_mut().search.take() {
            s.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn end_search(&self) {
        if self.is_searching() {
            let loc = self.location();
            self.load(loc, None);
        }
    }
}

impl Drop for Tab {
    fn drop(&mut self) {
        self.cancel_search();
    }
}

fn same_device(a: &gio::File, b: &gio::File) -> bool {
    use std::os::unix::fs::MetadataExt;
    let dev = |f: &gio::File| f.path().and_then(|p| std::fs::metadata(p).ok()).map(|m| m.dev());
    let (da, db) = (dev(a), dev(b));
    da.is_some() && da == db
}
