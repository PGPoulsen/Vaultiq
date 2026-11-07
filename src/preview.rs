//! Preview pane: images, text and PDF, with an info card for everything
//! else. Content is loaded on worker threads; a generation counter drops
//! results that arrive after the selection has moved on.

use crate::thumbnails::texture_from_pixbuf;
use crate::{archive, util};
use gtk::gdk_pixbuf::Pixbuf;
use gtk::{cairo, gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const TEXT_LIMIT: usize = 256 * 1024;
const IMAGE_MAX: i32 = 2048;
const PDF_MAX: (f64, f64) = (1200.0, 1600.0);
/// Larger archive members aren't extracted just to preview them.
const MAX_EXTRACT: i64 = 100 * 1024 * 1024;

#[derive(Default)]
struct State {
    generation: u64,
    /// (path, current page, page count)
    pdf: Option<(PathBuf, i32, i32)>,
}

pub struct Preview {
    pub widget: gtk::Box,
    stack: gtk::Stack,
    picture: gtk::Picture,
    text: gtk::TextView,
    pdf_picture: gtk::Picture,
    pdf_label: gtk::Label,
    pdf_prev: gtk::Button,
    pdf_next: gtk::Button,
    message: adw::StatusPage,
    title: gtk::Label,
    details: gtk::Label,
    state: RefCell<State>,
}

impl Preview {
    pub fn new() -> Rc<Self> {
        let stack = gtk::Stack::builder().vexpand(true).transition_type(gtk::StackTransitionType::Crossfade).build();

        let message = adw::StatusPage::builder()
            .description("Select a file to preview.")
            .build();
        message.add_css_class("compact");
        stack.add_named(&message, Some("message"));

        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .margin_start(8)
            .margin_end(8)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        stack.add_named(&picture, Some("image"));

        let text = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::None)
            .left_margin(8)
            .right_margin(8)
            .top_margin(8)
            .bottom_margin(8)
            .build();
        stack.add_named(&gtk::ScrolledWindow::builder().child(&text).build(), Some("text"));

        let pdf_picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .vexpand(true)
            .build();
        let pdf_prev = gtk::Button::from_icon_name("go-previous-symbolic");
        let pdf_next = gtk::Button::from_icon_name("go-next-symbolic");
        let pdf_label = gtk::Label::new(None);
        let nav = gtk::CenterBox::builder().margin_top(4).build();
        let nav_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        nav_box.append(&pdf_prev);
        nav_box.append(&pdf_label);
        nav_box.append(&pdf_next);
        nav.set_center_widget(Some(&nav_box));
        let pdf_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(8)
            .margin_end(8)
            .margin_top(8)
            .build();
        pdf_box.append(&pdf_picture);
        pdf_box.append(&nav);
        stack.add_named(&pdf_box, Some("pdf"));

        let title = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .selectable(true)
            .css_classes(["heading"])
            .build();
        let details = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build();
        let footer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_start(12)
            .margin_end(12)
            .margin_top(8)
            .margin_bottom(12)
            .build();
        footer.append(&title);
        footer.append(&details);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("vq-preview");
        widget.append(&stack);
        widget.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        widget.append(&footer);

        let p = Rc::new(Preview {
            widget,
            stack,
            picture,
            text,
            pdf_picture,
            pdf_label,
            pdf_prev,
            pdf_next,
            message,
            title,
            details,
            state: RefCell::default(),
        });
        let w = Rc::downgrade(&p);
        p.pdf_prev.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.pdf_step(-1)
            }
        });
        let w = Rc::downgrade(&p);
        p.pdf_next.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.pdf_step(1)
            }
        });
        p.show(&[]);
        p
    }

    fn next_generation(&self) -> u64 {
        let mut st = self.state.borrow_mut();
        st.generation += 1;
        st.pdf = None;
        st.generation
    }

    fn current(&self, generation: u64) -> bool {
        self.state.borrow().generation == generation
    }

    fn show_message(&self, icon: &str, text: &str) {
        self.message.set_icon_name((!icon.is_empty()).then_some(icon));
        self.message.set_description(Some(&glib::markup_escape_text(text)));
        self.stack.set_visible_child_name("message");
    }

    pub fn show(self: &Rc<Self>, infos: &[gio::FileInfo]) {
        let generation = self.next_generation();
        match infos {
            [] => {
                self.title.set_text("");
                self.details.set_text("");
                self.show_message("", "Select a file to preview.");
            }
            [info] => self.show_one(info, generation),
            many => {
                let size: i64 = many.iter().filter(|i| !util::is_dir(i)).map(|i| i.size()).sum();
                self.title.set_text(&format!("{} items selected", many.len()));
                self.details.set_text(&format!("Total size of files: {}", util::format_size(size as u64)));
                self.show_message("edit-select-all-symbolic", "");
            }
        }
    }

    fn show_one(self: &Rc<Self>, info: &gio::FileInfo, generation: u64) {
        let ctype = util::content_type(info);
        let mut details = vec![util::type_description(info)];
        if !util::is_dir(info) {
            details.push(util::format_size(info.size() as u64));
        }
        details.push(format!("Modified {}", util::format_time(info.modification_date_time())));
        self.title.set_text(&info.display_name());
        self.details.set_text(&details.join("\n"));

        let icon_name = |info: &gio::FileInfo| {
            info.icon()
                .and_downcast::<gio::ThemedIcon>()
                .and_then(|t| t.names().first().map(|s| s.to_string()))
                .unwrap_or_else(|| "text-x-generic".into())
        };
        if util::is_dir(info) {
            self.show_message("folder", "");
            return;
        }
        let Some(path) = util::file_of(info).path() else {
            // Inside an archive: extract it, then preview the copy.
            if let Some((archive, inner)) = archive::parts(&util::file_of(info))
                && info.size() <= MAX_EXTRACT
            {
                self.show_message(&icon_name(info), "");
                let (w, info) = (Rc::downgrade(self), info.clone());
                glib::spawn_future_local(async move {
                    let res = gio::spawn_blocking(move || archive::extract_member(&archive, &inner)).await;
                    let Some(p) = w.upgrade().filter(|p| p.current(generation)) else { return };
                    match res {
                        Ok(Ok(local)) => {
                            let info = info.dup();
                            info.set_attribute_object("standard::file", &gio::File::for_path(local));
                            p.show_one(&info, generation);
                        }
                        Ok(Err(e)) => p.show_message("dialog-warning-symbolic", &e),
                        Err(_) => {}
                    }
                });
                return;
            }
            self.show_message(&icon_name(info), "No preview available.");
            return;
        };
        let size = info.size();
        let is_text = gio::content_type_is_a(&ctype, "text/plain")
            || matches!(
                ctype.as_str(),
                "application/json" | "application/xml" | "application/toml" | "application/x-yaml"
                    | "application/javascript" | "application/x-shellscript" | "application/x-desktop"
                    | "application/sql" | "inode/x-empty"
            )
            || (ctype == "application/octet-stream" && size < 4 * 1024 * 1024);

        if ctype.starts_with("image/") {
            self.load_image(path, generation);
        } else if ctype == "application/pdf" {
            self.state.borrow_mut().pdf = Some((path, 0, 1));
            self.load_pdf_page(generation);
        } else if is_text {
            self.load_text(path, generation, icon_name(info));
        } else {
            self.show_message(&icon_name(info), "No preview available.");
        }
    }

    fn load_image(self: &Rc<Self>, path: PathBuf, generation: u64) {
        let p = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || -> Result<(gdk::Texture, i32, i32), String> {
                let (_, w, h) = Pixbuf::file_info(&path).ok_or("Unsupported image format")?;
                let pb = if w > IMAGE_MAX || h > IMAGE_MAX {
                    Pixbuf::from_file_at_scale(&path, IMAGE_MAX, IMAGE_MAX, true)
                } else {
                    Pixbuf::from_file(&path)
                }
                .map_err(|e| e.to_string())?;
                let pb = pb.apply_embedded_orientation().unwrap_or(pb);
                Ok((texture_from_pixbuf(&pb), w, h))
            })
            .await;
            let Some(p) = p.upgrade().filter(|p| p.current(generation)) else { return };
            match res {
                Ok(Ok((tex, w, h))) => {
                    p.picture.set_paintable(Some(&tex));
                    p.stack.set_visible_child_name("image");
                    let d = p.details.text();
                    p.details.set_text(&format!("{w} × {h} pixels\n{d}"));
                }
                Ok(Err(e)) => p.show_message("image-missing", &e),
                Err(_) => p.show_message("image-missing", "Could not load image."),
            }
        });
    }

    fn load_text(self: &Rc<Self>, path: PathBuf, generation: u64, icon: String) {
        let p = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || -> std::io::Result<(Vec<u8>, bool)> {
                let mut buf = Vec::with_capacity(TEXT_LIMIT.min(64 * 1024));
                let f = std::fs::File::open(&path)?;
                f.take(TEXT_LIMIT as u64 + 1).read_to_end(&mut buf)?;
                let truncated = buf.len() > TEXT_LIMIT;
                buf.truncate(TEXT_LIMIT);
                Ok((buf, truncated))
            })
            .await;
            let Some(p) = p.upgrade().filter(|p| p.current(generation)) else { return };
            match res {
                Ok(Ok((data, truncated))) => {
                    if util::looks_binary(&data) {
                        p.show_message(&icon, "Binary file — no text preview.");
                        return;
                    }
                    let mut text = String::from_utf8_lossy(&data).into_owned();
                    if truncated {
                        text.push_str("\n\n… (preview truncated)");
                    }
                    p.text.buffer().set_text(&text);
                    p.stack.set_visible_child_name("text");
                }
                Ok(Err(e)) => p.show_message("dialog-error-symbolic", &e.to_string()),
                Err(_) => p.show_message("dialog-error-symbolic", "Could not read file."),
            }
        });
    }

    fn pdf_step(self: &Rc<Self>, delta: i32) {
        let generation = {
            let mut st = self.state.borrow_mut();
            let Some((_, page, n)) = st.pdf.as_mut() else { return };
            let new = (*page + delta).clamp(0, *n - 1);
            if new == *page {
                return;
            }
            *page = new;
            st.generation += 1;
            st.generation
        };
        self.load_pdf_page(generation);
    }

    fn load_pdf_page(self: &Rc<Self>, generation: u64) {
        let Some((path, page, _)) = self.state.borrow().pdf.clone() else { return };
        let p = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || render_pdf_page(&path, page, PDF_MAX.0, PDF_MAX.1)).await;
            let Some(p) = p.upgrade().filter(|p| p.current(generation)) else { return };
            match res {
                Ok(Ok((tex, n))) => {
                    if let Some(pdf) = p.state.borrow_mut().pdf.as_mut() {
                        pdf.2 = n;
                    }
                    p.pdf_picture.set_paintable(Some(&tex));
                    p.pdf_label.set_text(&format!("Page {} of {}", page + 1, n));
                    p.pdf_prev.set_sensitive(page > 0);
                    p.pdf_next.set_sensitive(page + 1 < n);
                    p.stack.set_visible_child_name("pdf");
                }
                Ok(Err(e)) => p.show_message("x-office-document", &e),
                Err(_) => p.show_message("x-office-document", "Could not render PDF."),
            }
        });
    }
}

