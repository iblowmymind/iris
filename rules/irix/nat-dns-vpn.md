# NAT DNS and macOS VPNs

If guest TCP connections to public IP addresses work with Mullvad enabled but
hostname lookups fail, check DNS forwarding before changing routing or bridging.
The reported reproduction connected to 1.1.1.1:443 and exchanged terminal data
with 74.207.233.40:1984 while Mullvad was active.

IRIS previously intercepted every guest UDP port 53 request and forwarded it to
8.8.8.8. Mullvad restricts DNS to its permitted resolver or configured custom DNS
servers. Changing the guest resolver did not change the actual upstream.

On macOS, `src/net_dns.rs` uses `dns_open(NULL)` and `dns_query` from libresolv.
Apple's Super resolver selects system and domain-specific DNS configurations.
Each query opens a fresh handle, so VPN changes do not require a VM restart.
There is no fallback to a public DNS server on macOS. Other platforms retain
their existing default upstream. An explicit `GatewayConfig::dns_upstream`
override uses UDP forwarding.

DNS runs on a worker with bounded queues, keeping resolver waits off the NAT
thread. DHCP advertises the gateway as DNS. Replies preserve the guest's query
ID and requested server address; the old implementation always replied from
the gateway even when the guest queried another address.

Validation: unit tests cover query parsing, malformed requests, negative DNS
responses, and forwarding through a local UDP resolver. A live guest test with
Mullvad remains necessary; do not restart an active VM to run it without the
user's instruction. DNS over TCP still follows the ordinary TCP NAT path.

References:

- [Mullvad connected-state DNS policy](https://github.com/mullvad/mullvadvpn-app/blob/main/docs/security.md#connected)
- [Apple DNS API](https://github.com/apple-oss-distributions/libresolv/blob/main/dns.h)
