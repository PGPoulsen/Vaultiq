//! Application lookup and launching. Vaultiq-only overrides from the config
//! take precedence over the XDG defaults in `mimeapps.list`.

use crate::config::{self, Handler};
use crate::util;
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

pub type Notify = Rc<dyn Fn(&str)>;

pub fn find_app(id: &str) -> Option<gio::AppInfo> {
    gio::AppInfo::all().into_iter().find(|a| a.id().as_deref() == Some(id))
}

pub fn command_app(cmd: &str) -> Result<gio::AppInfo, glib::Error> {
    let name = cmd.split_whitespace().next().unwrap_or(cmd);
    gio::AppInfo::create_from_commandline(cmd, Some(name), gio::AppInfoCreateFlags::NONE)
}

pub fn handler_app(h: &Handler) -> Option<gio::AppInfo> {
    match h {
        Handler::App { app } => find_app(app),
        Handler::Command { command } => command_app(command).ok(),
    }
}

pub fn describe_handler(h: &Handler) -> String {
    match h {
        Handler::App { app } => find_app(app).map(|a| a.display_name().to_string()).unwrap_or_else(|| app.clone()),
        Handler::Command { command } => format!("Command: {command}"),
    }
}

fn override_for(info: &gio::FileInfo) -> Option<Handler> {
    let name = info.name();
    let ctype = util::content_type(info);
    config::with(|c| c.open_with.resolve(&name.to_string_lossy(), &ctype).cloned())
}

/// The application a double-click would use.
pub fn default_app(info: &gio::FileInfo) -> Option<gio::AppInfo> {
    override_for(info)
        .and_then(|h| handler_app(&h))
        .or_else(|| gio::AppInfo::default_for_type(&util::content_type(info), false))
}

/// Entries for the "Open with" submenu as (label, action target).
/// Targets are `app:<desktop id>` or `cmd:<command line>`.
pub fn menu_entries(info: &gio::FileInfo) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut seen = Vec::new();
    if let Some(h) = override_for(info) {
        match &h {
            Handler::App { app } => {
                if let Some(a) = find_app(app) {
                    out.push((a.display_name().to_string(), format!("app:{app}")));
                    seen.push(app.clone());
                }
            }
            Handler::Command { command } => out.push((format!("Command: {command}"), format!("cmd:{command}"))),
        }
    }
    let ctype = util::content_type(info);
    for app in apps_for_type(&ctype) {
        let Some(id) = app.id().map(|s| s.to_string()) else { continue };
        if seen.contains(&id) || out.len() >= 12 {
            continue;
        }
        seen.push(id.clone());
        out.push((app.display_name().to_string(), format!("app:{id}")));
    }
    out
}

/// Default app first, then apps registered for the type, then apps for its
/// parent types (e.g. text editors for a `text/x-script`).
fn apps_for_type(ctype: &str) -> Vec<gio::AppInfo> {
    let mut out: Vec<gio::AppInfo> = Vec::new();
    let candidates = gio::AppInfo::default_for_type(ctype, false)
        .into_iter()
        .chain(gio::AppInfo::recommended_for_type(ctype))
        .chain(gio::AppInfo::fallback_for_type(ctype));
    for app in candidates {
        if !out.iter().any(|a| a.equal(&app)) {
            out.push(app);
        }
    }
    out
}

pub fn app_from_target(target: &str) -> Option<gio::AppInfo> {
    if let Some(id) = target.strip_prefix("app:") {
        find_app(id)
    } else if let Some(cmd) = target.strip_prefix("cmd:") {
        command_app(cmd).ok()
    } else {
        None
    }
}

pub fn launch(app: &gio::AppInfo, files: &[gio::File], widget: &impl IsA<gtk::Widget>) -> Result<(), glib::Error> {
    // GdkAppLaunchContext passes an xdg-activation token on Wayland so the
    // compositor focuses the launched window.
    let ctx = widget.as_ref().display().app_launch_context();
    app.launch(files, Some(&ctx))
}

/// Open files with their default (or overridden) application, grouping files
/// that share an application into one launch. Files without any handler
/// bring up the chooser.
pub fn open_default(infos: &[gio::FileInfo], widget: &gtk::Widget, notify: Notify) {
    let mut groups: Vec<(gio::AppInfo, Vec<gio::File>)> = Vec::new();
    let mut unhandled = Vec::new();
    for info in infos {
        match default_app(info) {
            Some(app) => match groups.iter_mut().find(|(a, _)| a.equal(&app)) {
                Some((_, files)) => files.push(util::file_of(info)),
                None => groups.push((app, vec![util::file_of(info)])),
            },
            None => unhandled.push(info.clone()),
        }
    }
    for (app, files) in groups {
        if let Err(e) = launch(&app, &files, widget) {
            notify(&format!("Couldn't start {}: {}", app.display_name(), e.message()));
        }
    }
    if !unhandled.is_empty() {
        choose(widget, unhandled, notify);
    }
}

