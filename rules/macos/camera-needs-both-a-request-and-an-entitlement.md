# The IndyCam camera on macOS needs *two* things, and IRIS had neither

Symptom: `Help → Test Camera` (and `[vino] source = "camera"`) showed a black
picture, no error, and macOS never asked for camera permission — in the shipped,
notarized DMG build.

Two independent causes, both required for capture to work:

## 1. Something has to *ask* — AVFoundation never does

AVFoundation will happily start a capture session while unauthorized and simply
deliver nothing. The permission dialog only appears if the app calls
`AVCaptureDevice.requestAccessForMediaType:`. nokhwa wraps that as
`nokhwa_initialize(cb)` and its docs say plainly: *"It is your responsibility to
call this function before anything else, but only on MacOS."* IRIS never called
it, so the prompt never appeared, `Camera::new` succeeded, and every frame was
black.

`src/camera_nokhwa.rs` now calls `nokhwa_check()` and, if not already
authorized, `nokhwa_initialize` — and **blocks the capture thread on the
answer**, because the wait is the user reading the dialog. A previously refused
camera returns denied immediately without a prompt, which is why the error text
points at System Settings → Privacy & Security → Camera: that is the only place
it can be undone from.

Note `NSCameraUsageDescription` must be in the bundle's Info.plist before you
call `requestAccess` — TCC kills a bundled app that asks without one. Both
`scripts/build-macos.sh` and `release.yml`'s `make_dmg` write it.

## 2. Under the hardened runtime, the camera needs an entitlement

Signing with `codesign --options runtime` (which the notarized DMG does) gates
camera access behind `com.apple.security.device.camera`. Without it the request
is refused *before TCC is consulted*, so there is no prompt to accept — the same
black-picture symptom, from a completely different layer.

One trap when editing these files: **a comment may not contain a double
hyphen.** That is illegal XML, and AMFI enforces it —
`Failed to parse entitlements: AMFIUnserializeXML: syntax error near line N` —
while `plutil -lint` accepts it happily. Writing `--options runtime` in a
comment is enough to break signing. `scripts/build-macos.sh` now checks for it
before calling codesign, since the failure names a line number and nothing else.

`installer/iris-gui.entitlements` (App Store) and
`iris-gui-sandbox-local.entitlements` both carried it. `iris-gui-notarized.
entitlements` — the file `release.yml` actually signs the DMG and the CLI
binaries with — did not. That is why the camera worked in a local
`./scripts/build-macos.sh` build (which signs with the App Store file) and not
in the release everyone downloads. **When adding a resource-access capability,
check all three entitlement files.**

## 3. A failure on the capture thread used to be invisible

`CameraSource::new` spawns the worker and returns `Ok` *before* the camera is
opened, so every open failure happened after the caller had already been told
everything was fine. The backends only `eprintln!`d and returned; `next_field()`
kept handing out black fields forever. `Shared::error` + `CameraSource::error()`
now carry the reason to the GUI's Test Camera window and into `vino status`.
Any future "it just shows black" should surface as text.
