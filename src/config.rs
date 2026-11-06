//! Persistent settings stored in `~/.config/vaultiq/config.toml`.

use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// What to launch for a file type: an installed application (by desktop id)
/// or a custom command line (`%f`/`%F`/`%u`/`%U` field codes supported).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Handler {
    App { app: String },
    Command { command: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    #[default]
    Details,
    List,
    Icons,
}

impl ViewMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ViewMode::Details => "details",
            ViewMode::List => "list",
            ViewMode::Icons => "icons",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "details" => Some(ViewMode::Details),
            "list" => Some(ViewMode::List),
            "icons" => Some(ViewMode::Icons),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenWith {
    /// Keys are either an extension (`.log`, `.tar.gz`), a MIME type
    /// (`text/x-rust`) or a MIME wildcard (`image/*`).
    pub overrides: BTreeMap<String, Handler>,
}

impl OpenWith {
    /// Vaultiq-only override for a file. Extensions win over MIME types,
    /// longer extensions over shorter ones, exact MIME types over wildcards.
    pub fn resolve(&self, file_name: &str, content_type: &str) -> Option<&Handler> {
        let lower = file_name.to_lowercase();
        for (i, _) in lower.match_indices('.') {
            if i == 0 {
                continue; // ".bashrc" is a hidden file, not an extension
            }
            if let Some(h) = self.overrides.get(&lower[i..]) {
                return Some(h);
            }
        }
        if let Some(h) = self.overrides.get(content_type) {
            return Some(h);
        }
        let major = content_type.split('/').next()?;
        self.overrides.get(&format!("{major}/*"))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
    pub sidebar_width: i32,
    pub preview_width: i32,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1200,
            height: 760,
            maximized: false,
            sidebar_width: 240,
            preview_width: 340,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub show_hidden: bool,
    pub default_view: ViewMode,
    pub show_preview: bool,
    /// Terminal used by "Open terminal here"; auto-detected when unset.
    pub terminal: Option<String>,
    pub pinned: Vec<PathBuf>,
    pub open_with: OpenWith,
    pub window: WindowState,
    /// Details-view column widths set by the user, by column key
    /// (name, location, date, type, size).
    pub column_widths: BTreeMap<String, i32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            show_hidden: false,
            default_view: ViewMode::Details,
            show_preview: true,
            terminal: None,
            pinned: Vec::new(),
            open_with: OpenWith::default(),
            window: WindowState::default(),
            column_widths: BTreeMap::new(),
        }
    }
}

thread_local! {
    static CONFIG: RefCell<Config> = RefCell::new(Config::default());
    static PENDING_SAVE: RefCell<Option<gtk::glib::SourceId>> = const { RefCell::new(None) };
}

fn path() -> PathBuf {
    gtk::glib::user_config_dir().join("vaultiq").join("config.toml")
}

pub fn load() {
    let path = path();
    let cfg = match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("vaultiq: invalid {}: {e}; using defaults", path.display());
            let _ = std::fs::copy(&path, path.with_extension("toml.bak"));
            Config::default()
        }),
        Err(_) => Config::default(),
    };
    CONFIG.with(|c| *c.borrow_mut() = cfg);
}

pub fn with<R>(f: impl FnOnce(&Config) -> R) -> R {
    CONFIG.with(|c| f(&c.borrow()))
}

/// Mutate the configuration and write it to disk.
pub fn update(f: impl FnOnce(&mut Config)) {
    CONFIG.with(|c| f(&mut c.borrow_mut()));
    save();
}

/// Like `update`, but coalesces rapid changes (e.g. while dragging a
/// column edge) into one write shortly after the last one.
pub fn update_deferred(f: impl FnOnce(&mut Config)) {
    CONFIG.with(|c| f(&mut c.borrow_mut()));
    PENDING_SAVE.with(|p| {
        if let Some(id) = p.borrow_mut().take() {
            id.remove();
        }
        let id = gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(500), || {
            PENDING_SAVE.with(|p| p.borrow_mut().take());
            save();
        });
        *p.borrow_mut() = Some(id);
    });
}

fn save() {
    let text = CONFIG.with(|c| toml::to_string_pretty(&*c.borrow()));
    let path = path();
    match text {
        Ok(text) => {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            if let Err(e) = std::fs::write(&path, text) {
                eprintln!("vaultiq: cannot write {}: {e}", path.display());
            }
        }
        Err(e) => eprintln!("vaultiq: cannot serialize config: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pairs: &[(&str, &str)]) -> OpenWith {
        OpenWith {
            overrides: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), Handler::App { app: v.to_string() }))
                .collect(),
        }
    }

    fn app(h: Option<&Handler>) -> Option<&str> {
        match h {
            Some(Handler::App { app }) => Some(app),
            _ => None,
        }
    }

    #[test]
    fn resolve_prefers_longest_extension_then_mime_then_wildcard() {
        let ow = rules(&[
            (".gz", "gz"),
            (".tar.gz", "targz"),
            ("text/plain", "plain"),
            ("image/*", "img"),
        ]);
        assert_eq!(app(ow.resolve("a.tar.gz", "application/gzip")), Some("targz"));
        assert_eq!(app(ow.resolve("a.GZ", "application/gzip")), Some("gz"));
        assert_eq!(app(ow.resolve("notes.txt", "text/plain")), Some("plain"));
        assert_eq!(app(ow.resolve("x.png", "image/png")), Some("img"));
        assert_eq!(app(ow.resolve(".gz", "application/gzip")), None);
        assert_eq!(app(ow.resolve("a.rs", "text/x-rust")), None);
    }

    #[test]
    fn config_roundtrip() {
        let mut cfg = Config {
            show_hidden: true,
            default_view: ViewMode::Icons,
            pinned: vec!["/tmp".into()],
            column_widths: [("name".to_string(), 320), ("size".to_string(), 80)].into(),
            ..Default::default()
        };
        cfg.open_with.overrides.insert(
            ".log".into(),
            Handler::Command { command: "kitty less %f".into() },
        );
        cfg.open_with.overrides.insert("image/png".into(), Handler::App { app: "org.gnome.Loupe.desktop".into() });
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn partial_config_uses_defaults() {
        let cfg: Config = toml::from_str("show_hidden = true\n").unwrap();
        assert!(cfg.show_hidden);
        assert!(cfg.show_preview);
        assert_eq!(cfg.window, WindowState::default());
    }
}
