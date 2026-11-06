//! Thumbnails for the icon view, using the freedesktop thumbnail cache
//! (`~/.cache/thumbnails/normal`) so they are shared with other file managers.

use crate::{preview, util};
use gtk::gdk_pixbuf::Pixbuf;
use gtk::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SIZE: i32 = 128;
const MEMORY_CACHE_LIMIT: usize = 1500;

type Waiter = (glib::WeakRef<gtk::Image>, glib::WeakRef<glib::Object>, gio::FileInfo);

thread_local! {
    static CACHE: RefCell<HashMap<String, gdk::Texture>> = RefCell::new(HashMap::new());
    static PENDING: RefCell<HashMap<String, Vec<Waiter>>> = RefCell::new(HashMap::new());
}

fn wants_thumbnail(ctype: &str) -> bool {
    (ctype.starts_with("image/") && ctype != "image/x-icon") || ctype == "application/pdf"
}

/// Show the file's icon in `image`, replacing it with a thumbnail once one
/// is available. `item` is the list item, used to detect recycled widgets.
pub fn apply(image: &gtk::Image, info: &gio::FileInfo, item: &glib::Object) {
    match info.icon() {
        Some(icon) => image.set_from_gicon(&icon),
        None => image.set_icon_name(Some("text-x-generic")),
    }
    let ctype = util::content_type(info);
    let file = util::file_of(info);
    let Some(path) = file.path().filter(|_| wants_thumbnail(&ctype)) else { return };
    let mtime = info.modification_date_time().map(|d| d.to_unix()).unwrap_or(0);
    let uri = file.uri().to_string();
    let key = format!("{uri}@{mtime}");

    if let Some(tex) = CACHE.with(|c| c.borrow().get(&key).cloned()) {
        image.set_paintable(Some(&tex));
        return;
    }
    let waiter = (image.downgrade(), item.downgrade(), info.clone());
    let first = PENDING.with(|p| {
        let mut p = p.borrow_mut();
        let list = p.entry(key.clone()).or_default();
        list.push(waiter);
        list.len() == 1
    });
    if !first {
        return;
    }
    let cached = info
        .boolean("thumbnail::is-valid")
        .then(|| info.attribute_byte_string("thumbnail::path"))
        .flatten()
        .map(PathBuf::from);
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || load_or_generate(&path, &uri, mtime, cached, &ctype))
            .await
            .ok()
            .flatten();
        let waiters = PENDING.with(|p| p.borrow_mut().remove(&key)).unwrap_or_default();
        let Some(tex) = result else { return };
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() > MEMORY_CACHE_LIMIT {
                c.clear();
            }
            c.insert(key, tex.clone());
        });
        for (image, item, info) in waiters {
            let (Some(image), Some(item)) = (image.upgrade(), item.upgrade()) else { continue };
            // The row may have been recycled for another file meanwhile.
            if crate::views::item_info(&item).as_ref() == Some(&info) {
                image.set_paintable(Some(&tex));
            }
        }
    });
}

fn cache_path(uri: &str) -> PathBuf {
    let md5 = glib::compute_checksum_for_string(glib::ChecksumType::Md5, uri).unwrap();
    glib::user_cache_dir().join("thumbnails").join("normal").join(format!("{md5}.png"))
}

fn load_or_generate(path: &Path, uri: &str, mtime: i64, cached: Option<PathBuf>, ctype: &str) -> Option<gdk::Texture> {
    if let Some(pb) = cached.and_then(|p| Pixbuf::from_file(p).ok()) {
        return Some(texture_from_pixbuf(&pb));
    }
    let thumb = cache_path(uri);
    if let Ok(pb) = Pixbuf::from_file(&thumb)
        && pb.option("tEXt::Thumb::MTime").is_some_and(|m| m == mtime.to_string())
    {
        return Some(texture_from_pixbuf(&pb));
    }
    if ctype == "application/pdf" {
        return preview::render_pdf_page(path, 0, SIZE as f64, SIZE as f64).ok().map(|(t, _)| t);
    }
    let (_, w, h) = Pixbuf::file_info(path)?;
    let pb = if w <= SIZE && h <= SIZE {
        Pixbuf::from_file(path).ok()?
    } else {
        Pixbuf::from_file_at_scale(path, SIZE, SIZE, true).ok()?
    };
    let pb = pb.apply_embedded_orientation().unwrap_or(pb);
    // Don't litter the shared cache with thumbnails of files inside it.
    if !path.starts_with(glib::user_cache_dir().join("thumbnails")) {
        save_thumbnail(&pb, &thumb, uri, mtime);
    }
    Some(texture_from_pixbuf(&pb))
}

fn save_thumbnail(pb: &Pixbuf, thumb: &Path, uri: &str, mtime: i64) {
    use std::os::unix::fs::DirBuilderExt;
    let dir = thumb.parent().unwrap();
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    let tmp = thumb.with_extension(format!("png.{}.tmp", std::process::id()));
    let mtime = mtime.to_string();
    let opts = [("tEXt::Thumb::URI", uri), ("tEXt::Thumb::MTime", mtime.as_str())];
    if pb.savev(&tmp, "png", &opts).is_ok() {
        let _ = std::fs::rename(&tmp, thumb);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Convert a pixbuf into a texture without the deprecated GdkPixbuf path;
/// safe to call from worker threads.
pub fn texture_from_pixbuf(pb: &Pixbuf) -> gdk::Texture {
    let format = if pb.has_alpha() { gdk::MemoryFormat::R8g8b8a8 } else { gdk::MemoryFormat::R8g8b8 };
    gdk::MemoryTexture::new(pb.width(), pb.height(), format, &pb.read_pixel_bytes(), pb.rowstride() as usize)
        .upcast()
}
