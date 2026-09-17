# GUI platform boundaries

The macOS menu bar and separate-window layout (see
`macos-menu-bar-and-window-per-panel.md`) is macOS-only. Windows and Linux keep
the upstream layout, restored in `classic_ui.rs`.

| Area | macOS | Windows and Linux |
| --- | --- | --- |
| Main GUI | Native menu bar, display-only main window, status in title | Upstream sidebar, machine controls, status footer, embedded configuration |
| Dialogs | Immediate OS viewports (`oswindow::show`) | Embedded egui windows (same `oswindow::show` call) |
| AppKit dependencies and window tabbing | Target-gated dependencies and calls | Excluded |

`classic_ui.rs` restores the upstream sidebar methods without duplicating the
emulator lifecycle or framebuffer implementation; its only behaviour addition is
the shared fullscreen toggle. Shared features need an entry in both `menus.rs`
(macOS) and `classic_ui.rs` (Windows/Linux).

## Verification

`classic_ui.rs` and the embedded-dialog path are compiled under `cfg(test)` on
macOS as well, so `cargo test -p iris-gui` on a Mac catches missing methods or
incompatible types in them. That is not equivalent to a native Windows or Linux
build or UI test; run `cargo check --workspace` and `cargo test -p iris-gui` on
those hosts before relying on the layout there.