/// The "Choose another app" dialog.
pub fn choose(parent: &impl IsA<gtk::Widget>, infos: Vec<gio::FileInfo>, notify: Notify) {
    let Some(first) = infos.first().cloned() else { return };
    let ctype = util::content_type(&first);
    let desc = util::type_description(&first);

    let recommended = apps_for_type(&ctype);
    let mut others: Vec<gio::AppInfo> = gio::AppInfo::all()
        .into_iter()
        .filter(|a| a.should_show() && !recommended.iter().any(|r| r.equal(a)))
        .collect();
    others.sort_by_key(|a| a.display_name().to_lowercase());
    let n_recommended = recommended.len() as i32;
    let apps: Rc<Vec<gio::AppInfo>> = Rc::new(recommended.into_iter().chain(others).collect());

    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).css_classes(["boxed-list"]).build();
    for app in apps.iter() {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&app.display_name()))
            .subtitle(glib::markup_escape_text(app.id().as_deref().unwrap_or("")))
            .build();
        let icon = gtk::Image::builder().pixel_size(32).build();
        match app.icon() {
            Some(i) => icon.set_from_gicon(&i),
            None => icon.set_icon_name(Some("application-x-executable")),
        }
        row.add_prefix(&icon);
        list.append(&row);
    }
    list.set_header_func(move |row, _| {
        let title = match row.index() {
            0 if n_recommended > 0 => "Recommended apps",
            i if i == n_recommended => "Other apps",
            _ => {
                row.set_header(None::<&gtk::Widget>);
                return;
            }
        };
        let label = gtk::Label::builder()
            .label(title)
            .xalign(0.0)
            .margin_top(12)
            .margin_bottom(6)
            .margin_start(6)
            .css_classes(["heading"])
            .build();
        row.set_header(Some(&label));
    });
    if let Some(row) = list.row_at_index(0) {
        list.select_row(Some(&row));
    }

    let search = gtk::SearchEntry::builder().placeholder_text("Search apps").build();
    let query = Rc::new(RefCell::new(String::new()));
    let (q, a) = (query.clone(), apps.clone());
    list.set_filter_func(move |row| {
        let q = q.borrow();
        q.is_empty() || a[row.index() as usize].display_name().to_lowercase().contains(q.as_str())
    });
    let l = list.clone();
    search.connect_search_changed(move |s| {
        *query.borrow_mut() = s.text().to_lowercase();
        l.invalidate_filter();
        l.invalidate_headers();
    });

    let custom = adw::EntryRow::builder().title("Or run a custom command (e.g. kitty nvim %f)").build();
    let custom_group = adw::PreferencesGroup::new();
    custom_group.add(&custom);

    let always = gtk::CheckButton::with_label(&format!("Always use this app for {desc} files"));
    let scope = gtk::DropDown::from_strings(&["in Vaultiq only", "as the system default"]);
    scope.set_sensitive(false);
    always.bind_property("active", &scope, "sensitive").build();
    let always_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    always_box.append(&always);
    always_box.append(&scope);

    let what = if infos.len() == 1 {
        format!("Choose an app to open “{}”", first.display_name())
    } else {
        format!("Choose an app to open {} files", infos.len())
    };
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_start(18)
        .margin_end(18)
        .margin_top(12)
        .margin_bottom(18)
        .build();
    body.append(&gtk::Label::builder().label(&what).xalign(0.0).wrap(true).build());
    body.append(&search);
    body.append(
        &gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build(),
    );
    body.append(&custom_group);
    body.append(&always_box);

    let cancel = gtk::Button::with_label("Cancel");
    let open = gtk::Button::builder().label("Open").css_classes(["suggested-action"]).build();
    let header = adw::HeaderBar::builder().show_end_title_buttons(false).show_start_title_buttons(false).build();
    header.pack_start(&cancel);
    header.pack_end(&open);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));

    let dialog = adw::Dialog::builder()
        .title("Open with")
        .content_width(520)
        .content_height(640)
        .child(&toolbar)
        .default_widget(&open)
        .build();

    let d = dialog.clone();
    cancel.connect_clicked(move |_| {
        d.close();
    });
    let o = open.clone();
    list.connect_row_activated(move |_, _| {
        o.emit_clicked();
    });
    let d = dialog.clone();
    let parent_widget: gtk::Widget = parent.as_ref().clone();
    open.connect_clicked(move |_| {
        let cmd = custom.text().trim().to_string();
        let (app, handler) = if !cmd.is_empty() {
            match command_app(&cmd) {
                Ok(a) => (a, Handler::Command { command: cmd }),
                Err(e) => {
                    notify(&format!("Invalid command: {}", e.message()));
                    return;
                }
            }
        } else {
            let Some(row) = list.selected_row() else { return };
            let app = apps[row.index() as usize].clone();
            let id = app.id().map(|s| s.to_string()).unwrap_or_default();
            (app, Handler::App { app: id })
        };
        if always.is_active() {
            if scope.selected() == 0 {
                let key = ctype.clone();
                config::update(|c| {
                    c.open_with.overrides.insert(key, handler);
                });
            } else if let Err(e) = app.set_as_default_for_type(&ctype) {
                notify(&format!("Couldn't set default app: {}", e.message()));
            }
        }
        let files: Vec<gio::File> = infos.iter().map(util::file_of).collect();
        if let Err(e) = launch(&app, &files, &parent_widget) {
            notify(&format!("Couldn't start {}: {}", app.display_name(), e.message()));
        }
        d.close();
    });
    dialog.present(Some(parent.as_ref()));
}
