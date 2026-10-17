# Packet capture permission, NAT subnet checks. See ../README.md for conventions.

## Packet capture permission
## Shown next to the packet capture warning in the network settings, and as
## the result of the button that enables capture.

capture-hint-linux = Installed packages can capture packets already. The AppImage restarts IRIS with administrator rights.
capture-hint-macos = Needs a one-time administrator install. Quit and reopen IRIS afterwards.
capture-hint-windows = Needs the Npcap driver and Administrator rights. The button opens the Npcap download page.
capture-hint-other = Packet capture needs administrator rights on this system.

# $error is the operating system's error text.
capture-error-no-exe = Couldn't find the IRIS program file: { $error }
# $path is the IRIS program file. Keep the command line as it is.
capture-error-no-pkexec =
    pkexec isn't installed. Install polkit, or allow packet capture by hand:

        sudo setcap cap_net_raw,cap_net_admin+eip { $path }
capture-error-pkexec = Couldn't restart IRIS with pkexec: { $error }
capture-error-no-user = Couldn't tell which macOS user should get capture access.
capture-error-stage = Couldn't prepare the capture helper: { $error }
capture-error-script = Couldn't write the capture helper's install script: { $error }
capture-error-install = Couldn't install the capture helper: { $error }
capture-error-osascript = Couldn't ask for administrator rights: { $error }
capture-enabled-relaunch = Packet capture is on. Quit and reopen IRIS, then start the machine again.
capture-npcap-installed = Npcap is installed. Restart IRIS as Administrator, then start the machine again.
capture-npcap-download = The Npcap download page is open in your browser. Install Npcap, then restart IRIS as Administrator and try again.
capture-unsupported = Packet capture isn't supported on this system.

## NAT subnet checks

# $prefix is the prefix length the user picked; $max is the longest allowed (30).
netplan-prefix-out-of-range = A /{ $prefix } subnet won't work. Use a prefix from /1 to /{ $max }.
netplan-not-private = { $network } isn't a private address range.
# $interface is a host network interface name such as en0; $address is its IP address.
netplan-overlaps-host = Overlaps { $interface } ({ $address }) on this computer.
# An entry in the subnet mask list. $hosts is $count with digit grouping.
netplan-mask-option = /{ $prefix } = { $mask } ({ $count ->
    [one] { $hosts } host
   *[other] { $hosts } hosts
})
