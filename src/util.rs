//! Small helpers shared across modules: file-info access, formatting,
//! name matching and path handling.

use gtk::{gio, glib, prelude::*};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

/// Attributes queried for every listed file.
pub const ATTRS: &str = "standard::name,standard::display-name,standard::type,\
standard::size,standard::icon,standard::symbolic-icon,standard::content-type,standard::is-hidden,\
standard::is-backup,standard::is-symlink,time::modified,thumbnail::path,thumbnail::is-valid";

pub fn file_of(info: &gio::FileInfo) -> gio::File {
    info.attribute_object("standard::file")
        .and_downcast::<gio::File>()
        .unwrap_or_else(|| gio::File::for_path(info.name()))
}

pub fn is_dir(info: &gio::FileInfo) -> bool {
    info.file_type() == gio::FileType::Directory
}

pub fn is_hidden(info: &gio::FileInfo) -> bool {
    info.is_hidden() || info.is_backup()
}

pub fn content_type(info: &gio::FileInfo) -> String {
    info.content_type()
        .map(|c| c.to_string())
        .unwrap_or_else(|| "application/octet-stream".into())
}

pub fn type_description(info: &gio::FileInfo) -> String {
    if is_dir(info) {
        return "File folder".into();
    }
    gio::content_type_get_description(&content_type(info)).to_string()
}

pub fn format_size(bytes: u64) -> String {
    glib::format_size(bytes).to_string()
}

pub fn format_time(dt: Option<glib::DateTime>) -> String {
    dt.and_then(|d| d.to_local().ok())
        .and_then(|d| d.format("%Y-%m-%d %H:%M").ok())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// Human name of a location for tab titles: "Home", "/" or the folder name.
pub fn location_name(file: &gio::File) -> String {
    match file.path() {
        Some(p) if p == glib::home_dir() => "Home".into(),
        Some(p) if p == Path::new("/") => "File System".into(),
        _ => file
            .basename()
            .map(|b| b.to_string_lossy().into_owned())
            .unwrap_or_else(|| file.uri().to_string()),
    }
}

/// Build a `FileInfo` for a path found outside a `DirectoryList`
/// (search results, tree roots).
pub fn info_from_path(path: &Path, md: Option<&std::fs::Metadata>) -> gio::FileInfo {
    let info = gio::FileInfo::new();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    info.set_name(path.file_name().map(Path::new).unwrap_or(path));
    info.set_display_name(&name);
    info.set_is_hidden(name.starts_with('.'));
    info.set_attribute_boolean("standard::is-backup", name.ends_with('~'));
    let is_dir = md.map(|m| m.is_dir()).unwrap_or_else(|| path.is_dir());
    let ctype = if is_dir {
        "inode/directory".to_string()
    } else {
        gio::content_type_guess(Some(&name), None::<&[u8]>).0.to_string()
    };
    info.set_file_type(if is_dir { gio::FileType::Directory } else { gio::FileType::Regular });
    info.set_content_type(&ctype);
    info.set_icon(&gio::content_type_get_icon(&ctype));
    info.set_symbolic_icon(&gio::content_type_get_symbolic_icon(&ctype));
    if let Some(md) = md {
        info.set_size(md.len() as i64);
        if let Ok(t) = md.modified()
            && let Ok(d) = t.duration_since(std::time::UNIX_EPOCH)
            && let Ok(dt) = glib::DateTime::from_unix_utc(d.as_secs() as i64)
        {
            info.set_modification_date_time(&dt);
        }
    }
    info.set_attribute_object("standard::file", &gio::File::for_path(path));
    info
}

/// Case-insensitive comparison that orders "file2" before "file10".
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (na, nb) = (take(&mut a), take(&mut b));
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                a.next();
                b.next();
            }
        }
    }
}

/// Case-insensitive glob match supporting `*` and `?`.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// Search matching: glob when the query has wildcards, otherwise substring.
pub fn matches_query(query: &str, name: &str) -> bool {
    if query.contains(['*', '?']) {
        glob_match(query, name)
    } else {
        name.to_lowercase().contains(&query.to_lowercase())
    }
}

/// Heuristic: NUL bytes or many control characters mean "not text".
pub fn looks_binary(data: &[u8]) -> bool {
    let sample = &data[..data.len().min(8192)];
    if sample.contains(&0) {
        return true;
    }
    let control = sample
        .iter()
        .filter(|&&b| b < 0x20 && !matches!(b, b'\n' | b'\r' | b'\t' | 0x0c | 0x1b))
        .count();
    !sample.is_empty() && control * 10 > sample.len()
}

