//! Properties dialog for one or more items.

use crate::openwith::{self, Notify};
use crate::util;
use adw::prelude::*;
use gtk::{gio, glib};
use std::path::PathBuf;

const ATTRS: &str = "standard::*,time::*,unix::mode,owner::user,owner::group";

fn row(group: &adw::PreferencesGroup, title: &str, value: &str) -> adw::ActionRow {
    let r = adw::ActionRow::builder()
        .title(title)
        .subtitle(glib::markup_escape_text(value))
        .subtitle_selectable(true)
        .css_classes(["property"])
        .build();
    group.add(&r);
    r
}

pub fn mode_string(mode: u32) -> String {
    let kind = match mode & 0o170000 {
        0o040000 => 'd',
        0o120000 => 'l',
        _ => '-',
    };
    let bits: String = (0..9)
        .map(|i| {
            let set = mode & (0o400 >> i) != 0;
            match (set, i % 3) {
                (false, _) => '-',
                (true, 0) => 'r',
                (true, 1) => 'w',
                (true, _) => 'x',
            }
        })
        .collect();
    format!("{kind}{bits}  ({:o})", mode & 0o7777)
}

/// Recursively count bytes, files and folders.
fn measure(paths: Vec<PathBuf>) -> (u64, u64, u64) {
    let (mut bytes, mut files, mut dirs) = (0, 0, 0);
    for p in paths {
        for e in walkdir::WalkDir::new(p).follow_links(false).into_iter().flatten() {
            if e.file_type().is_dir() {
                dirs += 1;
            } else {
                files += 1;
                bytes += e.metadata().map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    (bytes, files, dirs)
}

pub fn show(parent: &gtk::Widget, infos: Vec<gio::FileInfo>, notify: Notify) {
    if infos.is_empty() {
        return;
    }
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::new();
    page.add(&group);
    let files: Vec<gio::File> = infos.iter().map(util::file_of).collect();
    let paths: Vec<PathBuf> = files.iter().filter_map(|f| f.path()).collect();
    let any_dir = infos.iter().any(util::is_dir);

    let size_row;
    if let [info] = infos.as_slice() {
        let file = &files[0];
        row(&group, "Name", &info.display_name());
        let ctype = util::content_type(info);
        row(&group, "Type", &format!("{} ({ctype})", util::type_description(info)));
        if let Some(parent) = file.parent().and_then(|p| p.path()) {
            row(&group, "Location", &parent.display().to_string());
        }
        size_row = row(&group, "Size", &format!("{} ({} bytes)", util::format_size(info.size() as u64), info.size()));

        let times = adw::PreferencesGroup::new();
        page.add(&times);
        let perms = adw::PreferencesGroup::new();
        page.add(&perms);
        let f = file.clone();
        glib::spawn_future_local(async move {
            let Ok(full) = f.query_info_future(ATTRS, gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS, glib::Priority::DEFAULT).await else { return };
            if full.is_symlink() {
                row(&times, "Link target", &full.symlink_target().map(|p| p.display().to_string()).unwrap_or_default());
            }
            row(&times, "Modified", &util::format_time(full.modification_date_time()));
            for (attr, label) in [("time::access", "Accessed"), ("time::created", "Created")] {
                if full.has_attribute(attr) {
                    let t = glib::DateTime::from_unix_utc(full.attribute_uint64(attr) as i64).ok();
                    row(&times, label, &util::format_time(t));
                }
            }
            if full.has_attribute("unix::mode") {
                row(&perms, "Permissions", &mode_string(full.attribute_uint32("unix::mode")));
            }
            let owner = full.attribute_string("owner::user").unwrap_or_default();
            let group = full.attribute_string("owner::group").unwrap_or_default();
            if !owner.is_empty() {
                row(&perms, "Owner", &format!("{owner} : {group}"));
            }
        });

        if !util::is_dir(info) {
            let apps = adw::PreferencesGroup::new();
            page.add(&apps);
            let opens = row(&apps, "Opens with", &openwith::default_app(info).map(|a| a.display_name().to_string()).unwrap_or_else(|| "No application".into()));
            let change = gtk::Button::builder().label("Change…").valign(gtk::Align::Center).build();
            let (p, i, n) = (parent.clone(), info.clone(), notify.clone());
            change.connect_clicked(move |_| openwith::choose(&p, vec![i.clone()], n.clone()));
            opens.add_suffix(&change);
        }
    } else {
        row(&group, "Selection", &format!("{} items", infos.len()));
        if let Some(parent) = files[0].parent().and_then(|p| p.path()) {
            row(&group, "Location", &parent.display().to_string());
        }
        let total: i64 = infos.iter().filter(|i| !util::is_dir(i)).map(|i| i.size()).sum();
        size_row = row(&group, "Size", &util::format_size(total as u64));
    }

    if any_dir {
        size_row.set_subtitle("Calculating…");
        let contains = row(&group, "Contains", "Calculating…");
        let single_dir = infos.len() == 1;
        glib::spawn_future_local(async move {
            let Ok((bytes, files, dirs)) = gio::spawn_blocking(move || measure(paths)).await else { return };
            // The top-level folder itself isn't "contained".
            let dirs = if single_dir { dirs.saturating_sub(1) } else { dirs };
            size_row.set_subtitle(&format!("{} ({bytes} bytes)", util::format_size(bytes)));
            let plural = |n: u64, one: &str, many: &str| format!("{n} {}", if n == 1 { one } else { many });
            contains.set_subtitle(&format!("{}, {}", plural(files, "file", "files"), plural(dirs, "folder", "folders")));
        });
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    let title = match infos.as_slice() {
        [one] => format!("{} Properties", one.display_name()),
        _ => "Properties".into(),
    };
    adw::Dialog::builder()
        .title(title)
        .content_width(460)
        .content_height(560)
        .child(&toolbar)
        .build()
        .present(Some(parent));
}

#[cfg(test)]
mod tests {
    #[test]
    fn mode_strings() {
        assert_eq!(super::mode_string(0o040755), "drwxr-xr-x  (755)");
        assert_eq!(super::mode_string(0o100640), "-rw-r-----  (640)");
    }
}
