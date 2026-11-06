//! Preferences: display options, terminal and file associations.

use crate::config::{self, ColorScheme, Handler, ViewMode};
use crate::openwith;
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

pub fn show(parent: &gtk::Widget, on_changed: Rc<dyn Fn()>) {
    let dialog = adw::PreferencesDialog::builder().title("Settings").search_enabled(false).build();
    dialog.add(&general_page(on_changed));
    dialog.add(&associations_page());
    dialog.present(Some(parent));
}

fn general_page(on_changed: Rc<dyn Fn()>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder().title("General").icon_name("preferences-system-symbolic").build();
    let display = adw::PreferencesGroup::builder().title("Display").build();
    page.add(&display);

    let hidden = adw::SwitchRow::builder().title("Show hidden files").active(config::with(|c| c.show_hidden)).build();
    let cb = on_changed.clone();
    hidden.connect_active_notify(move |r| {
        let v = r.is_active();
        config::update(|c| c.show_hidden = v);
        cb();
    });
    display.add(&hidden);

    let preview = adw::SwitchRow::builder().title("Show preview pane").active(config::with(|c| c.show_preview)).build();
    let cb = on_changed.clone();
    preview.connect_active_notify(move |r| {
        let v = r.is_active();
        config::update(|c| c.show_preview = v);
        cb();
    });
    display.add(&preview);

    let views = [ViewMode::Details, ViewMode::List, ViewMode::Icons];
    let view = adw::ComboRow::builder()
        .title("Default view for new tabs")
        .model(&gtk::StringList::new(&["Details", "List", "Icons"]))
        .selected(views.iter().position(|v| *v == config::with(|c| c.default_view)).unwrap_or(0) as u32)
        .build();
    view.connect_selected_notify(move |r| {
        let v = views[r.selected() as usize];
        config::update(|c| c.default_view = v);
    });
    display.add(&view);

    let schemes = ColorScheme::ALL;
    let appearance = adw::ComboRow::builder()
        .title("Appearance")
        .model(&gtk::StringList::new(&["Follow system", "Light", "Dark"]))
        .selected(schemes.iter().position(|s| *s == config::with(|c| c.color_scheme)).unwrap_or(0) as u32)
        .build();
    let cb = on_changed.clone();
    appearance.connect_selected_notify(move |r| {
        let s = schemes[r.selected() as usize];
        config::update(|c| c.color_scheme = s);
        cb();
    });
    display.add(&appearance);

    let term_group = adw::PreferencesGroup::builder()
        .title("Terminal")
        .description(format!(
            "Used by “Open terminal here”. Leave empty to auto-detect (currently: {}).",
            crate::window::detect_terminal().unwrap_or_else(|| "none found".into())
        ))
        .build();
    page.add(&term_group);
    let term = adw::EntryRow::builder().title("Terminal command").show_apply_button(true).build();
    term.set_text(&config::with(|c| c.terminal.clone()).unwrap_or_default());
    term.connect_apply(|r| {
        let t = r.text().trim().to_string();
        config::update(|c| c.terminal = (!t.is_empty()).then_some(t));
    });
    term_group.add(&term);
    page
}

fn associations_page() -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder()
        .title("File associations")
        .icon_name("application-x-executable-symbolic")
        .build();
    let rules = adw::PreferencesGroup::builder()
        .title("Vaultiq overrides")
        .description(
            "These take precedence over the system defaults (mimeapps.list) and only affect Vaultiq. \
             A key can be an extension (.log), a MIME type (text/plain) or a wildcard (image/*).",
        )
        .build();
    page.add(&rules);
    let rule_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Rc::default();
    fill_rules(&rules, &rule_rows);
    let refresh_now = move || fill_rules(&rules, &rule_rows);

    let add = adw::PreferencesGroup::builder().title("Add a rule").build();
    page.add(&add);
    let key = adw::EntryRow::builder().title("Extension or MIME type (e.g. .log)").build();
    let mut apps: Vec<gio::AppInfo> = gio::AppInfo::all().into_iter().filter(|a| a.should_show()).collect();
    apps.sort_by_key(|a| a.display_name().to_lowercase());
    let names: Vec<String> = std::iter::once("Custom command…".to_string())
        .chain(apps.iter().map(|a| a.display_name().to_string()))
        .collect();
    let app_row = adw::ComboRow::builder()
        .title("Application")
        .model(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>()))
        .enable_search(true)
        .build();
    let command = adw::EntryRow::builder().title("Custom command (e.g. kitty nvim %f)").build();
    app_row.bind_property("selected", &command, "visible").transform_to(|_, s: u32| Some(s == 0)).sync_create().build();
    let button = gtk::Button::builder()
        .label("Add rule")
        .halign(gtk::Align::End)
        .margin_top(12)
        .css_classes(["suggested-action", "pill"])
        .build();
    add.add(&key);
    add.add(&app_row);
    add.add(&command);
    add.add(&button);

    button.connect_clicked(move |_| {
        let k = key.text().trim().to_lowercase();
        if k.is_empty() || !(k.starts_with('.') || k.contains('/')) {
            key.add_css_class("error");
            return;
        }
        key.remove_css_class("error");
        let handler = match app_row.selected() {
            0 => {
                let cmd = command.text().trim().to_string();
                if cmd.is_empty() {
                    command.add_css_class("error");
                    return;
                }
                command.remove_css_class("error");
                Handler::Command { command: cmd }
            }
            i => Handler::App { app: apps[i as usize - 1].id().map(|s| s.to_string()).unwrap_or_default() },
        };
        config::update(|c| {
            c.open_with.overrides.insert(k, handler);
        });
        key.set_text("");
        command.set_text("");
        refresh_now();
    });
    page
}

fn fill_rules(group: &adw::PreferencesGroup, rows: &Rc<RefCell<Vec<adw::ActionRow>>>) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    let overrides = config::with(|c| c.open_with.overrides.clone());
    if overrides.is_empty() {
        let row = adw::ActionRow::builder().title("No overrides yet").css_classes(["dim-label"]).build();
        group.add(&row);
        rows.borrow_mut().push(row);
    }
    for (key, handler) in overrides {
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&key))
            .subtitle(glib::markup_escape_text(&openwith::describe_handler(&handler)))
            .build();
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let (g, r) = (group.downgrade(), Rc::downgrade(rows));
        remove.connect_clicked(move |_| {
            config::update(|c| {
                c.open_with.overrides.remove(&key);
            });
            if let (Some(g), Some(r)) = (g.upgrade(), r.upgrade()) {
                fill_rules(&g, &r);
            }
        });
        row.add_suffix(&remove);
        group.add(&row);
        rows.borrow_mut().push(row);
    }
}
