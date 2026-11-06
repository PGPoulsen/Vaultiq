//! Cut/copy/paste of files through the system clipboard, using the same
//! formats as GNOME Files, Dolphin and Thunar so pasting works across apps.

use gtk::{gdk, gio, glib, prelude::*};

const GNOME_FILES: &str = "x-special/gnome-copied-files";

pub fn set(widget: &impl IsA<gtk::Widget>, files: &[gio::File], cut: bool) {
    let uris: Vec<String> = files.iter().map(|f| f.uri().to_string()).collect();
    let gnome = format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n"));
    let text = files
        .iter()
        .map(|f| f.path().map(|p| p.display().to_string()).unwrap_or_else(|| f.uri().to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    let providers = [
        gdk::ContentProvider::for_bytes(GNOME_FILES, &glib::Bytes::from_owned(gnome.into_bytes())),
        gdk::ContentProvider::for_value(&gdk::FileList::from_array(files).to_value()),
        gdk::ContentProvider::for_bytes("text/plain;charset=utf-8", &glib::Bytes::from_owned(text.into_bytes())),
    ];
    let _ = widget.as_ref().clipboard().set_content(Some(&gdk::ContentProvider::new_union(&providers)));
}

pub fn set_text(widget: &impl IsA<gtk::Widget>, text: &str) {
    widget.as_ref().clipboard().set_text(text);
}

pub fn clear(widget: &impl IsA<gtk::Widget>) {
    let _ = widget.as_ref().clipboard().set_content(None::<&gdk::ContentProvider>);
}

pub fn has_files(widget: &impl IsA<gtk::Widget>) -> bool {
    let formats = widget.as_ref().clipboard().formats();
    formats.contain_mime_type(GNOME_FILES) || formats.contains_type(gdk::FileList::static_type())
}

/// Files on the clipboard and whether they were cut (vs copied).
pub async fn get(widget: &impl IsA<gtk::Widget>) -> Option<(Vec<gio::File>, bool)> {
    let clipboard = widget.as_ref().clipboard();
    let formats = clipboard.formats();
    if formats.contain_mime_type(GNOME_FILES) {
        let (stream, _) = clipboard.read_future(&[GNOME_FILES], glib::Priority::DEFAULT).await.ok()?;
        let out = gio::MemoryOutputStream::new_resizable();
        out.splice_future(
            &stream,
            gio::OutputStreamSpliceFlags::CLOSE_SOURCE | gio::OutputStreamSpliceFlags::CLOSE_TARGET,
            glib::Priority::DEFAULT,
        )
        .await
        .ok()?;
        let bytes = out.steal_as_bytes();
        let text = String::from_utf8_lossy(&bytes);
        let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        let cut = lines.next()? == "cut";
        let files: Vec<gio::File> = lines.map(gio::File::for_uri).collect();
        return (!files.is_empty()).then_some((files, cut));
    }
    if formats.contains_type(gdk::FileList::static_type()) {
        let value = clipboard.read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT).await.ok()?;
        let files = value.get::<gdk::FileList>().ok()?.files();
        return (!files.is_empty()).then_some((files, false));
    }
    None
}
