//! Panels that aren't the emulated display get their own OS window.
//!
//! While a machine is running the main window shows the guest's framebuffer and
//! nothing else — no side panels, no status bar, and no floating egui windows
//! painted over the picture. Everything that used to be one of those (the
//! config editor, the help and diagnostic windows, and every confirmation) goes
//! through here instead, as a real window the compositor can put wherever the
//! user wants it.
//!
//! These are egui *immediate* viewports: they're drawn inside the parent's
//! frame, which is what lets the body borrow the app's state directly the way
//! an in-window `egui::Window` did.

use eframe::egui;

/// Show `contents` in its own OS window. Returns true when the user has asked
/// to close it (the window's close button), which the caller turns into
/// whatever "closed" means for that window — usually clearing the flag or the
/// `Option` that made it appear.
pub fn show(
    ctx: &egui::Context,
    id: &str,
    title: &str,
    size: [f32; 2],
    resizable: bool,
    contents: impl FnOnce(&mut egui::Ui),
) -> bool {
    // `show_viewport_immediate` wants an FnMut but calls it exactly once per
    // frame; the Option hands our FnOnce body over on that one call.
    let mut contents = Some(contents);
    let mut close = false;
    ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of(id),
        egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size(size)
            .with_resizable(resizable),
        |ctx, _class| {
            egui::CentralPanel::default().show(ctx, |ui| {
                if let Some(body) = contents.take() {
                    body(ui);
                }
            });
            if ctx.input(|i| i.viewport().close_requested()) {
                close = true;
            }
        },
    );
    close
}
