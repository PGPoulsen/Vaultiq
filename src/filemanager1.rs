//! The `org.freedesktop.FileManager1` D-Bus service, which browsers and
//! other apps call for "Show in folder" / "Open containing folder".

use crate::{properties, util, window::Explorer};
use gtk::{gio, glib, prelude::*};

const NAME: &str = "org.freedesktop.FileManager1";
const PATH: &str = "/org/freedesktop/FileManager1";
const XML: &str = r#"
<node>
  <interface name="org.freedesktop.FileManager1">
    <method name="ShowFolders">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItems">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
    <method name="ShowItemProperties">
      <arg type="as" name="URIs" direction="in"/>
      <arg type="s" name="StartupId" direction="in"/>
    </method>
  </interface>
</node>"#;

/// Export the interface on the app's bus connection and claim the name.
/// Only the primary instance calls this (from `startup`).
pub fn register(app: &adw::Application) {
    let Some(conn) = app.dbus_connection() else { return };
    let iface = gio::DBusNodeInfo::for_xml(XML).ok().and_then(|n| n.lookup_interface(NAME)).expect("valid interface XML");
    let a = app.clone();
    let registered = conn
        .register_object(PATH, &iface)
        .method_call(move |_, _, _, _, method, params, invocation| {
            match params.get::<(Vec<String>, String)>() {
                Some((uris, startup_id)) => {
                    let files: Vec<gio::File> = uris.iter().map(|u| gio::File::for_uri(u)).collect();
                    show(&a, method, files, &startup_id);
                    invocation.return_value(None);
                }
                None => invocation.return_error(gio::DBusError::InvalidArgs, "expected (as, s)"),
            }
        })
        .build();
    if let Err(e) = registered {
        eprintln!("vaultiq: can't export {NAME}: {}", e.message());
        return;
    }
    // Take the name over from another file manager if it allows that.
    let _ = gio::bus_own_name_on_connection(&conn, NAME, gio::BusNameOwnerFlags::REPLACE, |_, _| {}, |_, _| {});
}

fn show(app: &adw::Application, method: &str, files: Vec<gio::File>, startup_id: &str) {
    if files.is_empty() {
        return;
    }
    // ShowFolders opens the folders themselves; the others open each item's
    // folder with the item selected.
    let tabs: Vec<(gio::File, Option<String>)> = match method {
        "ShowFolders" => files.iter().map(|f| (f.clone(), None)).collect(),
        _ => files
            .iter()
            .map(|f| match (f.parent(), f.basename()) {
                (Some(parent), Some(name)) => (parent, Some(name.to_string_lossy().into_owned())),
                _ => (f.clone(), None),
            })
            .collect(),
    };
    let ex = Explorer::show(app, tabs, Some(startup_id));
    if method == "ShowItemProperties" {
        let infos = files.iter().filter_map(|f| f.path()).map(|p| util::info_from_path(&p, std::fs::metadata(&p).ok().as_ref())).collect();
        glib::idle_add_local_once(move || properties::show(ex.window.upcast_ref(), infos, ex.notifier()));
    }
}
