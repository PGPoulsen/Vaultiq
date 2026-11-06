//! The explorer window: header bar, tabs, sidebar/tree, preview pane,
//! status bar, and all user-facing actions.

use crate::addressbar::AddressBar;
use crate::config::{self, ViewMode};
use crate::fileops::{self, OpsPanel, Transfer};
use crate::openwith::{self, Notify};
use crate::preview::Preview;
use crate::sidebar::{OpenFn, Sidebar};
use crate::tab::{Tab, TabEvent};
use crate::tree::FolderTree;
use crate::{clipboard, properties, settings, util};
use adw::prelude::*;
use gtk::{gdk, gio, glib, graphene};
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

thread_local! {
    /// Open windows; each entry keeps its window's state alive.
    static WINDOWS: RefCell<Vec<Rc<Explorer>>> = const { RefCell::new(Vec::new()) };
}

pub struct Explorer {
    pub window: adw::ApplicationWindow,
    tab_view: adw::TabView,
    tabs: RefCell<Vec<Rc<Tab>>>,
    back: gtk::Button,
    forward: gtk::Button,
    up: gtk::Button,
    address: Rc<AddressBar>,
    search: gtk::SearchEntry,
    sidebar: Rc<Sidebar>,
    tree: Rc<FolderTree>,
    preview: Rc<Preview>,
    outer_paned: gtk::Paned,
    inner_paned: gtk::Paned,
    status: gtk::Label,
    toasts: adw::ToastOverlay,
    ops: Rc<OpsPanel>,
    actions: gio::SimpleActionGroup,
    /// Width of the preview pane, kept constant when the window resizes.
    preview_width: Cell<i32>,
    /// Set once the paned has a real size; position changes after that are
    /// user drags (or resizes, which keep the end child's width anyway).
    preview_placed: Cell<bool>,
}

pub fn set_accels(app: &adw::Application) {
    for (action, accels) in [
        ("win.new-tab", &["<Control>t"][..]),
        ("win.close-tab", &["<Control>w"]),
        ("win.new-window", &["<Control>n"]),
        ("win.focus-location", &["<Control>l", "<Alt>d"]),
        ("win.focus-search", &["<Control>f", "F3"]),
        ("win.refresh", &["F5", "<Control>r"]),
        ("win.forward", &["<Alt>Right"]),
        ("win.back", &["<Alt>Left"]),
        ("win.up", &["<Alt>Up"]),
        ("win.show-hidden", &["<Control>h"]),
        ("win.toggle-preview", &["<Alt>p"]),
        ("win.view::details", &["<Control>1"]),
        ("win.view::list", &["<Control>2"]),
        ("win.view::icons", &["<Control>3"]),
        ("win.open-terminal", &["<Control><Alt>t"]),
        ("win.settings", &["<Control>comma"]),
    ] {
        app.set_accels_for_action(action, accels);
    }
}

/// `$TERMINAL`, or the first common terminal emulator found in `$PATH`.
pub fn detect_terminal() -> Option<String> {
    if let Ok(t) = std::env::var("TERMINAL")
        && !t.is_empty()
    {
        return Some(t);
    }
    [
        "kitty", "foot", "alacritty", "wezterm", "ghostty", "gnome-terminal", "konsole", "xfce4-terminal", "xterm",
    ]
    .into_iter()
    .find(|t| glib::find_program_in_path(t).is_some())
    .map(String::from)
}

impl Explorer {
    pub fn open(app: &adw::Application, locations: Vec<gio::File>) {
        let ex = Explorer::build(app);
        let locations = if locations.is_empty() { vec![gio::File::for_path(glib::home_dir())] } else { locations };
        for loc in locations {
            ex.add_tab(loc, true);
        }
        ex.window.present();
        WINDOWS.with(|w| w.borrow_mut().push(ex));
    }

    fn build(app: &adw::Application) -> Rc<Explorer> {
        let ws = config::with(|c| c.window.clone());
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("Vaultiq")
            .default_width(ws.width)
            .default_height(ws.height)
            .maximized(ws.maximized)
            .build();

        // Callbacks need the explorer, which doesn't exist yet: route them
        // through a cell filled in after construction.
        let me: Rc<RefCell<std::rc::Weak<Explorer>>> = Rc::default();
        let with_me = {
            let me = me.clone();
            move || me.borrow().upgrade()
        };

        // Header bar
        let nav_button = |icon: &str, tip: &str, action: &str| {
            gtk::Button::builder().icon_name(icon).tooltip_text(tip).action_name(action).build()
        };
        let back = nav_button("go-previous-symbolic", "Back (Alt+Left)", "win.back");
        let forward = nav_button("go-next-symbolic", "Forward (Alt+Right)", "win.forward");
        let up = nav_button("go-up-symbolic", "Up (Alt+Up)", "win.up");
        let refresh = nav_button("view-refresh-symbolic", "Refresh (F5)", "win.refresh");

        let m = with_me.clone();
        let navigate: Rc<dyn Fn(gio::File)> = Rc::new(move |f| {
            if let Some(ex) = m() {
                ex.navigate_active(f);
            }
        });
        let m = with_me.clone();
        let submit: Rc<dyn Fn(String)> = Rc::new(move |text| {
            if let Some(ex) = m() {
                ex.address_submitted(&text);
            }
        });
        let address = AddressBar::new(navigate, submit);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search")
            .search_delay(400)
            .width_chars(18)
            .build();

        let menu = gio::Menu::new();
        let section = gio::Menu::new();
        section.append(Some("New Window"), Some("win.new-window"));
        section.append(Some("New Tab"), Some("win.new-tab"));
        menu.append_section(None, &section);
        let section = gio::Menu::new();
        section.append(Some("Show Hidden Files"), Some("win.show-hidden"));
        section.append(Some("Show Preview Pane"), Some("win.toggle-preview"));
        menu.append_section(None, &section);
        let section = gio::Menu::new();
        section.append(Some("Settings"), Some("win.settings"));
        section.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
        section.append(Some("About Vaultiq"), Some("win.about"));
        menu.append_section(None, &section);
        let menu_button = gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).primary(true).build();
        let preview_toggle = gtk::ToggleButton::builder()
            .icon_name("view-dual-symbolic")
            .tooltip_text("Preview pane (Alt+P)")
            .action_name("win.toggle-preview")
            .build();

