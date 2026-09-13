# PCAP bridging over Wi-Fi half-works, which is worse than not working

Symptom, from a real report: the guest is bridged and reachable at
192.168.10.65 — the host Mac can ping it and telnet into IRIX — but no other
device on the same LAN can ping, reach, or telnet it. Nothing in the logs, no
error, the NET indicator lights.

The cause is the host interface: `pcap_interface = "en0"`, and en0 is Wi-Fi.

## Why it cannot work

An 802.11 association is bound to a single MAC address, and infrastructure
mode's 3-address frames have nowhere to say "this frame is for a machine behind
me" — that is what 4-address/WDS mode exists for, and client adapters do not
offer it. So:

- **Outbound**, frames carrying the guest's MAC are dropped, either by the
  host's own Wi-Fi driver or by the AP, which sees a source MAC that never
  associated and treats it as spoofing.
- **Inbound**, another device ARPs for the guest (broadcast — that gets
  through, so it even *learns* the right MAC) and then sends unicast to
  `08:00:69:…`. The AP looks that MAC up in its association table, finds no
  station, and never transmits the frame. Promiscuous mode on the host cannot
  recover a frame the radio was never sent.

## Why the host still reaches the guest

This is the part that makes it confusing: host↔guest traffic never leaves the
machine. BPF taps the host's *outbound* path, so IRIS sees the host's ARP and
IP frames without them going near the radio, and its injected replies loop back
locally. The loop closes inside one computer. So "I can ping it from this Mac"
tells you nothing about whether the bridge works.

## What to check

`ifconfig <iface>` won't tell you. On macOS,
`networksetup -listallhardwareports` maps device → hardware port; on Linux,
`/sys/class/net/<iface>/wireless` exists for wireless devices. Both are what
`net_pcap::is_wireless` uses, and the GUI's interface picker now tags such
interfaces `[Wi-Fi — cannot bridge]` and explains the failure inline.

The fix is a wired NIC (a USB/Thunderbolt Ethernet adapter counts), or NAT
mode, which works over anything because the guest never appears on the LAN as
its own host.

Unrelated red herring worth naming: a host VPN (Mullvad and friends) does *not*
break this. PCAP bridging is layer 2 on the interface, below the host IP stack
and below PF, so the VPN's routes and kill-switch rules never see the guest's
frames.
