//! Browsing inside archives (zip, tar, 7z, rar…) with libarchive.
//!
//! A place inside an archive is a `gio::File` of our own type (URI scheme
//! `vq-archive`), so listings, tabs, history, breadcrumbs and "Up" treat it
//! like any folder. Archives are read-only. When something needs a real
//! file (preview, opening in an app, copying out), the member is extracted
//! to a cache folder first.

use crate::util;
use gtk::{gio, glib, prelude::*, subclass::prelude::*};
use std::collections::HashMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEME: &str = "vq-archive";

/// Content types opened as folders.
const TYPES: &[&str] = &[
    "application/zip",
    "application/x-zip-compressed",
    "application/x-7z-compressed",
    "application/vnd.rar",
    "application/x-rar",
    "application/x-rar-compressed",
    "application/x-tar",
    "application/x-compressed-tar",
    "application/x-bzip-compressed-tar",
    "application/x-bzip2-compressed-tar",
    "application/x-xz-compressed-tar",
    "application/x-zstd-compressed-tar",
    "application/x-lzma-compressed-tar",
    "application/x-lz4-compressed-tar",
    "application/x-lzip-compressed-tar",
    "application/x-cpio",
    "application/vnd.ms-cab-compressed",
];

pub fn is_archive(content_type: &str) -> bool {
    TYPES.contains(&content_type)
}

pub fn is_archive_info(info: &gio::FileInfo) -> bool {
    !util::is_dir(info) && is_archive(&util::content_type(info))
}

/// Register the URI scheme, so `gio::File::for_uri` (clipboard, drag and
/// drop) gives back archive members.
pub fn register() {
    let from_uri = |_: &gio::Vfs, uri: &str| parse_uri(uri);
    gio::Vfs::default().register_uri_scheme(SCHEME, Some(Box::new(from_uri)), Some(Box::new(from_uri)));
}

/// The root folder of the archive at `path`.
pub fn root(path: &Path) -> gio::File {
    ArchiveFile::create(path.to_path_buf(), String::new())
}

/// (archive path, path inside it) of an archive member.
pub fn parts(file: &gio::File) -> Option<(PathBuf, String)> {
    file.downcast_ref::<ArchiveFile>().map(|f| f.loc().clone())
}

pub fn is_member(file: &gio::File) -> bool {
    file.is::<ArchiveFile>()
}

/// The real folder that contains the archive a member belongs to.
pub fn host_dir(file: &gio::File) -> Option<PathBuf> {
    parts(file).and_then(|(archive, _)| archive.parent().map(Path::to_path_buf))
}

/// Where to go to open `file`: archives become their root folder, and a path
/// that runs through an archive ("/x/a.zip/docs") becomes that member.
pub fn browsable(file: gio::File) -> gio::File {
    let Some(path) = file.path() else { return file };
    if path.is_file() && is_archive_file(&path) {
        return root(&path);
    }
    if !path.exists() {
        for dir in path.ancestors().skip(1) {
            if dir.is_file() {
                if is_archive_file(dir)
                    && let Ok(rest) = path.strip_prefix(dir)
                {
                    return ArchiveFile::create(dir.to_path_buf(), rest.to_string_lossy().into_owned());
                }
                break;
            }
        }
    }
    file
}

fn is_archive_file(path: &Path) -> bool {
    gio::File::for_path(path)
        .query_info("standard::content-type", gio::FileQueryInfoFlags::NONE, None::<&gio::Cancellable>)
        .ok()
        .and_then(|i| i.content_type())
        .is_some_and(|t| is_archive(&t))
}

/// A folder `file` can be shown as: itself, or the root of an archive. A
/// nested archive is extracted first, so this may finish later.
pub fn browse(file: gio::File, done: impl FnOnce(Result<gio::File, String>) + 'static) {
    match (parts(&file), file.path()) {
        (Some(loc), _) if !loc.1.is_empty() && member_is_archive(&loc) => {
            glib::spawn_future_local(async move {
                let res = gio::spawn_blocking(move || extract_member(&loc.0, &loc.1)).await;
                done(res.unwrap_or_else(|_| Err("Extraction failed".into())).map(|p| root(&p)));
            });
        }
        (None, Some(path)) if path.is_file() => done(Ok(root(&path))),
        _ => done(Ok(file)),
    }
}

