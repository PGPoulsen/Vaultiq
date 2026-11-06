//! Copy/move with progress, cancellation and conflict prompts, plus trash,
//! permanent delete, rename and "new item" helpers.

use crate::util;
use adw::prelude::*;
use gtk::{gio, glib};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transfer {
    Copy,
    Move,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Resolution {
    Replace,
    Skip,
    KeepBoth,
    Cancel,
}

enum Msg {
    Progress { done: u64, total: u64, name: String },
    Conflict { target: PathBuf, src_is_dir: bool, reply: async_channel::Sender<(Resolution, bool)> },
    Done { errors: Vec<String>, created: Vec<PathBuf> },
}

pub struct Outcome {
    pub errors: Vec<String>,
    /// Top-level items created in the destination.
    pub created: Vec<PathBuf>,
}

/// Panel at the bottom of the window listing running operations.
pub struct OpsPanel {
    pub widget: gtk::Revealer,
    list: gtk::Box,
}

impl OpsPanel {
    pub fn new() -> Rc<Self> {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let widget = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .child(&list)
            .build();
        Rc::new(OpsPanel { widget, list })
    }

    fn add_row(&self, cancel: Arc<AtomicBool>) -> (gtk::Box, gtk::Label, gtk::ProgressBar) {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let label = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        let bar = gtk::ProgressBar::builder().valign(gtk::Align::Center).width_request(220).build();
        let stop = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text("Cancel")
            .css_classes(["flat", "circular"])
            .build();
        stop.connect_clicked(move |b| {
            cancel.store(true, Ordering::Relaxed);
            b.set_sensitive(false);
        });
        row.append(&label);
        row.append(&bar);
        row.append(&stop);
        self.list.append(&row);
        self.widget.set_reveal_child(true);
        (row, label, bar)
    }

    fn remove_row(&self, row: &gtk::Box) {
        self.list.remove(row);
        if self.list.first_child().is_none() {
            self.widget.set_reveal_child(false);
        }
    }

    /// Copy or move `sources` into `dest`, then call `done` on the main thread.
    pub fn transfer(
        self: &Rc<Self>,
        parent: &gtk::Widget,
        kind: Transfer,
        sources: Vec<PathBuf>,
        dest: PathBuf,
        done: impl FnOnce(Outcome) + 'static,
    ) {
        let cancel = Arc::new(AtomicBool::new(false));
        let (row, label, bar) = self.add_row(cancel.clone());
        let verb = if kind == Transfer::Copy { "Copying" } else { "Moving" };
        label.set_text(&format!("{verb} {} item(s) to {}", sources.len(), dest.display()));

        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || {
            let mut job = Job::new(tx.clone(), cancel);
            job.run(kind, &sources, &dest);
            let _ = tx.send_blocking(Msg::Done { errors: job.errors, created: job.created });
        });

        let panel = Rc::downgrade(self);
        let parent = parent.clone();
        glib::spawn_future_local(async move {
            let mut done = Some(done);
            while let Ok(msg) = rx.recv().await {
                match msg {
                    Msg::Progress { done, total, name } => {
                        if total > 0 {
                            bar.set_fraction(done as f64 / total as f64);
                        }
                        label.set_text(&format!(
                            "{verb} “{name}” — {} of {}",
                            util::format_size(done),
                            util::format_size(total)
                        ));
                    }
                    Msg::Conflict { target, src_is_dir, reply } => {
                        let answer = ask_conflict(&parent, &target, src_is_dir).await;
                        let _ = reply.send(answer).await;
                    }
                    Msg::Done { errors, created } => {
                        if let Some(p) = panel.upgrade() {
                            p.remove_row(&row);
                        }
                        if let Some(f) = done.take() {
                            f(Outcome { errors, created });
                        }
                        break;
                    }
                }
            }
        });
    }
}

async fn ask_conflict(parent: &gtk::Widget, target: &Path, src_is_dir: bool) -> (Resolution, bool) {
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let dest_is_dir = target.is_dir();
    let merging = src_is_dir && dest_is_dir;
    let dialog = adw::AlertDialog::builder()
        .heading(format!("“{name}” already exists"))
        .body(if merging {
            "The destination already has a folder with this name. Merge the contents? Files with the same name will be replaced."
        } else {
            "The destination already has an item with this name."
        })
        .build();
    dialog.add_responses(&[
        ("cancel", "Cancel"),
        ("skip", "Skip"),
        ("keep", "Keep Both"),
        ("replace", if merging { "Merge" } else { "Replace" }),
    ]);
    dialog.set_response_appearance("replace", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("keep"));
    dialog.set_close_response("cancel");
    let all = gtk::CheckButton::with_label("Do this for all conflicts");
    dialog.set_extra_child(Some(&all));
    let res = match dialog.choose_future(Some(parent)).await.as_str() {
        "replace" => Resolution::Replace,
        "skip" => Resolution::Skip,
        "keep" => Resolution::KeepBoth,
        _ => Resolution::Cancel,
    };
    (res, all.is_active())
}

