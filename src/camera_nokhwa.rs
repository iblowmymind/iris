//! nokhwa-based capture loop (macOS AVFoundation, Windows MediaFoundation).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use parking_lot::Mutex;

use super::{report_error, Shared, downscale_yuyv_to_uyvy, split_fields};

/// Ask macOS for camera access, and wait for the answer.
///
/// AVFoundation hands out no frames until the user has allowed the camera, and
/// — this is the part that bites — it never *asks* on its own. Something has to
/// call `AVCaptureDevice.requestAccessForMediaType:` (which is what nokhwa's
/// `nokhwa_initialize` does) or the prompt never appears, the capture session
/// starts anyway, and every frame is black. That was IRIS's IndyCam on macOS:
/// no prompt, no picture, no error.
///
/// Blocking here is fine — this runs on the dedicated capture thread, and the
/// wait *is* the user reading the prompt. A previously refused camera comes
/// back denied immediately without a prompt, so the message points at the only
/// place that can be undone from.
#[cfg(target_os = "macos")]
fn ensure_camera_access() -> Result<(), String> {
    use std::sync::mpsc;

    if nokhwa::nokhwa_check() {
        return Ok(());
    }
    let (tx, rx) = mpsc::channel();
    nokhwa::nokhwa_initialize(move |granted| {
        let _ = tx.send(granted);
    });
    match rx.recv_timeout(Duration::from_secs(60)) {
        Ok(true) => Ok(()),
        Ok(false) => Err("macOS denied camera access. Allow IRIS under System Settings \u{2192} \
                          Privacy & Security \u{2192} Camera, then try again."
            .to_string()),
        Err(_) => Err("timed out waiting for the macOS camera permission prompt".to_string()),
    }
}

#[cfg(not(target_os = "macos"))]
fn ensure_camera_access() -> Result<(), String> {
    Ok(())
}

pub(super) fn capture_loop(shared: Arc<Mutex<Shared>>, frame_w: u32, frame_h: u32,
                            camera_index: u32, running: Arc<AtomicBool>) {
    use nokhwa::pixel_format::YuyvFormat;
    use nokhwa::utils::{
        CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution,
    };
    use nokhwa::Camera;

    // Most cameras don't advertise 640×486 (NTSC native isn't a standard
    // webcam mode), and many only expose NV12 / MJPEG at their native
    // resolutions.  Ask for the highest-framerate format the camera offers
    // and let YuyvFormat handle the decode to YUV422 inside nokhwa.  We
    // downscale to (frame_w × frame_h) ourselves regardless.
    let _ = CameraFormat::new(
        Resolution::new(frame_w, frame_h),
        FrameFormat::YUYV,
        30,
    ); // (kept for future fine-grained requests)

    let req = RequestedFormat::new::<YuyvFormat>(
        RequestedFormatType::AbsoluteHighestFrameRate
    );

    if let Err(e) = ensure_camera_access() {
        report_error(&shared, e);
        return;
    }

    let mut cam = match Camera::new(CameraIndex::Index(camera_index), req) {
        Ok(c) => c,
        Err(e) => {
            report_error(&shared, format!("open failed: {e} \u{2014} falling back to black source"));
            return;
        }
    };
    if let Err(e) = cam.open_stream() {
        report_error(&shared, format!("open_stream failed: {e} \u{2014} falling back to black source"));
        return;
    }

    let res = cam.resolution();
    let sw  = res.width_x;
    let sh  = res.height_y;
    eprintln!("camera: streaming at {}×{} → downscale to {}×{}", sw, sh, frame_w, frame_h);
    {
        let mut s = shared.lock();
        s.capture_res = Some((sw, sh));
        s.error = None;
    }

    while running.load(Ordering::Relaxed) {
        let buf = match cam.frame() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("camera: frame error: {} — retrying", e);
                thread::sleep(Duration::from_millis(33));
                continue;
            }
        };

        let yuyv = buf.buffer();
        let expected = (sw * sh * 2) as usize;
        if yuyv.len() < expected {
            eprintln!("camera: short frame ({} bytes for {}×{}), skipping",
                yuyv.len(), sw, sh);
            continue;
        }

        let frame = downscale_yuyv_to_uyvy(yuyv, sw, sh, frame_w, frame_h);
        let (even, odd) = split_fields(&frame, frame_w, frame_h);

        let mut s = shared.lock();
        s.even = Some(Arc::from(even));
        s.odd  = Some(Arc::from(odd));
        s.frame_count += 1;
    }
}
