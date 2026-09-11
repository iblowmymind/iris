# Branch platform compatibility

Audit baseline: `mac-changes` commit `16561f0`, compared with `aac7674`, the
upstream parent incorporated by merge `508f027`. The review includes all 29
changed paths, including the DNS and build-script changes in `16561f0`.

## Platform boundaries

| Area | macOS | Windows and Linux |
| --- | --- | --- |
| Main GUI | Native menu bar, display-only main window, status in title | Upstream sidebar, machine controls, status footer, embedded configuration |
| Dialogs | Immediate OS viewports | Embedded egui windows |
| Display | Integer/stretch scaling and black margins | Same portable renderer; controls in the sidebar View menu |
| DNS | Apple libresolv system resolver | Existing UDP upstream default, 8.8.8.8 |
| DNS worker, DHCP, reply addressing | Shared Rust implementation | Shared Rust implementation |
| Camera permission | AVFoundation request through nokhwa | No macOS permission calls; Media Foundation on Windows, V4L2 on Linux |
| Camera errors | Shared error reporting | Shared error reporting |
| PCAP interface detection | Guarded networksetup invocation | Linux sysfs; Windows adapter description |
| AppKit dependencies and window tabbing | Target-gated dependencies and calls | Excluded |
| Bundle script and entitlements | macOS packaging only | Not used by other platform builds |
| Settings | Serde defaults for new scaling fields | Same backwards-compatible settings |

`classic_ui.rs` restores the upstream sidebar methods without duplicating the
emulator lifecycle or framebuffer implementation. Its only behavior additions
are access to the shared scaling modes and use of the shared fullscreen toggle.
The module and embedded-dialog body also compile in macOS unit tests, so missing
methods or incompatible types in those implementations are caught locally.
This is not equivalent to a native Windows or Linux build or UI test.

## Verification

The native platform CI workflow checks the default workspace, Lightning/JIT v2,
and PCAP GUI configurations on Linux, Windows, and macOS. It runs GUI and DNS
tests without starting a VM. Windows PCAP uses the
[Npcap SDK](https://npcap.com/dist/) for compile checks; capture-driver behavior
still requires a machine with a capture driver installed.

Local validation passed on macOS: 52 GUI tests (including embedded-dialog
viewport routing), the GUI compile check with PCAP/Lightning/JIT v2, build-script
syntax, workflow YAML parsing, and `git diff --check`.

Native Windows/Linux builds and interactive UI,
camera, and networking checks remain pending until the workflow or those hosts
run them. The running emulator and its app bundle were not replaced or stopped.
