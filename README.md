# Vaultiq

A tabbed file explorer in the style of Windows Explorer, for Linux / Wayland
(developed on Hyprland). Rust + GTK 4 + libadwaita.

- Tabs (Ctrl+T / Ctrl+W, middle-click a folder to open it in a new tab)
- Quick access (home, XDG folders, pinned folders), drives with mount/eject,
  and a folder tree that follows the current location
- Details / List / Icons views (icons show thumbnails from the shared
  freedesktop thumbnail cache), sortable columns, folders first, natural sort
- Preview pane: images, text (first 256 KB), PDF (with paging), info card
- Open, **Open with ▸**, "Choose another app…" with *Always use* — either
  as the system default (`~/.config/mimeapps.list`) or as a Vaultiq-only
  override, plus custom commands (`kitty nvim %f`)
- Cut/copy/paste (interoperates with other file managers), drag and drop,
  rename, trash, permanent delete, new folder/document, progress panel with
  cancel, conflict prompts (replace / merge / keep both / skip)
- Editable address bar (click the breadcrumbs or Ctrl+L; Tab completes),
  recursive search (`*`/`?` wildcards), properties dialog, settings

## Build and run

Requires GTK ≥ 4.20, libadwaita ≥ 1.8, poppler-glib and a Rust toolchain.

```sh
cargo run --release              # opens your home folder
cargo run --release -- ~/Downloads /etc   # one tab per argument
```

Install for your user and make it the default folder handler:

```sh
cargo build --release
install -Dm755 target/release/vaultiq ~/.local/bin/vaultiq
install -Dm644 data/dk.accuratech.Vaultiq.desktop ~/.local/share/applications/dk.accuratech.Vaultiq.desktop
xdg-mime default dk.accuratech.Vaultiq.desktop inode/directory
```

On Hyprland you can bind it, e.g. in `hyprland.lua` / your binds config, to
run `vaultiq`.

## Configuration

`~/.config/vaultiq/config.toml` is written by the app (Settings, Ctrl+,) but
can be edited by hand:

```toml
show_hidden = false
default_view = "details"      # details | list | icons
show_preview = true
terminal = "kitty"            # used by "Open terminal here"; auto-detected if unset
pinned = ["/home/me/Projects"]

# Vaultiq-only "open with" overrides. Keys: extension, MIME type or wildcard.
# Extensions win over MIME types; these win over the system defaults.
[open_with.overrides]
".log" = { command = "kitty less %f" }
"image/*" = { app = "org.gnome.Loupe.desktop" }
"text/x-rust" = { app = "codium.desktop" }
```

## Keyboard shortcuts

| Action | Keys |
|---|---|
| Back / Forward / Up | Alt+← / Alt+→ (Backspace, mouse buttons) / Alt+↑ |
| Edit location / Search | Ctrl+L or Alt+D / Ctrl+F or F3 |
| New tab / close tab / new window | Ctrl+T / Ctrl+W / Ctrl+N |
| Cut / Copy / Paste | Ctrl+X / Ctrl+C / Ctrl+V |
| Rename / Trash / Delete permanently | F2 / Delete / Shift+Delete |
| New folder / Copy path / Properties | Ctrl+Shift+N / Ctrl+Shift+C / Alt+Enter |
| Context menu | Shift+F10 or Menu |
| Details / List / Icons | Ctrl+1 / Ctrl+2 / Ctrl+3 |
| Hidden files / Preview pane / Refresh | Ctrl+H / Alt+P / F5 |
| Open terminal here | Ctrl+Alt+T |

While dragging: hold Ctrl to copy, Shift to move (default: move on the same
drive, copy across drives).

## Code layout

| File | Purpose |
|---|---|
| `src/window.rs` | Window layout, actions, context menus, tab ↔ chrome sync |
| `src/tab.rs` | One tab: model chain, views, history, selection, search |
| `src/views.rs` | Cell factories + per-item gestures, drag & drop |
| `src/tree.rs`, `src/sidebar.rs`, `src/addressbar.rs` | Navigation widgets |
| `src/preview.rs`, `src/thumbnails.rs` | Preview pane and icon thumbnails |
| `src/openwith.rs` | App resolution, launching, chooser dialog |
| `src/fileops.rs`, `src/clipboard.rs` | Copy/move worker, trash, rename, clipboard |
| `src/properties.rs`, `src/settings.rs` | Dialogs |
| `src/config.rs`, `src/util.rs`, `src/search.rs` | Config, helpers, search worker |

`cargo test` covers the config/override resolution, history, natural sort,
glob matching, binary detection and naming helpers.
