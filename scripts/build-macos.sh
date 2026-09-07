#!/bin/bash
# Build iris-gui as a local .app bundle for macOS testing.
#
# Running the binary directly (cargo run / ./iris-gui) will always open
# inside your current Terminal window. This script wraps it in a proper
# .app bundle so you can launch it with  open IRIS.app  — just like users
# do — and the Terminal window stays closed.
#
# Usage:
#   ./scripts/build-macos.sh            # standard build
#   ./scripts/build-macos.sh lightning  # enable iris/lightning feature
#   ./scripts/build-macos.sh appstore   # SANDBOXED build (App Store parity)
#
# The `appstore` variant compiles `--features appstore` (so the real
# security-scoped bookmark code + IRIS_CHD_DIFF_DIR are active, not the
# off-sandbox stubs) and signs with installer/iris-gui-sandbox-local.entitlements
# (app-sandbox = true). The result is a genuinely sandboxed IRIS.app you can run
# locally — no App Store round-trip — to test the folder-grant / CHD-fold flow.
# Ad-hoc signing by default; set CODESIGN_IDENTITY="Developer ID Application: …"
# to get persistent app-scoped bookmarks (see the entitlements file's caveat).
#
# After it finishes:
#   open IRIS.app

set -e

VARIANT="${1:-standard}"

# ── Architecture ────────────────────────────────────────────────────────────

ARCH=$(uname -m)
if [ "$ARCH" = "arm64" ]; then
    TARGET="aarch64-apple-darwin"
elif [ "$ARCH" = "x86_64" ]; then
    TARGET="x86_64-apple-darwin"
else
    echo "Unsupported architecture: $ARCH" >&2
    exit 1
fi

# ── Bundle ID — derived from the git remote so any fork gets the right ID ──
# git@github.com:owner/repo.git  →  io.github.owner.repo
# https://github.com/owner/repo  →  io.github.owner.repo

REMOTE_URL=$(git remote get-url origin 2>/dev/null || echo "")
if [[ "$REMOTE_URL" =~ github\.com[:/]([^/]+)/([^/.]+) ]]; then
    BUNDLE_ID="io.github.${BASH_REMATCH[1]}.${BASH_REMATCH[2]}"
else
    # Fallback: read from Cargo.toml if present, otherwise use a default
    BUNDLE_ID=$(grep -m1 '^bundle_id\s*=' iris-gui/Cargo.toml 2>/dev/null \
        | sed 's/.*"\(.*\)".*/\1/' || echo "io.github.unknown.iris")
fi

echo "Building iris-gui ($VARIANT) for macOS ($ARCH)..."
echo "  Bundle ID: $BUNDLE_ID"

# ── Build ───────────────────────────────────────────────────────────────────

if [ "$VARIANT" = "lightning" ]; then
    cargo build --release --target "$TARGET" -p iris-gui --features iris/lightning
elif [ "$VARIANT" = "appstore" ] || [ "$VARIANT" = "sandbox" ]; then
    # Sandbox parity: enable the bookmark code + container diff redirect.
    # lightning gives a usable interpreter (the appstore feature forces
    # IRIS_NO_JIT, so the MIPS/REX JITs are off regardless).
    cargo build --release --target "$TARGET" -p iris-gui --features appstore,iris/lightning
else
    cargo build --release --target "$TARGET" -p iris-gui
fi

VERSION=$(cargo metadata --no-deps --format-version 1 2>/dev/null \
    | python3 -c "import sys,json; pkgs=json.load(sys.stdin)['packages']; \
      print(next(p['version'] for p in pkgs if p['name']=='iris-gui'))" 2>/dev/null \
    || echo "0.0.0-local")

# ── Bundle ──────────────────────────────────────────────────────────────────

BUNDLE="IRIS.app"
rm -rf "$BUNDLE"
mkdir -p "${BUNDLE}/Contents/MacOS" "${BUNDLE}/Contents/Resources"

cp "target/${TARGET}/release/iris-gui" "${BUNDLE}/Contents/MacOS/iris-gui"
chmod +x "${BUNDLE}/Contents/MacOS/iris-gui"

if [ -f "iris-gui/assets/icons/icon.icns" ]; then
    cp "iris-gui/assets/icons/icon.icns" "${BUNDLE}/Contents/Resources/AppIcon.icns"