/// "name.txt" -> "name - Copy.txt", "name - Copy (2).txt", ... (first free one).
pub fn keep_both_name(dir: &Path, name: &str) -> String {
    let (stem, ext) = split_ext(name);
    (1..)
        .map(|i| {
            if i == 1 {
                format!("{stem} - Copy{ext}")
            } else {
                format!("{stem} - Copy ({i}){ext}")
            }
        })
        .find(|n| !dir.join(n).exists())
        .unwrap()
}

/// "New folder", "New folder (2)", ...
pub fn unique_name(dir: &Path, base: &str) -> String {
    let (stem, ext) = split_ext(base);
    (1..)
        .map(|i| if i == 1 { base.to_string() } else { format!("{stem} ({i}){ext}") })
        .find(|n| !dir.join(n).exists())
        .unwrap()
}

/// Split "archive.tar.gz" into ("archive.tar", ".gz"); dotfiles have no extension.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Interpret text typed into the address bar.
pub fn expand_path(text: &str, cwd: &Path) -> gio::File {
    let text = text.trim();
    if text.contains("://") {
        return gio::File::for_uri(text);
    }
    let path = if text == "~" {
        glib::home_dir()
    } else if let Some(rest) = text.strip_prefix("~/") {
        glib::home_dir().join(rest)
    } else {
        let p = PathBuf::from(text);
        if p.is_absolute() { p } else { cwd.join(p) }
    };
    gio::File::for_path(path)
}

/// Back/forward navigation stacks.
#[derive(Debug)]
pub struct History<T> {
    back: Vec<T>,
    forward: Vec<T>,
}

impl<T> Default for History<T> {
    fn default() -> Self {
        Self { back: Vec::new(), forward: Vec::new() }
    }
}

impl<T> History<T> {
    /// Record `current` before navigating somewhere new.
    pub fn visit(&mut self, current: T) {
        self.back.push(current);
        self.forward.clear();
    }

    pub fn back(&mut self, current: T) -> Option<T> {
        let prev = self.back.pop()?;
        self.forward.push(current);
        Some(prev)
    }

    pub fn forward(&mut self, current: T) -> Option<T> {
        let next = self.forward.pop()?;
        self.back.push(current);
        Some(next)
    }

    pub fn can_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_ordering() {
        let mut v = vec!["file10", "File2", "file1", "file02b", "alpha", "Beta"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["alpha", "Beta", "file1", "File2", "file02b", "file10"]);
    }

    #[test]
    fn globbing() {
        assert!(glob_match("*.RS", "main.rs"));
        assert!(glob_match("ma?n.*", "main.rs"));
        assert!(glob_match("*a*b*", "xxaYYbzz"));
        assert!(!glob_match("*.rs", "main.rs.bak"));
        assert!(!glob_match("?", ""));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn query_matching() {
        assert!(matches_query("READ", "readme.md"));
        assert!(matches_query("*.md", "README.MD"));
        assert!(!matches_query("*.md", "readme.txt"));
    }

    #[test]
    fn binary_detection() {
        assert!(!looks_binary(b"fn main() {\n\tprintln!(\"hi\");\n}\n"));
        assert!(!looks_binary("blåbærgrød æøå".as_bytes()));
        assert!(looks_binary(b"\x7fELF\x02\x01\x01\0\0\0"));
        assert!(looks_binary(&[1u8, 2, 3, 4, 5, 6, 7, 8, b'a']));
        assert!(!looks_binary(b""));
    }

    #[test]
    fn extension_split() {
        assert_eq!(split_ext("a.tar.gz"), ("a.tar", ".gz"));
        assert_eq!(split_ext(".bashrc"), (".bashrc", ""));
        assert_eq!(split_ext("Makefile"), ("Makefile", ""));
    }

    #[test]
    fn unique_names() {
        let dir = std::env::temp_dir().join(format!("vaultiq-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        std::fs::write(dir.join("a - Copy.txt"), "").unwrap();
        assert_eq!(keep_both_name(&dir, "a.txt"), "a - Copy (2).txt");
        assert_eq!(unique_name(&dir, "a.txt"), "a (2).txt");
        assert_eq!(unique_name(&dir, "New folder"), "New folder");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn history_stacks() {
        let mut h = History::default();
        h.visit(1);
        h.visit(2); // now at 3
        assert_eq!(h.back(3), Some(2));
        assert_eq!(h.back(2), Some(1));
        assert_eq!(h.back(1), None);
        assert_eq!(h.forward(1), Some(2));
        h.visit(2); // navigating away clears forward
        assert!(!h.can_forward());
        assert!(h.can_back());
    }

    #[test]
    fn path_expansion() {
        let cwd = Path::new("/usr");
        assert_eq!(expand_path("share", cwd).path().unwrap(), Path::new("/usr/share"));
        assert_eq!(expand_path("/etc ", cwd).path().unwrap(), Path::new("/etc"));
        assert_eq!(expand_path("~", cwd).path().unwrap(), glib::home_dir());
    }
}
