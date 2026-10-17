# Native macOS front-end (built with --features macos-gui): text that only the
# system menu bar, the window title and the native windows show. Everything the
# default layout also shows uses its ids from the other files.

## Application menu

macos-app-about = About IRIS
macos-app-services = Services
macos-app-hide = Hide IRIS
macos-app-hide-others = Hide others
macos-app-show-all = Show all
macos-app-quit = Quit IRIS

## File menu

macos-file-settings = Settings…
# A granted disk folder, as a submenu title. $path is the folder.
macos-file-folder-granted = { $path } (access granted)
macos-file-folder-lapsed = { $path } (no access, grant again)
macos-file-folder-reveal = Show in Finder
macos-file-folder-revoke = Remove access

## Machine menu

# $keys is the key combination that releases capture, such as "Left Ctrl+Alt".
macos-machine-release-capture = Release mouse and keyboard ({ $keys })

## View menu

macos-view-fullscreen = Enter full screen
macos-view-exit-fullscreen = Exit full screen
# $scale is a number such as 1 or 1.25.
macos-view-vm-scale = Screen size: { $scale }×
macos-view-ui-scale = Interface size: { $scale }×

## Help menu

macos-help-net-check = Check networking…
macos-help-github = IRIS on GitHub

## Window title
# The main window's title carries what the default layout shows in its status
# footer, as parts joined by " · ".

# $status is the rest of the title.
macos-title = IRIS — { $status }
macos-status-powered-off = Powered off
# $backend is "NAT" or the bridged interface (macos-status-net-bridged).
macos-status-net = Network: { $backend }
macos-status-net-idle = Network: { $backend }, no traffic
macos-status-net-off = Network: { $backend }, off
# $iface is a host network interface name, or "Auto".
macos-status-net-bridged = bridged to { $iface }
