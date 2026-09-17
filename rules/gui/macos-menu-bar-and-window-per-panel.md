# macOS menu bar and separate windows

iris-gui used to put a 186 pt control column, a collapsible config side panel,
and a status footer in the same window as the emulated display. On macOS, all
three are gone. Windows and Linux retain that layout in `classic_ui.rs`. What replaced them, and the traps in each:

## Native menu description

`src/menus.rs` builds a platform-neutral tree (`Menu` / `Item` / `Action`) from
app state, and `App::apply_menu_action` is the single place a chosen item takes
effect. On macOS `src/macos_menu.rs` renders that tree as a real `NSMenu`;
Windows and Linux use the original sidebar menus in `classic_ui.rs`. Shared
features need an entry in both interfaces.

- **An `NSMenuItem` can't hold a closure.** Each item carries a *tag* indexing
  the action table for the menu as last built; a click queues the `Action` and
  the app applies it on its next frame. That is also why a menu item can open a
  file dialog: by the time the action runs, the menu has closed. (Opening one
  from inside an egui menu closure, as the old SCSI menu did, put the dialog
  underneath the open menu.)
- **The model is rebuilt on a throttle, not every frame** — naming a SCSI slot
  stats its image file, and a change hands AppKit a whole new menu. Rebuilding
  is safe at any point *between* interactions: while a menu is open, AppKit's
  tracking loop blocks the event loop the rebuild is called from.
- **`setAutoenablesItems(false)` on every menu you create.** Otherwise AppKit
  decides enablement itself and quietly ignores `setEnabled(false)`.
- winit installs its own menu bar at launch (`platform_impl/macos/menu.rs`),
  including a Quit wired to `terminate:`. We replace the whole bar, which is
  what lets Quit go through the app's close handling — so the exit-time CHD
  fold-back is no longer skipped when quitting from the menu.
- winit is on objc2 0.5 / objc2-app-kit 0.2 while this crate uses 0.6 / 0.3.
  Two versions in one graph is fine — they're bindings over the same runtime —
  as long as no *typed* object is handed between them.

## macOS panels use OS windows

`src/oswindow.rs` wraps `Context::show_viewport_immediate`: the config editor,
the help and diagnostic windows, and every confirmation are separate windows.
On non-macOS systems the wrapper uses an embedded `egui::Window`.
Immediate viewports are drawn inside the parent's frame, which is what lets
their bodies borrow app state directly the way an in-window `egui::Window` did.

- **Do not nest them.** Every `oswindow::show` call must happen at the top level
  of `App::ui`, never inside another window's body — an immediate viewport
  inside an immediate viewport is not allowed. Config-editor tabs therefore
  *set a flag* (`net_sanity_modal`, `confirm_embedded_prom`) and the window for
  it is opened from the top level on the next frame.
- The body gets the *child* context. Anything that must act on the main window
  (viewport commands, the input pump) needs the parent's `ctx`.
- `show` returns whether the user hit the window's close button; wire that into
  the same state the body's own Cancel button sets, or the window can't be
  dismissed from the title bar.

## macOS status appears in the window title

`App::window_title` composes run state, MIPS, networking, on-screen scale, the
capture hint and the transient toast; `sync_window_title` pushes it at ~4 Hz and
only when the text changed. Retitling every frame is both wasted work and
visibly jittery, and the toast now expires there rather than in a status widget.
