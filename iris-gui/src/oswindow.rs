//! macOS panels use separate OS windows. Other platforms use embedded egui
//! windows, preserving the upstream single-window layout.

use eframe::egui;

/// Show `contents` in its own OS window. Returns true when the user has asked
/// to close it (the window's close button), which the caller turns into
/// whatever "closed" means for that window — usually clearing the flag or the
/// `Option` that made it appear.
#[cfg(target_os = "macos")]
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

#[cfg(any(not(target_os = "macos"), test))]
pub fn show_embedded(
    ctx: &egui::Context, id: &str, title: &str, size: [f32; 2],
    resizable: bool, contents: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut open = true;
    egui::Window::new(title)
        .id(egui::Id::new(id))
        .open(&mut open)
        .collapsible(false)
        .resizable(resizable)
        .default_size(size)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, contents);
    !open
}

#[cfg(not(target_os = "macos"))]
pub use show_embedded as show;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_dialog_keeps_content_in_root_viewport() {
        let ctx = egui::Context::default();
        let mut drawn = false;
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(!show_embedded(ui.ctx(), "platform_dialog", "Configuration",
                [440.0, 300.0], true, |ui| {
                    drawn = true;
                    assert_eq!(ui.ctx().viewport_id(), egui::ViewportId::ROOT);
                    ui.label("Configuration content");
                }));
        });
        // Headless test: no renderer consumes the font texture upload.
        output.textures_delta.clear();
        assert!(drawn);
        assert_eq!(output.viewport_output.len(), 1);
        assert!(output.viewport_output.contains_key(&egui::ViewportId::ROOT));
    }
}