#[derive(Debug)]
struct Cancelled;

struct Job {
    tx: async_channel::Sender<Msg>,
    cancel: Arc<AtomicBool>,
    policy: Option<Resolution>,
    done: u64,
    total: u64,
    last_report: Instant,
    errors: Vec<String>,
    created: Vec<PathBuf>,
}

impl Job {
    fn new(tx: async_channel::Sender<Msg>, cancel: Arc<AtomicBool>) -> Self {
        Job {
            tx,
            cancel,
            policy: None,
            done: 0,
            total: 0,
            last_report: Instant::now() - Duration::from_secs(1),
            errors: Vec::new(),
            created: Vec::new(),
        }
    }

    fn cancelled(&self) -> Result<(), Cancelled> {
        if self.cancel.load(Ordering::Relaxed) { Err(Cancelled) } else { Ok(()) }
    }

    fn report(&mut self, name: &Path, force: bool) {
        if force || self.last_report.elapsed() > Duration::from_millis(100) {
            self.last_report = Instant::now();
            let name = name.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let _ = self.tx.send_blocking(Msg::Progress { done: self.done, total: self.total, name });
        }
    }

    fn error(&mut self, path: &Path, e: impl std::fmt::Display) {
        self.errors.push(format!("{}: {e}", path.display()));
    }

    fn resolve(&mut self, target: &Path, src_is_dir: bool) -> Resolution {
        if let Some(p) = self.policy {
            return p;
        }
        let (reply, answer) = async_channel::bounded(1);
        let msg = Msg::Conflict { target: target.to_path_buf(), src_is_dir, reply };
        if self.tx.send_blocking(msg).is_err() {
            return Resolution::Cancel;
        }
        let (res, all) = answer.recv_blocking().unwrap_or((Resolution::Cancel, false));
        if all {
            self.policy = Some(res);
        }
        res
    }

    fn run(&mut self, kind: Transfer, sources: &[PathBuf], dest: &Path) {
        self.total = sources.iter().map(|s| tree_size(s)).sum();
        for src in sources {
            if self.cancelled().is_err() {
                break;
            }
            let Some(name) = src.file_name() else { continue };
            if dest.starts_with(src) {
                self.error(src, "can't copy or move a folder into itself");
                continue;
            }
            let mut target = dest.join(name);
            let mut merge = false;
            if target == *src {
                if kind == Transfer::Move {
                    continue;
                }
                target = dest.join(util::keep_both_name(dest, &name.to_string_lossy()));
            } else if fs::symlink_metadata(&target).is_ok() {
                let src_is_dir = src.is_dir();
                match self.resolve(&target, src_is_dir) {
                    Resolution::Skip => continue,
                    Resolution::Cancel => break,
                    Resolution::KeepBoth => target = dest.join(util::keep_both_name(dest, &name.to_string_lossy())),
                    Resolution::Replace => {
                        if src_is_dir && target.is_dir() {
                            merge = true;
                        } else if let Err(e) = remove_path(&target) {
                            self.error(&target, e);
                            continue;
                        }
                    }
                }
            }
            let result = match kind {
                Transfer::Copy => self.copy_tree(src, &target),
                Transfer::Move => self.move_tree(src, &target, merge),
            };
            match result {
                Ok(()) => self.created.push(target),
                Err(Cancelled) => break,
            }
        }
        self.report(dest, true);
    }

    fn copy_tree(&mut self, src: &Path, dst: &Path) -> Result<(), Cancelled> {
        self.cancelled()?;
        let md = match fs::symlink_metadata(src) {
            Ok(m) => m,
            Err(e) => {
                self.error(src, e);
                return Ok(());
            }
        };
        if md.file_type().is_symlink() {
            let r = fs::read_link(src).and_then(|link| {
                let _ = fs::remove_file(dst);
                std::os::unix::fs::symlink(link, dst)
            });
            if let Err(e) = r {
                self.error(src, e);
            }
        } else if md.is_dir() {
            if let Err(e) = fs::create_dir_all(dst) {
                self.error(dst, e);
                return Ok(());
            }
            match fs::read_dir(src) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        self.copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
                    }
                }
                Err(e) => self.error(src, e),
            }
            let _ = fs::set_permissions(dst, md.permissions());
        } else {
            match self.copy_file(src, dst, &md) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    let _ = fs::remove_file(dst);
                    return Err(Cancelled);
                }
                Err(e) => self.error(src, e),
            }
        }
        Ok(())
    }

    fn copy_file(&mut self, src: &Path, dst: &Path, md: &fs::Metadata) -> io::Result<()> {
        let mut input = fs::File::open(src)?;
        let mut output = fs::File::create(dst)?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let n = input.read(&mut buf)?;
            if n == 0 {
                break;
            }
            output.write_all(&buf[..n])?;
            self.done += n as u64;
            self.report(src, false);
        }
        output.set_permissions(md.permissions())?;
        if let Ok(t) = md.modified() {
            let _ = output.set_modified(t);
        }
        Ok(())
    }

    fn move_tree(&mut self, src: &Path, dst: &Path, merge: bool) -> Result<(), Cancelled> {
        self.cancelled()?;
        if merge {
            if let Ok(entries) = fs::read_dir(src) {
                for entry in entries.flatten() {
                    let target = dst.join(entry.file_name());
                    let child_merge = entry.path().is_dir() && target.is_dir();
                    if !child_merge && fs::symlink_metadata(&target).is_ok() && let Err(e) = remove_path(&target) {
                        self.error(&target, e);
                        continue;
                    }
                    self.move_tree(&entry.path(), &target, child_merge)?;
                }
            }
            let _ = fs::remove_dir(src);
            return Ok(());
        }
        let size = tree_size(src);
        match fs::rename(src, dst) {
            Ok(()) => {
                self.done += size;
                self.report(src, false);
            }
            // Different filesystem: copy, then remove the original.
            Err(e) if e.raw_os_error() == Some(18) => {
                let errors_before = self.errors.len();
                self.copy_tree(src, dst)?;
                if self.errors.len() == errors_before && let Err(e) = remove_path(src) {
                    self.error(src, e);
                }
            }
            Err(e) => self.error(src, e),
        }
        Ok(())
    }
}