        let header = adw::HeaderBar::new();
        header.pack_start(&back);
        header.pack_start(&forward);
        header.pack_start(&up);
        header.pack_start(&refresh);
        header.set_title_widget(Some(&address.widget));
        header.pack_end(&menu_button);
        header.pack_end(&preview_toggle);
        header.pack_end(&search);

        // Tabs
        let tab_view = adw::TabView::new();
        let tab_bar = adw::TabBar::builder().view(&tab_view).autohide(false).build();
        let new_tab = gtk::Button::builder()
            .icon_name("tab-new-symbolic")
            .tooltip_text("New tab (Ctrl+T)")
            .action_name("win.new-tab")
            .css_classes(["flat"])
            .build();
        tab_bar.set_end_action_widget(Some(&new_tab));

        // Left pane: quick access above the folder tree.
        let m = with_me.clone();
        let open_fn: OpenFn = Rc::new(move |file, new_tab| {
            if let Some(ex) = m() {
                if new_tab {
                    ex.add_tab(file, false);
                } else {
                    ex.navigate_active(file);
                }
            }
        });
        let sidebar = Sidebar::new(open_fn.clone());
        let tree = FolderTree::new(open_fn);
        let left_pane = gtk::Box::new(gtk::Orientation::Vertical, 0);
        left_pane.append(&sidebar.widget);
        left_pane.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        left_pane.append(&tree.widget);
        left_pane.add_css_class("vq-sidebar");

