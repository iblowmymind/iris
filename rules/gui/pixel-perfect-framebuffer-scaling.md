# The emulated display only stays crisp if BOTH the scale and the rect are exact

Symptom: the View menu's `1×` produced a picture measured at ~0.9×, and even
when the number looked right, thin strokes lost whole rows/columns of pixels —
"nearest-neighbour scaled down" is exactly what it looks like, because that is
what it was.

Three separate bugs, all in `iris-gui/src/main.rs`:

1. **`snap_window_to_fb` never honoured the request.** It clamped the window to
   `MARGIN = 0.85` of the monitor "so the window stays obviously windowed". A
   1280×1024 display at 1× needs 1024 points of height; on a 1080p screen
   `1080 × 0.85 = 918`, so the requested scale was silently reduced to
   `918/1024 ≈ 0.897`. That is the reported 0.9×. The fit now uses the real
   work area — `NSScreen.visibleFrame` on macOS, which is also what AppKit
   constrains the window to, so a size derived from anything larger is a size
   you will not get — minus the window's *measured* title-bar height
   (`outer_rect − inner_rect`). Fitting 1280×1024 at 1× onto a 1080p screen
   comes down to a handful of points, far too tight to guess at.

   When the request genuinely doesn't fit, take the exact fit rather than
   stepping down to the next whole device scale: under the requested size we
   are minifying, where nothing is pixel-exact anyway, and a step down is a
   *halving* on a HiDPI screen.

2. **The draw filled the window instead of scaling by a whole number.**
   `fb_fit_size` returns the exact aspect-preserving fit, so any window size
   that isn't an exact multiple of 1280×1024 gives a fractional scale, which
   NEAREST can only render by doubling some source pixels and not others.
   `fb_device_scale` now floors to a whole number of device pixels at 1:1 and
   above (integer scaling); only genuine minification stays fractional, and
   that path uses LINEAR.

3. **The rect was not on the device-pixel grid.** `centered_and_justified`
   centres the image in the available space, which lands its edges on a
   *fraction* of a device pixel whenever the leftover space is odd. NEAREST
   then samples across texel boundaries and drops or doubles rows even at an
   exact integer scale. `snap_rect_to_pixels` rounds the rect's origin and size
   to whole device pixels, and the image is painted directly
   (`painter.image` on an explicitly interacted rect) rather than through
   `Image` + layout, so nothing re-rounds it afterwards.

Gotchas worth keeping in mind here:

- `vm_scale` is **logical points** per emulated pixel, not device pixels. `1×`
  is 1 device pixel on a standard screen and **2** on Retina — both are
  pixel-exact, which is the point.
- Every size egui-winit reports (`viewport_rect`, `available_size`,
  `monitor_size`) and `ViewportCommand::InnerSize` are in the **zoom-scaled**
  point space, so a reserve expressed in native points has to be divided by
  `ctx.zoom_factor()`.
- The window manager rounds the frame to whole device pixels, so the fit can
  land a hair under an integer. `snap_window_to_fb` adds a point of slack and
  `fb_device_scale` accepts the next step up when it overhangs by ≤1 device
  pixel — without that, being 0.2 px short knocks 2× down to 1× and halves the
  picture.

## Scaling modes

`display_size` returns both the destination size and the texture-filter choice.
Both display heads and the single-display path use it. Nearest integer preserves
integer device-pixel scaling; Stretch uses linear filtering and can optionally
preserve aspect ratio. Draw rectangles remain centered and aligned to device
pixels, and the running central panel stays black in both modes.

A mode change or resize can change the required texture filter even when the
frame sequence has not advanced. Update the sampler in that case too; otherwise
a frozen guest can retain nearest sampling while stretched to a fractional size.