/// Render one PDF page to a texture fitting within `max_w`×`max_h`.
/// Returns the texture and the document's page count. Thread-safe: the
/// poppler document never leaves the calling thread.
pub fn render_pdf_page(path: &Path, page: i32, max_w: f64, max_h: f64) -> Result<(gdk::Texture, i32), String> {
    let uri = gio::File::for_path(path).uri();
    let doc = poppler::Document::from_file(&uri, None).map_err(|e| e.to_string())?;
    let n = doc.n_pages();
    let pg = doc.page(page).ok_or("Page not found")?;
    let (pw, ph) = pg.size();
    let scale = (max_w / pw).min(max_h / ph);
    let (w, h) = ((pw * scale).ceil() as i32, (ph * scale).ceil() as i32);
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w.max(1), h.max(1)).map_err(|e| e.to_string())?;
    {
        let cr = cairo::Context::new(&surface).map_err(|e| e.to_string())?;
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.paint().map_err(|e| e.to_string())?;
        cr.scale(scale, scale);
        pg.render(&cr);
    }
    surface.flush();
    let stride = surface.stride() as usize;
    let (w, h) = (surface.width(), surface.height());
    let data = surface.data().map_err(|e| e.to_string())?.to_vec();
    // Cairo's ARGB32 is native-endian, i.e. BGRA in memory on little-endian.
    let format = if cfg!(target_endian = "little") {
        gdk::MemoryFormat::B8g8r8a8Premultiplied
    } else {
        gdk::MemoryFormat::A8r8g8b8Premultiplied
    };
    let tex = gdk::MemoryTexture::new(w, h, format, &glib::Bytes::from_owned(data), stride);
    Ok((tex.upcast(), n))
}