fi

cat > "${BUNDLE}/Contents/Info.plist" << EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>IRIS</string>
    <key>CFBundleDisplayName</key><string>IRIS</string>
    <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleExecutable</key><string>iris-gui</string>
    <key>CFBundleIconFile</key><string>AppIcon.icns</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>LSMinimumSystemVersion</key><string>10.13</string>
    <key>NSCameraUsageDescription</key><string>Provides the IndyCam video input for SGI Indy emulation (VINO device).</string>
</dict>
</plist>
EOF

# ── Sign ────────────────────────────────────────────────────────────────────

# The sandboxed variant signs with the local sandbox entitlements (app-sandbox).
# Everything else signs with the *notarized* (Developer ID) entitlements — the
# same file release.yml uses for the DMG, so a local build behaves like the one
# users download. It used to use installer/iris-gui.entitlements, which is the
# Mac App Store file: that carries com.apple.security.app-sandbox, so a plain
# local build came out sandboxed while the `appstore` feature — and with it the
# security-scoped bookmark code in macos_sandbox.rs — was NOT compiled in. The
# result was a bundle that could not reach a disk image outside its container
# on the next launch, for a variant that was never meant to be sandboxed at all.
# (It also masked a missing camera entitlement in the notarized file, since the
# App Store one has it: rules/macos/camera-needs-both-a-request-and-an-entitlement.md.)
# Use `./scripts/build-macos.sh appstore` when you actually want the sandbox.
# CODESIGN_IDENTITY overrides the default ad-hoc identity (e.g. a Developer ID
# for persistent bookmarks).
SIGN_ID="${CODESIGN_IDENTITY:--}"
if [ "$VARIANT" = "appstore" ] || [ "$VARIANT" = "sandbox" ]; then
    ENTITLEMENTS="installer/iris-gui-sandbox-local.entitlements"
else
    ENTITLEMENTS="installer/iris-gui-notarized.entitlements"
fi

echo "Signing bundle (identity: ${SIGN_ID}, entitlements: ${ENTITLEMENTS})..."
if [ -f "$ENTITLEMENTS" ]; then
    # Validate first: codesign's entitlements parser is strict, and a parse
    # failure would otherwise leave the bundle unsigned / un-sandboxed without
    # an obvious error. `plutil -lint` alone is NOT enough: it accepts a double
    # hyphen inside an XML comment, which is illegal XML and which AMFI rejects
    # with "Failed to parse entitlements: AMFIUnserializeXML: syntax error".
    # Comments in these files must not contain `-` twice in a row (write
    # "the runtime codesign option", not the flag itself).
    plutil -lint "$ENTITLEMENTS" >/dev/null
    if sed 's/<!--//g; s/-->//g' "$ENTITLEMENTS" | grep -q -- '--'; then
        echo "error: $ENTITLEMENTS has a double hyphen inside a comment;" >&2
        echo "       AMFI will refuse to parse it. Reword the comment." >&2
        exit 1
    fi
    codesign --force --deep --sign "$SIGN_ID" --entitlements "$ENTITLEMENTS" "${BUNDLE}"
else
    codesign --force --deep --sign "$SIGN_ID" "${BUNDLE}"
fi

echo ""
echo "Done: ${BUNDLE} (${VARIANT}, bundle ID: ${BUNDLE_ID})"
echo ""
echo "Launch without Terminal:"
echo "  open ${BUNDLE}"
echo ""
echo "Or double-click IRIS.app in Finder."

if [ "$VARIANT" = "appstore" ] || [ "$VARIANT" = "sandbox" ]; then
    echo ""
    echo "This is a SANDBOXED build. Verify the sandbox is actually on:"
    echo "  codesign -d --entitlements - ${BUNDLE} 2>/dev/null | grep -A1 app-sandbox"
    echo "  ls ~/Library/Containers/${BUNDLE_ID}/   # created on first launch"
    echo ""
    echo "Test the CHD folder-grant / fold: attach a compressed .chd from a"
    echo "folder you have NOT granted -> the grant modal should appear; grant the"
    echo "folder, boot, then quit -> the .diff.chd should fold away and the disk"
    echo "shrink. (Ad-hoc bookmarks may not persist across relaunches; the"
    echo "within-session fold does not depend on that.)"
fi