        let preview = Preview::new();
        let inner_paned = gtk::Paned::builder()
            .start_child(&tab_view)
            .end_child(&preview.widget)
            .resize_end_child(false)
            .shrink_end_child(false)
            .shrink_start_child(false)
            .build();
        preview.widget.set_size_request(220, -1);
        let outer_paned = gtk::Paned::builder()
            .start_child(&left_pane)
            .end_child(&inner_paned)
            .resize_start_child(false)
            .shrink_start_child(false)
            .shrink_end_child(false)
            .position(ws.sidebar_width)
            .build();
        left_pane.set_size_request(160, -1);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&outer_paned));

        // Status bar with view switcher, as in Explorer.
        let status = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let views = gtk::Box::builder().css_classes(["linked"]).build();
        for (mode, icon, tip) in [
            ("details", "view-list-symbolic", "Details (Ctrl+1)"),
            ("list", "view-continuous-symbolic", "List (Ctrl+2)"),
            ("icons", "view-grid-symbolic", "Icons (Ctrl+3)"),
        ] {
            let b = gtk::ToggleButton::builder()
                .icon_name(icon)
                .tooltip_text(tip)
                .action_name("win.view")
                .action_target(&mode.to_variant())
                .css_classes(["flat"])
                .build();
            views.append(&b);
        }
        let statusbar = gtk::Box::builder().css_classes(["vq-statusbar"]).spacing(12).build();
        statusbar.append(&status);
        statusbar.append(&views);

        let ops = OpsPanel::new();
        let toolbar = adw::ToolbarView::builder().top_bar_style(adw::ToolbarStyle::Raised).build();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&tab_bar);
        toolbar.set_content(Some(&toasts));
        toolbar.add_bottom_bar(&ops.widget);
        toolbar.add_bottom_bar(&statusbar);
        window.set_content(Some(&toolbar));

        let actions = gio::SimpleActionGroup::new();
        let ex = Rc::new(Explorer {
            window,
            tab_view,
            tabs: RefCell::default(),
            back,
            forward,
            up,
            address,
            search,
            sidebar,
            tree,
            preview,
            outer_paned,
            inner_paned,
            status,
            toasts,
            ops,
            actions,
            preview_width: Cell::new(ws.preview_width.max(220)),
            preview_placed: Cell::new(false),
        });
        *me.borrow_mut() = Rc::downgrade(&ex);
        ex.setup_actions();
        ex.connect_signals();
        ex.apply_settings();
        ex
    }

    fn connect_signals(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        self.tab_view.connect_selected_page_notify(move |_| {
            if let Some(ex) = w.upgrade() {
                ex.update_chrome();
            }
        });
        let w = Rc::downgrade(self);
        self.tab_view.connect_page_detached(move |view, page, _| {
            let Some(ex) = w.upgrade() else { return };
            ex.tabs.borrow_mut().retain(|t| t.widget != page.child());
            if view.n_pages() == 0 {
                ex.window.close();
            }
        });

        let w = Rc::downgrade(self);
        self.search.connect_search_changed(move |entry| {
            let Some(ex) = w.upgrade() else { return };
            let Some(tab) = ex.active_tab() else { return };
            let text = entry.text().trim().to_string();
            let current = tab.search().map(|(q, _)| q).unwrap_or_default();
            if text == current {
                return; // tab switch syncing the entry
            }
            if text.is_empty() {
                tab.end_search();
            } else {
                tab.start_search(&text);
            }
        });
        let w = Rc::downgrade(self);
        self.search.connect_stop_search(move |entry| {
            entry.set_text("");
            if let Some(tab) = w.upgrade().and_then(|ex| ex.active_tab()) {
                tab.end_search();
                tab.focus_view();
            }
        });
        let w = Rc::downgrade(self);
        self.search.connect_activate(move |_| {
            if let Some(tab) = w.upgrade().and_then(|ex| ex.active_tab()) {
                tab.focus_view();
            }
        });

        // Mouse back/forward buttons.
        let click = gtk::GestureClick::builder().button(0).propagation_phase(gtk::PropagationPhase::Capture).build();
        let w = Rc::downgrade(self);
        click.connect_pressed(move |g, _, _, _| {
            let Some(tab) = w.upgrade().and_then(|ex| ex.active_tab()) else { return };
            match g.current_button() {
                8 => tab.go_back(),
                9 => tab.go_forward(),
                _ => return,
            }
            g.set_state(gtk::EventSequenceState::Claimed);
        });
        self.window.add_controller(click);

        // Keep the preview pane at its width while the window is resized
        // (Hyprland resizes the window right after mapping it when tiling).
        let w = Rc::downgrade(self);
        self.inner_paned.connect_max_position_notify(move |p| {
            let Some(ex) = w.upgrade() else { return };
            // max-position is the width minus the preview's minimum width.
            if p.width() > 0 && ex.preview.widget.is_visible() {
                let pos = (p.width() - ex.preview_width.get()).clamp(p.width() / 3, p.max_position());
                p.set_position(pos);
                ex.preview_placed.set(true);
            }
        });
        let w = Rc::downgrade(self);
        self.inner_paned.connect_position_notify(move |p| {
            let Some(ex) = w.upgrade() else { return };
            if ex.preview_placed.get() && ex.preview.widget.is_visible() && p.width() > 0 {
                ex.preview_width.set((p.width() - p.position()).max(220));
            }
        });

        let w = Rc::downgrade(self);
        self.window.connect_close_request(move |win| {
            if let Some(ex) = w.upgrade() {
                ex.save_state();
                WINDOWS.with(|ws| ws.borrow_mut().retain(|e| !Rc::ptr_eq(e, &ex)));
            }
            let _ = win;
            glib::Propagation::Proceed
        });
    }

    fn save_state(&self) {
        // Tiling compositors (Hyprland) report tiled windows as maximized;
        // don't persist either the size or the "maximized" flag for those.
        let tiled = self
            .window
            .surface()
            .and_downcast::<gdk::Toplevel>()
            .is_some_and(|t| t.state().intersects(gdk::ToplevelState::TILED | gdk::ToplevelState::LEFT_TILED | gdk::ToplevelState::RIGHT_TILED | gdk::ToplevelState::TOP_TILED | gdk::ToplevelState::BOTTOM_TILED));
        let maximized = self.window.is_maximized() && !tiled;
        let (width, height) = (self.window.width(), self.window.height());
        let sidebar = self.outer_paned.position();
        let preview = self.preview_width.get();
        config::update(|c| {
            if !tiled {
                c.window.maximized = maximized;
                if !maximized && width > 0 {
                    c.window.width = width;
                    c.window.height = height;
                }
            }
            c.window.sidebar_width = sidebar;
            c.window.preview_width = preview;
        });
    }

    // ---- tabs -------------------------------------------------------------

    pub fn active_tab(&self) -> Option<Rc<Tab>> {
        let page = self.tab_view.selected_page()?;
        let child = page.child();
        self.tabs.borrow().iter().find(|t| t.widget == child).cloned()
    }

    fn page_for(&self, tab: &Tab) -> Option<adw::TabPage> {
        (0..self.tab_view.n_pages())
            .map(|i| self.tab_view.nth_page(i))
            .find(|p| p.child() == tab.widget)
    }

    fn is_active(&self, tab: &Tab) -> bool {
        self.tab_view.selected_page().is_some_and(|p| p.child() == tab.widget)
    }

    pub fn add_tab(self: &Rc<Self>, location: gio::File, select: bool) {
        let w = Rc::downgrade(self);
        let events: crate::tab::EventFn = Rc::new(move |tab, ev| {
            if let Some(ex) = w.upgrade() {
                ex.on_tab_event(tab, ev);
            }
        });
        let tab = Tab::new(location, events);
        self.tabs.borrow_mut().push(tab.clone());
        let page = self.tab_view.append(&tab.widget);
        page.set_icon(Some(&gio::ThemedIcon::new("folder-symbolic")));
        self.update_tab_title(&tab);
        if select {
            self.tab_view.set_selected_page(&page);
            tab.focus_view();
        }
    }

    fn update_tab_title(&self, tab: &Tab) {
        let Some(page) = self.page_for(tab) else { return };
        let loc = tab.location();
        let title = match tab.search() {
            Some((q, _)) => format!("Search: {q}"),
            None => util::location_name(&loc),
        };
        page.set_title(&title);
        page.set_tooltip(&glib::markup_escape_text(&loc.parse_name()));
        page.set_loading(tab.is_loading());
    }

    fn navigate_active(&self, file: gio::File) {
        if let Some(tab) = self.active_tab() {
            tab.navigate(file);
            tab.focus_view();
        }
    }

    fn on_tab_event(self: &Rc<Self>, tab: &Tab, ev: TabEvent) {
        match ev {
            TabEvent::LocationChanged => {
                self.update_tab_title(tab);
                if self.is_active(tab) {
                    self.update_chrome();
                }
            }
            TabEvent::SelectionChanged => {
                if self.is_active(tab) {
                    self.update_selection();
                }
            }
            TabEvent::ItemsChanged => {
                if let Some(page) = self.page_for(tab) {
                    page.set_loading(tab.is_loading());
                }
                if self.is_active(tab) {
                    self.update_status();
                }
            }
            TabEvent::Open(infos) => self.open_infos(infos),
            TabEvent::OpenInNewTab(file) => self.add_tab(file, false),
            TabEvent::ContextMenu { widget, x, y } => self.context_menu(tab, &widget, x, y),
            TabEvent::Drop { files, dest, copy } => {
                self.transfer(if copy { Transfer::Copy } else { Transfer::Move }, files, dest)
            }
        }
    }

    /// Sync everything outside the tab with the active tab.
    fn update_chrome(self: &Rc<Self>) {
        let Some(tab) = self.active_tab() else { return };
        let loc = tab.location();
        let search = tab.search();
        self.back.set_sensitive(tab.can_back() || search.is_some());
        self.forward.set_sensitive(tab.can_forward());
        self.up.set_sensitive(loc.parent().is_some());
        self.address.set_location(&loc, search.as_ref().map(|(q, _)| q.as_str()));
        let q = search.map(|(q, _)| q).unwrap_or_default();
        if self.search.text() != q {
            self.search.set_text(&q);
        }
        self.tree.reveal(loc.clone());
        self.sidebar.select_location(&loc);
        self.window.set_title(Some(&format!("{} — Vaultiq", util::location_name(&loc))));
        self.action("view").set_state(&tab.view().as_str().to_variant());
        let (key, asc) = tab.sort();
        self.action("sort-by").set_state(&key.to_variant());
        self.action("sort-order").set_state(&(if asc { "asc" } else { "desc" }).to_variant());
        self.update_selection();
    }

    fn update_selection(&self) {
        let Some(tab) = self.active_tab() else { return };
        let sel = tab.selected();
        if self.preview.widget.is_visible() {
            self.preview.show(&sel);
        }
        let any = !sel.is_empty();
        for name in ["open", "cut", "copy", "copy-path", "trash", "delete", "properties", "open-with-other"] {
            self.action(name).set_enabled(any);
        }
        self.action("rename").set_enabled(sel.len() == 1 && !tab.is_searching());
        self.update_status();
    }

    fn update_status(&self) {
        let Some(tab) = self.active_tab() else { return };
        let n = tab.n_items();
        let mut text = format!("{n} item{}", if n == 1 { "" } else { "s" });
        if let Some((_, running)) = tab.search() {
            text = format!("{n} result{}{}", if n == 1 { "" } else { "s" }, if running { " — searching…" } else { "" });
        } else if tab.is_loading() {
            text.push_str(" — loading…");
        }
        let sel = tab.selected();
        if !sel.is_empty() {
            text.push_str(&format!("     {} selected", sel.len()));
            let files: Vec<_> = sel.iter().filter(|i| !util::is_dir(i)).collect();
            if !files.is_empty() {
                let size: i64 = files.iter().map(|i| i.size()).sum();
                text.push_str(&format!("  {}", util::format_size(size as u64)));
            }
        }
        self.status.set_text(&text);
    }

    fn apply_settings(self: &Rc<Self>) {
        let (hidden, preview) = config::with(|c| (c.show_hidden, c.show_preview));
        self.action("show-hidden").set_state(&hidden.to_variant());
        self.action("toggle-preview").set_state(&preview.to_variant());
        self.preview.widget.set_visible(preview);
        for tab in self.tabs.borrow().iter() {
            tab.refilter();
        }
        self.tree.refilter();
        self.update_selection();
    }

    /// Re-apply settings in every window (they share one config).
    fn apply_settings_everywhere() {
        let windows = WINDOWS.with(|w| w.borrow().clone());
        for ex in windows {
            ex.apply_settings();
        }
    }

    // ---- opening ----------------------------------------------------------

    fn notifier(self: &Rc<Self>) -> Notify {
        let w = Rc::downgrade(self);
        Rc::new(move |msg: &str| {
            if let Some(ex) = w.upgrade() {
                ex.toast(msg);
            }
        })
    }

    pub fn toast(&self, msg: &str) {
        let toast = adw::Toast::builder().title(glib::markup_escape_text(msg)).timeout(5).build();
        self.toasts.add_toast(toast);
    }

    fn report_errors(&self, errors: &[String]) {
        match errors {
            [] => {}
            [one] => self.toast(one),
            many => {
                let mut body = many.iter().take(12).cloned().collect::<Vec<_>>().join("\n");
                if many.len() > 12 {
                    body.push_str(&format!("\n… and {} more", many.len() - 12));
                }
                let d = adw::AlertDialog::builder().heading(format!("{} errors occurred", many.len())).body(body).build();
                d.add_response("ok", "OK");
                d.present(Some(&self.window));
            }
        }
    }

    fn open_infos(self: &Rc<Self>, infos: Vec<gio::FileInfo>) {
        let (dirs, files): (Vec<_>, Vec<_>) = infos.into_iter().partition(util::is_dir);
        if !files.is_empty() {
            openwith::open_default(&files, self.window.upcast_ref(), self.notifier());
        }
        match dirs.as_slice() {
            [one] if files.is_empty() => self.navigate_active(util::file_of(one)),
            _ => {
                for d in dirs {
                    self.add_tab(util::file_of(&d), false);
                }
            }
        }
    }

    fn address_submitted(self: &Rc<Self>, text: &str) {
        let Some(tab) = self.active_tab() else { return };
        if text.trim().is_empty() {
            tab.focus_view();
            return;
        }
        let cwd = tab.location().path().unwrap_or_else(glib::home_dir);
        let file = util::expand_path(text, &cwd);
        match file.query_file_type(gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>) {
            gio::FileType::Directory => self.navigate_active(file),
            gio::FileType::Unknown if file.path().is_some() => self.toast(&format!("Can't find “{}”", text.trim())),
            gio::FileType::Unknown => self.navigate_active(file), // let GIO try the URI
            _ => {
                if let Some(path) = file.path() {
                    let info = util::info_from_path(&path, std::fs::metadata(&path).ok().as_ref());
                    openwith::open_default(&[info], self.window.upcast_ref(), self.notifier());
                }
            }
        }
    }

    fn open_terminal(&self, dir: Option<PathBuf>) {
        let Some(dir) = dir else { return };
        let Some(cmd) = config::with(|c| c.terminal.clone()).or_else(detect_terminal) else {
            self.toast("No terminal found — set one in Settings");
            return;
        };
        let argv = match glib::shell_parse_argv(&cmd) {
            Ok(a) if !a.is_empty() => a,
            _ => {
                self.toast(&format!("Invalid terminal command: {cmd}"));
                return;
            }
        };
        let mut command = std::process::Command::new(&argv[0]);
        command.args(&argv[1..]).current_dir(&dir);
        if let Err(e) = command.spawn() {
            self.toast(&format!("Couldn't start {}: {e}", argv[0].to_string_lossy()));
        }
    }

    // ---- file operations ---------------------------------------------------

    fn transfer(self: &Rc<Self>, kind: Transfer, files: Vec<gio::File>, dest: gio::File) {
        let Some(dest_path) = dest.path() else {
            self.toast("Only local folders are supported as destination");
            return;
        };
        let sources: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
        if sources.len() != files.len() {
            self.toast("Some items aren't local files and were skipped");
        }
        if sources.is_empty() {
            return;
        }
        let w = Rc::downgrade(self);
        self.ops.transfer(self.window.upcast_ref(), kind, sources, dest_path.clone(), move |outcome| {
            let Some(ex) = w.upgrade() else { return };
            ex.report_errors(&outcome.errors);
            if let Some(tab) = ex.active_tab()
                && tab.location().path().as_ref() == Some(&dest_path)
                && let Some(first) = outcome.created.first()
            {
                tab.select_name(first.file_name().map(|n| n.to_string_lossy().into_owned()));
            }
        });
    }

    fn paste(self: &Rc<Self>) {
        let Some(tab) = self.active_tab() else { return };
        if tab.is_searching() {
            return;
        }
        let dest = tab.location();
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(ex) = w.upgrade() else { return };
            let Some((files, cut)) = clipboard::get(&ex.window).await else {
                ex.toast("Nothing to paste");
                return;
            };
            if cut {
                clipboard::clear(&ex.window);
            }
            ex.transfer(if cut { Transfer::Move } else { Transfer::Copy }, files, dest);
        });
    }

    fn trash_selected(self: &Rc<Self>) {
        let Some(tab) = self.active_tab() else { return };
        let files = tab.selected_files();
        if files.is_empty() {
            return;
        }
        let n = files.len();
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let (errors, unsupported) = fileops::trash(files).await;
            let Some(ex) = w.upgrade() else { return };
            let trashed = n - errors.len() - unsupported.len();
            if trashed > 0 {
                ex.toast(&format!("Moved {trashed} item{} to the trash", if trashed == 1 { "" } else { "s" }));
            }
            ex.report_errors(&errors);
            // Like Explorer's "too big for the Recycle Bin": offer a real delete.
            let paths: Vec<PathBuf> = unsupported.iter().filter_map(|f| f.path()).collect();
            if !paths.is_empty() {
                let body = "This drive has no trash, so these items can't be moved there. \
                            Delete them permanently instead? You can't undo this action.";
                if let Some(errors) = fileops::delete_permanently(ex.window.upcast_ref(), paths, body).await {
                    ex.report_errors(&errors);
                }
            }
        });
    }

    fn delete_selected(self: &Rc<Self>) {
        let Some(tab) = self.active_tab() else { return };
        let paths: Vec<PathBuf> = tab.selected_files().iter().filter_map(|f| f.path()).collect();
        if paths.is_empty() {
            return;
        }
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(ex) = w.upgrade() else { return };
            if let Some(errors) = fileops::delete_permanently(
                ex.window.upcast_ref(),
                paths,
                "These items will be deleted immediately. You can't undo this action.",
            )
            .await {
                ex.report_errors(&errors);
            }
        });
    }

    fn rename_selected(self: &Rc<Self>) {
        let Some(tab) = self.active_tab() else { return };
        let sel = tab.selected();
        let [info] = sel.as_slice() else { return };
        let info = info.clone();
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(ex) = w.upgrade() else { return };
            let old = info.display_name().to_string();
            let Some(new) = fileops::ask_name(ex.window.upcast_ref(), "Rename", &old, "Rename", !util::is_dir(&info)).await else { return };
            if new == old {
                return;
            }
            let file = util::file_of(&info);
            match file.set_display_name_future(&new, glib::Priority::DEFAULT).await {
                Ok(_) => tab.select_name(Some(new)),
                Err(e) => ex.toast(&format!("Couldn't rename: {}", e.message())),
            }
        });
    }

    fn create_new(self: &Rc<Self>, folder: bool) {
        let Some(tab) = self.active_tab() else { return };
        let Some(dir) = tab.location().path().filter(|_| !tab.is_searching()) else { return };
        let w = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(ex) = w.upgrade() else { return };
            let (heading, base) = if folder { ("New folder", "New folder") } else { ("New text document", "New text document.txt") };
            let initial = util::unique_name(&dir, base);
            let Some(name) = fileops::ask_name(ex.window.upcast_ref(), heading, &initial, "Create", !folder).await else { return };
            let file = gio::File::for_path(dir.join(&name));
            let res = if folder {
                file.make_directory_future(glib::Priority::DEFAULT).await
            } else {
                file.create_future(gio::FileCreateFlags::NONE, glib::Priority::DEFAULT).await.map(|_| ())
            };
            match res {
                Ok(()) => tab.select_name(Some(name)),
                Err(e) => ex.toast(&format!("Couldn't create “{name}”: {}", e.message())),
            }
        });
    }

    fn pin(self: &Rc<Self>, paths: Vec<PathBuf>) {
        config::update(|c| {
            for p in paths {
                if !c.pinned.contains(&p) {
                    c.pinned.push(p);
                }
            }
        });
        let windows = WINDOWS.with(|w| w.borrow().clone());
        for e in windows {
            e.sidebar.rebuild();
        }
        self.update_chrome();
    }

    // ---- context menu -------------------------------------------------------

    fn context_menu(self: &Rc<Self>, tab: &Tab, widget: &gtk::Widget, x: f64, y: f64) {
        let sel = tab.selected();
        let menu = gio::Menu::new();
        let section = |items: &[(&str, &str)]| {
            let s = gio::Menu::new();
            for (label, action) in items {
                s.append(Some(label), Some(action));
            }
            menu.append_section(None, &s);
            s
        };

        if sel.is_empty() {
            let view = gio::Menu::new();
            for (label, target) in [("Details", "details"), ("List", "list"), ("Icons", "icons")] {
                view.append(Some(label), Some(&format!("win.view::{target}")));
            }
            let sort = gio::Menu::new();
            let keys = gio::Menu::new();
            for (label, key) in [("Name", "name"), ("Date modified", "date"), ("Type", "type"), ("Size", "size")] {
                keys.append(Some(label), Some(&format!("win.sort-by::{key}")));
            }
            let order = gio::Menu::new();
            order.append(Some("Ascending"), Some("win.sort-order::asc"));
            order.append(Some("Descending"), Some("win.sort-order::desc"));
            sort.append_section(None, &keys);
            sort.append_section(None, &order);
            let s = gio::Menu::new();
            s.append_submenu(Some("View"), &view);
            s.append_submenu(Some("Sort by"), &sort);
            s.append(Some("Refresh"), Some("win.refresh"));
            menu.append_section(None, &s);
            if !tab.is_searching() {
                section(&[("Paste", "win.paste")]);
                section(&[("New folder", "win.new-folder"), ("New text document", "win.new-file")]);
                section(&[("Open terminal here", "win.open-terminal"), ("Pin to Quick access", "win.pin-current")]);
            }
            section(&[("Properties", "win.folder-properties")]);
        } else {
            let all_dirs = sel.iter().all(util::is_dir);
            let open = section(&[("Open", "win.open")]);
            if all_dirs {
                open.append(Some("Open in new tab"), Some("win.open-new-tab"));
            }
            let with = gio::Menu::new();
            for (label, target) in openwith::menu_entries(&sel[0]) {
                let item = gio::MenuItem::new(Some(&label), None);
                item.set_action_and_target_value(Some("win.open-with"), Some(&target.to_variant()));
                if let Some(app) = openwith::app_from_target(&target)
                    && let Some(icon) = app.icon()
                {
                    item.set_icon(&icon);
                }
                with.append_item(&item);
            }
            let other = gio::Menu::new();
            other.append(Some("Choose another app…"), Some("win.open-with-other"));
            with.append_section(None, &other);
            open.append_submenu(Some("Open with"), &with);
            if tab.is_searching() {
                open.append(Some("Open file location"), Some("win.open-location"));
            }

            section(&[("Cut", "win.cut"), ("Copy", "win.copy"), ("Copy path", "win.copy-path")]);
            let edit = section(&[("Move to Trash", "win.trash"), ("Delete permanently", "win.delete")]);
            if sel.len() == 1 && !tab.is_searching() {
                edit.prepend(Some("Rename"), Some("win.rename"));
            }
            if all_dirs {
                let s = section(&[("Pin to Quick access", "win.pin")]);
                if sel.len() == 1 {
                    s.append(Some("Open terminal here"), Some("win.open-terminal-selected"));
                }
            }
            section(&[("Properties", "win.properties")]);
        }
        self.action("paste").set_enabled(clipboard::has_files(&self.window));

        let popover = gtk::PopoverMenu::builder()
            .menu_model(&menu)
            .has_arrow(false)
            .halign(gtk::Align::Start)
            .build();
        popover.set_parent(&tab.widget);
        let p = widget
            .compute_point(&tab.widget, &graphene::Point::new(x as f32, y as f32))
            .unwrap_or_else(|| graphene::Point::new(0.0, 0.0));
        popover.set_pointing_to(Some(&gdk::Rectangle::new(p.x() as i32, p.y() as i32, 1, 1)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    // ---- actions --------------------------------------------------------------

    fn action(&self, name: &str) -> gio::SimpleAction {
        self.actions.lookup_action(name).and_downcast().unwrap()
    }

    fn setup_actions(self: &Rc<Self>) {
        let add = |name: &str, f: fn(&Rc<Explorer>)| {
            let a = gio::SimpleAction::new(name, None);
            let w = Rc::downgrade(self);
            a.connect_activate(move |_, _| {
                if let Some(ex) = w.upgrade() {
                    f(&ex);
                }
            });
            self.actions.add_action(&a);
        };
        let with_tab = |name: &str, f: fn(&Rc<Explorer>, &Rc<Tab>)| {
            let a = gio::SimpleAction::new(name, None);
            let w = Rc::downgrade(self);
            a.connect_activate(move |_, _| {
                if let Some(ex) = w.upgrade()
                    && let Some(tab) = ex.active_tab()
                {
                    f(&ex, &tab);
                }
            });
            self.actions.add_action(&a);
        };

        add("new-tab", |ex| {
            let loc = ex.active_tab().map(|t| t.location()).unwrap_or_else(|| gio::File::for_path(glib::home_dir()));
            ex.add_tab(loc, true);
        });
        add("close-tab", |ex| {
            if let Some(page) = ex.tab_view.selected_page() {
                ex.tab_view.close_page(&page);
            }
        });
        add("new-window", |ex| {
            let loc = ex.active_tab().map(|t| t.location());
            if let Some(app) = ex.window.application().and_downcast::<adw::Application>() {
                Explorer::open(&app, loc.into_iter().collect());
            }
        });
        with_tab("back", |_, t| {
            if t.is_searching() { t.end_search() } else { t.go_back() }
        });
        with_tab("forward", |_, t| t.go_forward());
        with_tab("up", |_, t| t.go_up());
        with_tab("refresh", |_, t| t.refresh());
        add("focus-location", |ex| ex.address.edit());
        add("focus-search", |ex| {
            ex.search.grab_focus();
        });

        with_tab("open", |ex, t| ex.open_infos(t.selected()));
        with_tab("open-new-tab", |ex, t| {
            for f in t.selected_files() {
                ex.add_tab(f, false);
            }
        });
        with_tab("open-location", |ex, t| {
            if let Some(info) = t.selected().first() {
                let file = util::file_of(info);
                if let Some(parent) = file.parent() {
                    ex.search.set_text("");
                    t.navigate(parent);
                    t.select_name(file.basename().map(|b| b.to_string_lossy().into_owned()));
                }
            }
        });
        with_tab("open-with-other", |ex, t| {
            openwith::choose(&ex.window, t.selected(), ex.notifier());
        });
        let open_with = gio::SimpleAction::new("open-with", Some(glib::VariantTy::STRING));
        let w = Rc::downgrade(self);
        open_with.connect_activate(move |_, param| {
            let Some(ex) = w.upgrade() else { return };
            let Some(tab) = ex.active_tab() else { return };
            let Some(target) = param.and_then(|p| p.get::<String>()) else { return };
            match openwith::app_from_target(&target) {
                Some(app) => {
                    if let Err(e) = openwith::launch(&app, &tab.selected_files(), &ex.window) {
                        ex.toast(&format!("Couldn't start {}: {}", app.display_name(), e.message()));
                    }
                }
                None => ex.toast("Application not found"),
            }
        });
        self.actions.add_action(&open_with);

        with_tab("cut", |ex, t| clipboard::set(&ex.window, &t.selected_files(), true));
        with_tab("copy", |ex, t| clipboard::set(&ex.window, &t.selected_files(), false));
        add("paste", |ex| ex.paste());
        with_tab("context-menu", |ex, t| {
            // Keyboard-invoked: anchor the menu below the focused row.
            let focus = gtk::prelude::GtkWindowExt::focus(&ex.window);
            let (x, y) = focus
                .and_then(|f| f.compute_bounds(&t.widget))
                .map(|r| (r.x() as f64 + 24.0, (r.y() + r.height()) as f64))
                .unwrap_or((24.0, 24.0));
            ex.context_menu(t, t.widget.upcast_ref(), x, y);
        });
        with_tab("copy-path", |ex, t| {
            let text = t
                .selected_files()
                .iter()
                .map(|f| f.path().map(|p| p.display().to_string()).unwrap_or_else(|| f.uri().to_string()))
                .collect::<Vec<_>>()
                .join("\n");
            clipboard::set_text(&ex.window, &text);
            ex.toast("Path copied");
        });
        add("rename", |ex| ex.rename_selected());
        add("trash", |ex| ex.trash_selected());
        add("delete", |ex| ex.delete_selected());
        add("new-folder", |ex| ex.create_new(true));
        add("new-file", |ex| ex.create_new(false));
        with_tab("properties", |ex, t| properties::show(ex.window.upcast_ref(), t.selected(), ex.notifier()));
        with_tab("folder-properties", |ex, t| {
            if let Some(path) = t.location().path() {
                let info = util::info_from_path(&path, std::fs::metadata(&path).ok().as_ref());
                properties::show(ex.window.upcast_ref(), vec![info], ex.notifier());
            }
        });
        with_tab("open-terminal", |ex, t| ex.open_terminal(t.location().path()));
        with_tab("open-terminal-selected", |ex, t| ex.open_terminal(t.selected_files().first().and_then(|f| f.path())));
        with_tab("pin", |ex, t| ex.pin(t.selected_files().iter().filter_map(|f| f.path()).collect()));
        with_tab("pin-current", |ex, t| ex.pin(t.location().path().into_iter().collect()));

        let view = gio::SimpleAction::new_stateful("view", Some(glib::VariantTy::STRING), &"details".to_variant());
        let w = Rc::downgrade(self);
        view.connect_activate(move |a, param| {
            let Some(mode) = param.and_then(|p| p.get::<String>()).and_then(|s| ViewMode::from_str(&s)) else { return };
            a.set_state(&mode.as_str().to_variant());
            if let Some(tab) = w.upgrade().and_then(|ex| ex.active_tab()) {
                tab.set_view(mode);
                tab.focus_view();
            }
        });
        self.actions.add_action(&view);

        let sort_by = gio::SimpleAction::new_stateful("sort-by", Some(glib::VariantTy::STRING), &"name".to_variant());
        let w = Rc::downgrade(self);
        sort_by.connect_activate(move |a, param| {
            let Some(key) = param.and_then(|p| p.get::<String>()) else { return };
            a.set_state(&key.to_variant());
            if let Some(tab) = w.upgrade().and_then(|ex| ex.active_tab()) {
                let (_, asc) = tab.sort();
                tab.set_sort(&key, asc);
            }
        });
        self.actions.add_action(&sort_by);
        let sort_order = gio::SimpleAction::new_stateful("sort-order", Some(glib::VariantTy::STRING), &"asc".to_variant());
        let w = Rc::downgrade(self);
        sort_order.connect_activate(move |a, param| {
            let Some(order) = param.and_then(|p| p.get::<String>()) else { return };
            a.set_state(&order.to_variant());
            if let Some(tab) = w.upgrade().and_then(|ex| ex.active_tab()) {
                let (key, _) = tab.sort();
                tab.set_sort(key, order == "asc");
            }
        });
        self.actions.add_action(&sort_order);

        let hidden = gio::SimpleAction::new_stateful("show-hidden", None, &false.to_variant());
        hidden.connect_activate(|a, _| {
            let v = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(false);
            config::update(|c| c.show_hidden = v);
            Explorer::apply_settings_everywhere();
        });
        self.actions.add_action(&hidden);
        let preview = gio::SimpleAction::new_stateful("toggle-preview", None, &true.to_variant());
        preview.connect_activate(|a, _| {
            let v = !a.state().and_then(|s| s.get::<bool>()).unwrap_or(true);
            config::update(|c| c.show_preview = v);
            Explorer::apply_settings_everywhere();
        });
        self.actions.add_action(&preview);

        add("settings", |ex| {
            settings::show(ex.window.upcast_ref(), Rc::new(Explorer::apply_settings_everywhere));
        });
        add("shortcuts", |ex| show_shortcuts(&ex.window));
        add("about", |ex| {
            adw::AboutDialog::builder()
                .application_name("Vaultiq")
                .application_icon("system-file-manager")
                .version(env!("CARGO_PKG_VERSION"))
                .comments("A tabbed file explorer for Linux and Wayland")
                .developer_name("Accuratech")
                .build()
                .present(Some(&ex.window));
        });

        self.window.insert_action_group("win", Some(&self.actions));
    }
}

fn show_shortcuts(parent: &adw::ApplicationWindow) {
    let page = adw::PreferencesPage::new();
    for (title, rows) in [
        (
            "Navigation",
            &[
                ("Back / Forward", "Alt+← / Alt+→, Backspace, mouse buttons"),
                ("Parent folder", "Alt+↑"),
                ("Edit location", "Ctrl+L, Alt+D (Tab completes)"),
                ("Search", "Ctrl+F, F3"),
                ("Refresh", "F5"),
            ][..],
        ),
        (
            "Tabs & windows",
            &[
                ("New tab / close tab", "Ctrl+T / Ctrl+W"),
                ("Next / previous tab", "Ctrl+Tab / Ctrl+Shift+Tab"),
                ("Open folder in new tab", "Middle click"),
                ("New window", "Ctrl+N"),
            ],
        ),
        (
            "Files",
            &[
                ("Cut / Copy / Paste", "Ctrl+X / Ctrl+C / Ctrl+V"),
                ("Rename", "F2"),
                ("Move to trash / delete permanently", "Delete / Shift+Delete"),
                ("New folder", "Ctrl+Shift+N"),
                ("Copy path", "Ctrl+Shift+C"),
                ("Properties", "Alt+Enter"),
                ("Drag: copy / move", "hold Ctrl / Shift"),
            ],
        ),
        (
            "View",
            &[
                ("Details / List / Icons", "Ctrl+1 / Ctrl+2 / Ctrl+3"),
                ("Show hidden files", "Ctrl+H"),
                ("Preview pane", "Alt+P"),
                ("Open terminal here", "Ctrl+Alt+T"),
            ],
        ),
    ] {
        let group = adw::PreferencesGroup::builder().title(title).build();
        for (what, keys) in rows {
            let row = adw::ActionRow::builder().title(*what).build();
            row.add_suffix(&gtk::Label::builder().label(*keys).css_classes(["dim-label"]).build());
            group.add(&row);
        }
        page.add(&group);
    }
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    adw::Dialog::builder()
        .title("Keyboard Shortcuts")
        .content_width(560)
        .content_height(640)
        .child(&toolbar)
        .build()
        .present(Some(parent));
}
