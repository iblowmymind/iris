//! Screen metrics AppKit knows better than winit.

use objc2::MainThreadMarker;
use objc2_app_kit::NSScreen;

/// The usable area of the screen the window is on, in native points: the
/// monitor minus the menu bar and the Dock.
///
/// Worth asking AppKit rather than guessing, because fitting a 1280x1024
/// display at 1x onto a 1080p screen comes down to a handful of points — the
/// difference between "exactly 1x" and "0.99x, and therefore smoothed".
/// AppKit also constrains a window to this area itself, so a size derived from
/// anything larger is a size we will not actually get.
pub fn work_area() -> Option<(f32, f32)> {
    let mtm = MainThreadMarker::new()?;
    let screen = NSScreen::mainScreen(mtm)?;
    let frame = screen.visibleFrame();
    Some((frame.size.width as f32, frame.size.height as f32))
}