fn remove_path(p: &Path) -> io::Result<()> {
    let md = fs::symlink_metadata(p)?;
    if md.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}

fn tree_size(p: &Path) -> u64 {
    walkdir::WalkDir::new(p)
        .follow_links(false)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Move files to the trash. Returns error messages, plus the files that
/// failed only because their filesystem has no trash (e.g. tmpfs).
pub async fn trash(files: Vec<gio::File>) -> (Vec<String>, Vec<gio::File>) {
    let mut errors = Vec::new();
    let mut unsupported = Vec::new();
    for f in files {
        if let Err(e) = f.trash_future(glib::Priority::DEFAULT).await {
            if e.matches(gio::IOErrorEnum::NotSupported) {
                unsupported.push(f);
            } else {
                errors.push(format!("{}: {}", f.parse_name(), e.message()));
            }
        }
    }
    (errors, unsupported)
}

/// Permanently delete after confirmation; returns None when cancelled.
pub async fn delete_permanently(parent: &gtk::Widget, files: Vec<PathBuf>, body: &str) -> Option<Vec<String>> {
    let what = match files.as_slice() {
        [one] => format!("“{}”", one.file_name().unwrap_or_default().to_string_lossy()),
        many => format!("{} items", many.len()),
    };
    let dialog = adw::AlertDialog::builder()
        .heading(format!("Permanently delete {what}?"))
        .body(body)
        .build();
    dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    if dialog.choose_future(Some(parent)).await != "delete" {
        return None;
    }
    let errors = gio::spawn_blocking(move || {
        files
            .iter()
            .filter_map(|p| remove_path(p).err().map(|e| format!("{}: {e}", p.display())))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Some(errors)
}

/// Prompt for a file name. With `select_stem`, the extension is left
/// unselected so typing replaces just the name, as in Explorer.
pub async fn ask_name(parent: &gtk::Widget, heading: &str, initial: &str, confirm: &str, select_stem: bool) -> Option<String> {
    let entry = gtk::Entry::builder().text(initial).activates_default(true).build();
    let dialog = adw::AlertDialog::builder().heading(heading).extra_child(&entry).build();
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", confirm)]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    let d = dialog.clone();
    entry.connect_changed(move |e| {
        let t = e.text();
        d.set_response_enabled("ok", !t.trim().is_empty() && !t.contains('/') && t != "." && t != "..");
    });
    let (stem, ext) = util::split_ext(initial);
    let stem_len = if select_stem && !ext.is_empty() { stem.chars().count() as i32 } else { -1 };
    // GtkEntry selects everything when it gains focus, so apply the
    // stem-only selection afterwards, once.
    let focus = gtk::EventControllerFocus::new();
    let done = std::cell::Cell::new(false);
    focus.connect_enter(move |c| {
        if done.replace(true) {
            return;
        }
        let e = c.widget().and_downcast::<gtk::Entry>().unwrap();
        glib::idle_add_local_once(move || e.select_region(0, stem_len));
    });
    entry.add_controller(focus);
    entry.connect_map(|e| {
        e.grab_focus();
    });
    // The response arrives while the dialog is still closing; when it has
    // closed it restores keyboard focus to whatever had it before. Wait for
    // that, so callers that select/focus the new item afterwards win.
    let (closed_tx, closed_rx) = async_channel::bounded::<()>(1);
    dialog.connect_closed(move |_| {
        let _ = closed_tx.try_send(());
    });
    let ok = dialog.choose_future(Some(parent)).await == "ok";
    let _ = closed_rx.recv().await;
    ok.then(|| entry.text().trim().to_string())
}