fn member_is_archive((archive, inner): &(PathBuf, String)) -> bool {
    index(archive).ok().and_then(|ix| ix.entries.get(inner).map(|e| !e.dir)).unwrap_or(false)
        && is_archive(&gio::content_type_guess(Some(inner.as_str()), None::<&[u8]>).0)
}

/// A real path for `file`: itself if it's local, otherwise the member
/// extracted to the cache. Blocking.
pub fn local_path(file: &gio::File) -> Result<PathBuf, String> {
    match parts(file) {
        Some((archive, inner)) => extract_member(&archive, &inner),
        None => file.path().ok_or_else(|| format!("“{}” isn't a local file", file.uri())),
    }
}

/// Extract one member (a file or a whole folder) to the cache. Blocking.
pub fn extract_member(archive: &Path, inner: &str) -> Result<PathBuf, String> {
    let dest = cache_dir(archive)?;
    let target = dest.join(inner);
    let ix = index(archive)?;
    let entry = ix.entries.get(inner).ok_or_else(|| format!("“{inner}” isn't in the archive"))?;
    if !entry.dir && std::fs::metadata(&target).is_ok_and(|m| m.len() == entry.size) {
        return Ok(target);
    }
    let prefix = format!("{inner}/");
    extract(archive, &dest, false, |p| (p == inner || p.starts_with(&prefix)).then(|| PathBuf::from(p)))?;
    if entry.dir {
        // Implied folders have no entry of their own.
        std::fs::create_dir_all(&target).map_err(|e| e.to_string())?;
    } else if !target.exists() {
        return Err(format!("Couldn't extract “{inner}”"));
    }
    Ok(target)
}

/// Extract a whole archive into `into` like "Extract here": an archive with a
/// single top folder gives that folder, anything else goes into a new folder
/// named after the archive. Returns what was created. Blocking.
pub fn extract_here(archive: &Path, into: &Path) -> Result<PathBuf, String> {
    let ix = index(archive)?;
    let name = archive.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let top: Vec<&String> = ix.children.get("").into_iter().flatten().collect();
    let (folder, strip) = match top.as_slice() {
        [one] if ix.entries[*one].dir => (util::unique_name(into, one), Some(format!("{one}/"))),
        _ => (util::unique_name(into, archive_stem(&name)), None),
    };
    // Written straight into the final folder: a temporary folder that comes
    // and goes in the open folder would stall GtkDirectoryList (see tab.rs).
    let dest = into.join(folder);
    std::fs::create_dir(&dest).map_err(|e| format!("Can't create {}: {e}", dest.display()))?;
    let result = extract(archive, &dest, true, |p| match &strip {
        Some(top) => p.strip_prefix(top.as_str()).map(PathBuf::from),
        None => Some(PathBuf::from(p)),
    });
    match result {
        Ok(()) => Ok(dest),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest);
            Err(e)
        }
    }
}

/// "photos.tar.gz" → "photos".
pub fn archive_stem(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    for ext in [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lz", ".tar.lzma", ".tar.lz4", ".tar.z"] {
        if lower.ends_with(ext) && name.len() > ext.len() {
            return &name[..name.len() - ext.len()];
        }
    }
    util::split_ext(name).0
}

