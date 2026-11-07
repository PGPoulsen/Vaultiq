# Vaultiq

A tabbed file explorer in the style of Windows Explorer, for Linux / Wayland
(developed on Hyprland). Rust + GTK 4 + libadwaita.

- Tabs (Ctrl+T / Ctrl+W, middle-click a folder to open it in a new tab)
- Quick access (home, XDG folders, pinned folders), drives with mount/eject,
  and a folder tree that follows the current location
- Details / List / Icons views (icons show thumbnails from the shared
  freedesktop thumbnail cache), sortable columns, folders first, natural sort
- Preview pane: images, text (first 256 KB), PDF (with paging), info card
- Archives (zip, 7z, rar, tar.gz/bz2/xz/zst, cpio, cab) open like read-only
  folders: browse, preview, open files, copy or drag them out. "Extract
  here" unpacks next to the archive (into one folder, like Nautilus).
  Needs libarchive; extracted files are cached in `~/.cache/vaultiq/archives`
  for a day
- Open, **Open with ▸**, "Choose another app…" with *Always use* — either
  as the system default (`~/.config/mimeapps.list`) or as a Vaultiq-only
  override, plus custom commands (`kitty nvim %f`)
- Cut/copy/paste (interoperates with other file managers), drag and drop,
  rename, trash, permanent delete, new folder/document, progress panel with
  cancel, conflict prompts (replace / merge / keep both / skip)
- Editable address bar (click the breadcrumbs or Ctrl+L; Tab completes),
  recursive search (`*`/`?` wildcards), properties dialog, settings

## Build and run

Requires GTK ≥ 4.20, libadwaita ≥ 1.8, poppler-glib, libarchive and a Rust toolchain.

```sh
cargo run --release              # opens your home folder
cargo run --release -- ~/Downloads /etc   # one tab per argument
```

Install for your user and make it the default folder handler:

```sh
cargo build --release
install -Dm755 target/release/vaultiq ~/.local/bin/vaultiq
# Absolute Exec paths: ~/.local/bin is often not on the session's PATH
sed "s|^Exec=vaultiq|Exec=$HOME/.local/bin/vaultiq|" data/dk.accuratech.Vaultiq.desktop \
  > ~/.local/share/applications/dk.accuratech.Vaultiq.desktop
mkdir -p ~/.local/share/dbus-1/services
sed "s|@BINDIR@|$HOME/.local/bin|" data/org.freedesktop.FileManager1.service \
  > ~/.local/share/dbus-1/services/org.freedesktop.FileManager1.service
xdg-mime default dk.accuratech.Vaultiq.desktop inode/directory
# Optional: open archives in Vaultiq too
xdg-mime default dk.accuratech.Vaultiq.desktop $(grep -oP '(?<=MimeType=inode/directory;).*' data/dk.accuratech.Vaultiq.desktop | tr ';' ' ')
```

The D-Bus service makes Vaultiq the `org.freedesktop.FileManager1`
handler, which browsers use for "Show in folder" (it opens the folder with
the file selected). If another file manager is already running as that
service (e.g. `Thunar --gapplication-service`), quit it once so Vaultiq can
take over. While Vaultiq is running it also claims the name itself.

On Hyprland you can bind it, e.g. in `hyprland.lua` / your binds config, to
run `vaultiq`.

## Configuration

`~/.config/vaultiq/config.toml` is written by the app (Settings, Ctrl+,) but
can be edited by hand:

```toml
show_hidden = false
default_view = "details"      # details | list | icons
color_scheme = "system"       # system | light | dark  (also in Settings and the ☰ menu)
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

## Custom styling

Put GTK CSS in `~/.config/vaultiq/style.css`. It's applied on top of the
built-in styles and the libadwaita theme, and **reloads as soon as you save
it**, so you can tweak things while Vaultiq is running. Mistakes are printed
to the terminal with their location, e.g.
`vaultiq: style.css:2:15-19: No property named "colr"`. Delete the file to
go back to the defaults.

Classes Vaultiq uses:

| Selector | What it is |
|---|---|
| `.vq-sidebar` | Left pane (quick access, drives, folder tree) |
| `.vq-sidebar-header` | "Quick access", "Drives", "Folders" headings |
| `.vq-crumbs` | Breadcrumb address bar |
| `.vq-preview` | Preview pane |
| `.vq-statusbar` | Status bar at the bottom |
| `.vq-cell` | Every file entry in the details/list/icon views |
| `columnview.data-table` | Details view (`> header > button` are the column headers, `> listview > row > cell` the cells) |
| `gridview > child` | Items in the icon view |

Example:

```css
/* Bigger, roomier details view */
columnview.data-table > listview > row > cell {
  font-size: 1.1em;
  padding-top: 6px;
  padding-bottom: 6px;
}

/* Darker sidebar and a custom accent colour (libadwaita variables) */
.vq-sidebar { background-color: #1e1e2e; color: #cdd6f4; }
:root { --accent-bg-color: #e66100; --accent-color: #e66100; }
```

Run Vaultiq with `GTK_DEBUG=interactive` to open the GTK Inspector and find
the selector for any other widget.

## Keyboard shortcuts

| Action | Keys |
|---|---|
| Back / Forward / Up | Alt+← / Alt+→ (Backspace, mouse buttons) / Alt+↑ |
| Edit location / Search | Ctrl+L or Alt+D / Ctrl+F or F3 |
| New tab / close tab / new window | Ctrl+T / Ctrl+W / Ctrl+N |
| Move tab to a new window | Ctrl+Shift+T (or drag the tab out, or right-click it) |
| Cut / Copy / Paste | Ctrl+X / Ctrl+C / Ctrl+V |
| Rename / Trash / Delete permanently | F2 / Delete / Shift+Delete |
| New folder / Copy path / Properties | Ctrl+Shift+N / Ctrl+Shift+C / Alt+Enter |
| Context menu | Shift+F10 or Menu |
| Details / List / Icons | Ctrl+1 / Ctrl+2 / Ctrl+3 |
| Hidden files / Preview pane / Refresh | Ctrl+H / Alt+P / F5 |
| Switch light / dark | Alt+Shift+D |
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
| `src/archive.rs` | Archive browsing: libarchive bindings, the `vq-archive` GFile, extraction |
| `src/filemanager1.rs` | `org.freedesktop.FileManager1` D-Bus service ("Show in folder") |
| `src/fileops.rs`, `src/clipboard.rs` | Copy/move worker, trash, rename, clipboard |
| `src/properties.rs`, `src/settings.rs` | Dialogs |
| `src/config.rs`, `src/util.rs`, `src/search.rs` | Config, helpers, search worker |

`cargo test` covers the config/override resolution, history, natural sort,
glob matching, binary detection and naming helpers.