/// Delete cached extractions that haven't been touched for a day.
pub fn clean_cache() {
    std::thread::spawn(|| {
        let Ok(entries) = std::fs::read_dir(cache_root()) else { return };
        let old = SystemTime::now() - Duration::from_secs(24 * 3600);
        for e in entries.flatten() {
            if e.metadata().and_then(|m| m.modified()).is_ok_and(|t| t < old) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    });
}

fn cache_root() -> PathBuf {
    glib::user_cache_dir().join("vaultiq").join("archives")
}

/// A cache folder per archive version (path, size and mtime).
fn cache_dir(archive: &Path) -> Result<PathBuf, String> {
    use std::hash::{Hash, Hasher};
    let md = std::fs::metadata(archive).map_err(|e| e.to_string())?;
    let mut h = std::hash::DefaultHasher::new();
    (archive, md.len(), md.modified().ok()).hash(&mut h);
    let dir = cache_root().join(format!("{:016x}", h.finish()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let _ = std::fs::File::open(&dir).and_then(|f| f.set_modified(SystemTime::now()));
    Ok(dir)
}

// ---- listing ---------------------------------------------------------------

struct Entry {
    dir: bool,
    size: u64,
    mtime: i64,
}

struct Index {
    stamp: (u64, Option<SystemTime>),
    entries: HashMap<String, Entry>,
    /// Folder path ("" is the root) → names of its children.
    children: HashMap<String, Vec<String>>,
}

static INDEXES: LazyLock<Mutex<HashMap<PathBuf, Arc<Index>>>> = LazyLock::new(Mutex::default);

/// The archive's table of contents, read once and kept while the file is
/// unchanged.
fn index(archive: &Path) -> Result<Arc<Index>, String> {
    let md = std::fs::metadata(archive).map_err(|e| e.to_string())?;
    let stamp = (md.len(), md.modified().ok());
    if let Some(ix) = INDEXES.lock().unwrap().get(archive).filter(|ix| ix.stamp == stamp) {
        return Ok(ix.clone());
    }
    let mut ix = Index { stamp, entries: HashMap::new(), children: HashMap::new() };
    ix.entries.insert(String::new(), Entry { dir: true, size: 0, mtime: 0 });
    let mut reader = Reader::open(archive)?;
    while let Some(h) = reader.next()? {
        let Some(path) = h.path else { continue };
        // Folders often aren't stored, only implied by their files.
        let mut parent = String::new();
        for (i, comp) in path.split('/').enumerate() {
            let p = if i == 0 { comp.to_string() } else { format!("{parent}/{comp}") };
            let last = p.len() == path.len();
            if !ix.entries.contains_key(&p) {
                ix.children.entry(parent.clone()).or_default().push(comp.to_string());
                ix.entries.insert(p.clone(), Entry { dir: true, size: 0, mtime: 0 });
            }
            if last {
                ix.entries.insert(p.clone(), Entry { dir: h.kind == Kind::Dir, size: h.size, mtime: h.mtime });
            }
            parent = p;
        }
    }
    let ix = Arc::new(ix);
    INDEXES.lock().unwrap().insert(archive.to_path_buf(), ix.clone());
    Ok(ix)
}

fn entry_info(file: &gio::File, name: &str, e: &Entry) -> gio::FileInfo {
    let info = gio::FileInfo::new();
    info.set_name(name);
    info.set_display_name(name);
    info.set_is_hidden(name.starts_with('.'));
    info.set_attribute_boolean("standard::is-backup", name.ends_with('~'));
    let ctype = if e.dir { "inode/directory".to_string() } else { gio::content_type_guess(Some(name), None::<&[u8]>).0.to_string() };
    info.set_file_type(if e.dir { gio::FileType::Directory } else { gio::FileType::Regular });
    info.set_content_type(&ctype);
    info.set_icon(&gio::content_type_get_icon(&ctype));
    info.set_symbolic_icon(&gio::content_type_get_symbolic_icon(&ctype));
    info.set_size(e.size as i64);
    if e.mtime > 0
        && let Ok(dt) = glib::DateTime::from_unix_utc(e.mtime)
    {
        info.set_modification_date_time(&dt);
    }
    info.set_attribute_boolean("access::can-write", false);
    info.set_attribute_boolean("access::can-delete", false);
    info.set_attribute_boolean("access::can-rename", false);
    info.set_attribute_object("standard::file", file);
    info
}

fn io_error(kind: gio::IOErrorEnum, msg: &str) -> glib::Error {
    glib::Error::new(kind, msg)
}

// ---- extraction ------------------------------------------------------------

/// Extract the entries `select` maps to a relative destination into `dest`.
/// Symbolic links are created (last, so nothing is written through them)
/// only when `links` is set.
fn extract(archive: &Path, dest: &Path, links: bool, select: impl Fn(&str) -> Option<PathBuf>) -> Result<(), String> {
    let mut reader = Reader::open(archive)?;
    let mut symlinks = Vec::new();
    while let Some(h) = reader.next()? {
        let Some(rel) = h.path.as_deref().and_then(&select) else { continue };
        let target = dest.join(&rel);
        let mkparent = |t: &Path| std::fs::create_dir_all(t.parent().unwrap()).map_err(|e| format!("{}: {e}", t.display()));
        match h.kind {
            Kind::Dir => std::fs::create_dir_all(&target).map_err(|e| format!("{}: {e}", target.display()))?,
            Kind::Symlink => {
                if links && let Some(to) = h.link {
                    symlinks.push((target, to));
                }
            }
            Kind::File => {
                if h.encrypted {
                    return Err(format!("“{}” is password-protected, which isn't supported", h.path.unwrap_or_default()));
                }
                mkparent(&target)?;
                if let Some(link) = h.hardlink.as_deref().and_then(normalize).and_then(|l| select(&l)) {
                    std::fs::copy(dest.join(link), &target).map_err(|e| format!("{}: {e}", target.display()))?;
                    continue;
                }
                // Write next to the target and rename, so a half-written file
                // is never mistaken for a finished one.
                let tmp = target.with_file_name(format!(".{}.vq-part-{}", rel.file_name().unwrap().to_string_lossy(), std::process::id()));
                let written = (|| {
                    let mut out = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", target.display()))?;
                    reader.read_data(&mut out)?;
                    let _ = out.set_permissions(std::fs::Permissions::from_mode((h.perm & 0o777) | 0o600));
                    if h.mtime > 0 {
                        let _ = out.set_modified(UNIX_EPOCH + Duration::from_secs(h.mtime as u64));
                    }
                    std::fs::rename(&tmp, &target).map_err(|e| format!("{}: {e}", target.display()))
                })();
                if written.is_err() {
                    let _ = std::fs::remove_file(&tmp);
                }
                written?;
            }
        }
    }
    for (path, to) in symlinks {
        if path.symlink_metadata().is_err() {
            let _ = path.parent().map(std::fs::create_dir_all);
            let _ = std::os::unix::fs::symlink(to, path);
        }
    }
    Ok(())
}

/// "./a//b/" → "a/b". Rejects ".." and empty paths.
fn normalize(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => return None,
            c => parts.push(c),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

// ---- libarchive ------------------------------------------------------------

#[allow(non_camel_case_types)]
mod ffi {
    use std::ffi::{c_char, c_int, c_void};
    pub enum archive {}
    pub enum archive_entry {}
    pub const OK: c_int = 0;
    pub const EOF: c_int = 1;
    pub const WARN: c_int = -20;
    pub const IFMT: u32 = 0o170000;
    pub const IFDIR: u32 = 0o040000;
    pub const IFLNK: u32 = 0o120000;

    #[link(name = "archive")]
    unsafe extern "C" {
        pub fn archive_read_new() -> *mut archive;
        pub fn archive_read_support_filter_all(a: *mut archive) -> c_int;
        pub fn archive_read_support_format_all(a: *mut archive) -> c_int;
        pub fn archive_read_open_filename(a: *mut archive, name: *const c_char, block: usize) -> c_int;
        pub fn archive_read_next_header(a: *mut archive, e: *mut *mut archive_entry) -> c_int;
        pub fn archive_read_data(a: *mut archive, buf: *mut c_void, len: usize) -> isize;
        pub fn archive_read_free(a: *mut archive) -> c_int;
        pub fn archive_error_string(a: *mut archive) -> *const c_char;
        pub fn archive_entry_pathname(e: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_pathname_utf8(e: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_hardlink(e: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_symlink(e: *mut archive_entry) -> *const c_char;
        pub fn archive_entry_filetype(e: *mut archive_entry) -> u32;
        pub fn archive_entry_perm(e: *mut archive_entry) -> u32;
        pub fn archive_entry_size(e: *mut archive_entry) -> i64;
        pub fn archive_entry_mtime(e: *mut archive_entry) -> i64;
        pub fn archive_entry_is_encrypted(e: *mut archive_entry) -> c_int;
    }
}

#[derive(PartialEq)]
enum Kind {
    File,
    Dir,
    Symlink,
}

struct Header {
    /// Normalized; `None` for entries we skip (unsafe or empty paths).
    path: Option<String>,
    kind: Kind,
    size: u64,
    mtime: i64,
    perm: u32,
    link: Option<String>,
    hardlink: Option<String>,
    encrypted: bool,
}

/// A libarchive read handle positioned at one entry at a time.
struct Reader(*mut ffi::archive);

impl Reader {
    fn open(path: &Path) -> Result<Reader, String> {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
        unsafe {
            let r = Reader(ffi::archive_read_new());
            ffi::archive_read_support_filter_all(r.0);
            ffi::archive_read_support_format_all(r.0);
            if ffi::archive_read_open_filename(r.0, name.as_ptr(), 64 * 1024) != ffi::OK {
                return Err(r.error(path));
            }
            Ok(r)
        }
    }

    fn error(&self, path: &Path) -> String {
        let msg = unsafe { cstr(ffi::archive_error_string(self.0)) };
        format!("Can't read {}: {}", path.file_name().unwrap_or_default().to_string_lossy(), msg.unwrap_or_else(|| "unknown error".into()))
    }

    fn next(&mut self) -> Result<Option<Header>, String> {
        let mut e = std::ptr::null_mut();
        unsafe {
            match ffi::archive_read_next_header(self.0, &mut e) {
                ffi::EOF => Ok(None),
                ffi::OK | ffi::WARN => {
                    let raw = cstr(ffi::archive_entry_pathname_utf8(e)).or_else(|| cstr(ffi::archive_entry_pathname(e)));
                    let kind = match ffi::archive_entry_filetype(e) & ffi::IFMT {
                        ffi::IFDIR => Kind::Dir,
                        ffi::IFLNK => Kind::Symlink,
                        _ => Kind::File,
                    };
                    Ok(Some(Header {
                        path: raw.as_deref().and_then(normalize),
                        kind,
                        size: ffi::archive_entry_size(e).max(0) as u64,
                        mtime: ffi::archive_entry_mtime(e),
                        perm: ffi::archive_entry_perm(e),
                        link: cstr(ffi::archive_entry_symlink(e)),
                        hardlink: cstr(ffi::archive_entry_hardlink(e)),
                        encrypted: ffi::archive_entry_is_encrypted(e) != 0,
                    }))
                }
                _ => Err(self.error(Path::new("archive"))),
            }
        }
    }

    /// Copy the current entry's data to `out`.
    fn read_data(&mut self, out: &mut impl Write) -> Result<(), String> {
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = unsafe { ffi::archive_read_data(self.0, buf.as_mut_ptr().cast(), buf.len()) };
            if n == 0 {
                return Ok(());
            }
            if n < 0 && n as i32 != ffi::WARN {
                return Err(self.error(Path::new("archive")));
            }
            if n > 0 {
                out.write_all(&buf[..n as usize]).map_err(|e| e.to_string())?;
            }
        }
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        unsafe { ffi::archive_read_free(self.0) };
    }
}

unsafe fn cstr(p: *const std::ffi::c_char) -> Option<String> {
    (!p.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

// ---- the GFile -------------------------------------------------------------

fn parse_uri(uri: &str) -> Option<gio::File> {
    let rest = uri.strip_prefix(SCHEME)?.strip_prefix("://")?;
    let (archive, inner) = rest.split_once('/').unwrap_or((rest, ""));
    let archive = glib::Uri::unescape_string(archive, None::<&str>)?;
    let inner = glib::Uri::unescape_string(inner, None::<&str>)?;
    Some(ArchiveFile::create(PathBuf::from(archive.as_str()), normalize(&inner).unwrap_or_default()))
}

glib::wrapper! {
    pub struct ArchiveFile(ObjectSubclass<imp::ArchiveFile>) @implements gio::File;
}

impl ArchiveFile {
    fn create(archive: PathBuf, inner: String) -> gio::File {
        let f: ArchiveFile = glib::Object::new();
        let _ = f.imp().loc.set((archive, inner));
        f.upcast()
    }

    fn loc(&self) -> &(PathBuf, String) {
        self.imp().loc.get().expect("archive file without a location")
    }
}

glib::wrapper! {
    pub struct Enumerator(ObjectSubclass<imp::Enumerator>) @extends gio::FileEnumerator;
}

glib::wrapper! {
    /// Archives don't change under us; this monitor never reports anything.
    pub struct NoMonitor(ObjectSubclass<imp::NoMonitor>) @extends gio::FileMonitor;
}

mod imp {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::OnceLock;

    #[derive(Default)]
    pub struct ArchiveFile {
        pub loc: OnceLock<(PathBuf, String)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ArchiveFile {
        const NAME: &'static str = "VaultiqArchiveFile";
        type Type = super::ArchiveFile;
        type Interfaces = (gio::File,);
    }

    impl ObjectImpl for ArchiveFile {}

    impl ArchiveFile {
        fn loc(&self) -> (&Path, &str) {
            let (a, i) = self.loc.get().expect("archive file without a location");
            (a, i)
        }

        fn child(&self, name: &str) -> String {
            let (_, inner) = self.loc();
            if inner.is_empty() { name.to_string() } else { format!("{inner}/{name}") }
        }

        fn not_found(&self) -> glib::Error {
            io_error(gio::IOErrorEnum::NotFound, &format!("“{}” isn't in the archive", self.loc().1))
        }
    }

    impl FileImpl for ArchiveFile {
        const SUPPORT_THREAD_CONTEXT: bool = true;

        fn dup(&self) -> gio::File {
            let (a, i) = self.loc();
            super::ArchiveFile::create(a.to_path_buf(), i.to_string())
        }

        fn hash(&self) -> u32 {
            use std::hash::{Hash, Hasher};
            let mut h = std::hash::DefaultHasher::new();
            self.loc().hash(&mut h);
            h.finish() as u32
        }

        fn equal(&self, other: &gio::File) -> bool {
            other.downcast_ref::<super::ArchiveFile>().is_some_and(|o| o.imp().loc() == self.loc())
        }

        fn is_native(&self) -> bool {
            false
        }

        fn has_uri_scheme(&self, scheme: &str) -> bool {
            scheme == SCHEME
        }

        fn uri_scheme(&self) -> Option<String> {
            Some(SCHEME.into())
        }

        fn basename(&self) -> Option<PathBuf> {
            let (archive, inner) = self.loc();
            match inner.rsplit('/').next().filter(|s| !s.is_empty()) {
                Some(name) => Some(name.into()),
                None => archive.file_name().map(PathBuf::from),
            }
        }

        fn path(&self) -> Option<PathBuf> {
            None
        }

        fn uri(&self) -> String {
            let (archive, inner) = self.loc();
            let a = glib::Uri::escape_string(&archive.to_string_lossy(), None, true);
            let i = glib::Uri::escape_string(inner, Some("/"), true);
            format!("{SCHEME}://{a}/{i}")
        }

        fn parse_name(&self) -> String {
            let (archive, inner) = self.loc();
            if inner.is_empty() { archive.display().to_string() } else { format!("{}/{inner}", archive.display()) }
        }

        fn parent(&self) -> Option<gio::File> {
            let (archive, inner) = self.loc();
            if inner.is_empty() {
                return archive.parent().map(gio::File::for_path);
            }
            let up = inner.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
            Some(super::ArchiveFile::create(archive.to_path_buf(), up.to_string()))
        }

        fn has_prefix(&self, prefix: &gio::File) -> bool {
            let (archive, inner) = self.loc();
            match prefix.downcast_ref::<super::ArchiveFile>() {
                Some(p) => {
                    let (pa, pi) = p.imp().loc();
                    pa == archive && inner.len() > pi.len() && (pi.is_empty() || inner.starts_with(&format!("{pi}/")))
                }
                None => prefix.path().is_some_and(|p| archive.starts_with(p)),
            }
        }

        fn relative_path(&self, descendant: &gio::File) -> Option<PathBuf> {
            let d = descendant.downcast_ref::<super::ArchiveFile>()?;
            let (a, i) = self.loc();
            let (da, di) = d.imp().loc();
            if da != a {
                return None;
            }
            let rest = if i.is_empty() { di } else { di.strip_prefix(i)?.strip_prefix('/')? };
            (!rest.is_empty()).then(|| PathBuf::from(rest))
        }

        fn resolve_relative_path(&self, relative_path: impl AsRef<Path>) -> gio::File {
            let rel = relative_path.as_ref();
            if rel.is_absolute() {
                return gio::File::for_path(rel);
            }
            let (archive, inner) = self.loc();
            let mut parts: Vec<String> = inner.split('/').filter(|s| !s.is_empty()).map(String::from).collect();
            for comp in rel.to_string_lossy().split('/') {
                match comp {
                    "" | "." => {}
                    ".." => {
                        if parts.pop().is_none() {
                            // Above the archive root: back on the real disk.
                            return archive.parent().map(|p| gio::File::for_path(p).resolve_relative_path(rel)).unwrap_or_else(|| self.dup());
                        }
                    }
                    c => parts.push(c.to_string()),
                }
            }
            super::ArchiveFile::create(archive.to_path_buf(), parts.join("/"))
        }

        fn child_for_display_name(&self, display_name: &str) -> Result<gio::File, glib::Error> {
            Ok(self.resolve_relative_path(display_name))
        }

        fn enumerate_children(
            &self,
            _attributes: &str,
            _flags: gio::FileQueryInfoFlags,
            _cancellable: Option<&gio::Cancellable>,
        ) -> Result<gio::FileEnumerator, glib::Error> {
            let (archive, inner) = self.loc();
            let ix = index(archive).map_err(|e| io_error(gio::IOErrorEnum::Failed, &e))?;
            match ix.entries.get(inner) {
                Some(e) if e.dir => {}
                Some(_) => return Err(io_error(gio::IOErrorEnum::NotDirectory, "Not a folder")),
                None => return Err(self.not_found()),
            }
            let infos = ix
                .children
                .get(inner)
                .into_iter()
                .flatten()
                .map(|name| {
                    let path = self.child(name);
                    let file = super::ArchiveFile::create(archive.to_path_buf(), path.clone());
                    entry_info(&file, name, &ix.entries[&path])
                })
                .collect();
            let obj = self.obj();
            let en: super::Enumerator = glib::Object::builder().property("container", obj.upcast_ref::<gio::File>()).build();
            *en.imp().items.lock().unwrap() = infos;
            Ok(en.upcast())
        }

        fn query_info(
            &self,
            _attributes: &str,
            _flags: gio::FileQueryInfoFlags,
            _cancellable: Option<&gio::Cancellable>,
        ) -> Result<gio::FileInfo, glib::Error> {
            let (archive, inner) = self.loc();
            let ix = index(archive).map_err(|e| io_error(gio::IOErrorEnum::Failed, &e))?;
            let entry = ix.entries.get(inner).ok_or_else(|| self.not_found())?;
            let name = self.basename().unwrap_or_default().to_string_lossy().into_owned();
            Ok(entry_info(self.obj().upcast_ref(), &name, entry))
        }

        fn read_fn(&self, cancellable: Option<&gio::Cancellable>) -> Result<gio::FileInputStream, glib::Error> {
            let (archive, inner) = self.loc();
            let path = extract_member(archive, inner).map_err(|e| io_error(gio::IOErrorEnum::Failed, &e))?;
            gio::File::for_path(path).read(cancellable)
        }

        fn monitor_dir(
            &self,
            _flags: gio::FileMonitorFlags,
            _cancellable: Option<&gio::Cancellable>,
        ) -> Result<gio::FileMonitor, glib::Error> {
            Ok(glib::Object::new::<super::NoMonitor>().upcast())
        }

        fn monitor_file(
            &self,
            _flags: gio::FileMonitorFlags,
            _cancellable: Option<&gio::Cancellable>,
        ) -> Result<gio::FileMonitor, glib::Error> {
            Ok(glib::Object::new::<super::NoMonitor>().upcast())
        }
    }

    #[derive(Default)]
    pub struct Enumerator {
        pub items: Mutex<VecDeque<gio::FileInfo>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Enumerator {
        const NAME: &'static str = "VaultiqArchiveEnumerator";
        type Type = super::Enumerator;
        type ParentType = gio::FileEnumerator;
    }

    impl ObjectImpl for Enumerator {}

    impl FileEnumeratorImpl for Enumerator {
        fn next_file(&self, _cancellable: Option<&gio::Cancellable>) -> Result<Option<gio::FileInfo>, glib::Error> {
            Ok(self.items.lock().unwrap().pop_front())
        }

        fn close(&self, _cancellable: Option<&gio::Cancellable>) -> (bool, Option<glib::Error>) {
            (true, None)
        }
    }

    #[derive(Default)]
    pub struct NoMonitor;

    #[glib::object_subclass]
    impl ObjectSubclass for NoMonitor {
        const NAME: &'static str = "VaultiqArchiveMonitor";
        type Type = super::NoMonitor;
        type ParentType = gio::FileMonitor;
    }

    impl ObjectImpl for NoMonitor {}

    impl FileMonitorImpl for NoMonitor {
        fn cancel(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize("./a//b/").as_deref(), Some("a/b"));
        assert_eq!(normalize("/etc/passwd").as_deref(), Some("etc/passwd"));
        assert_eq!(normalize("a/../../b"), None);
        assert_eq!(normalize("./"), None);
    }

    #[test]
    fn stems() {
        assert_eq!(archive_stem("photos.tar.gz"), "photos");
        assert_eq!(archive_stem("Backup.TAR.XZ"), "Backup");
        assert_eq!(archive_stem("site.zip"), "site");
        assert_eq!(archive_stem("noext"), "noext");
    }
}
