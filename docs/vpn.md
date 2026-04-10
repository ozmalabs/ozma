# Ozma VPN

Ozma includes an optional VPN built on the same WireGuard infrastructure that
powers the device mesh. This document explains what the VPN actually does, what
it doesn't do, and how each mode differs — without the marketing language the VPN
industry typically uses.

---

## What VPN does and doesn't do

This section exists because the VPN industry has a long history of misleading
marketing. Understanding the actual threat model helps you decide whether and how
to use any VPN, including this one.

**What a VPN actually provides:**

- Your device's traffic is encrypted between you and the VPN exit node. An
  attacker on your local network (café WiFi, hotel, airport) cannot read it.
- Your ISP at the device end sees traffic to the VPN endpoint, not the sites you
  visit. They can see that you are using a VPN.
- Sites you visit see the exit node's IP address, not your device's IP. This is
  the extent to which a VPN changes your apparent origin.

**What a VPN does not provide:**

- **Anonymity.** Your traffic is still attributable — to the exit node operator
  (who can log it), to sites through fingerprinting, cookies, and login sessions,
  and through timing correlation attacks. A VPN prevents ISP-level traffic
  inspection; it does not prevent identification.
- **"No logs" guarantees from third parties.** Any VPN provider that makes this
  claim is either technically honest (they don't log) or legally meaningless (they
  can be compelled to start, or have already been). With your own exit node, "no
  logs" is actually true because you control the hardware.
- **Protection of traffic beyond the exit.** Traffic from the exit to its
  destination is unencrypted unless the underlying protocol (HTTPS, etc.) provides
  encryption. The exit node can read unencrypted traffic. For Tier 2 (Connect
  relay exit), Ozma can see this traffic.
- **Security against compromised applications.** A VPN encrypts the transport
  layer. An app that leaks data at the application layer continues to leak it.

---

## VPN modes

### Tier 1 — Home exit (your controller)

All traffic from your device is routed through your Ozma controller and exits
via the controller's internet connection.

| Property | Detail |
|---|---|
| Who can see your traffic | Your home/office ISP (same as if you were at your desk) |
| Can Ozma see your traffic? | No — it never touches Ozma infrastructure |
| Logs | None, unless you configure your own |
| Requires | Controller online and internet-reachable (or via Connect relay tunnel) |
| Cost | Included — no additional subscription |

This is the closest thing that exists to a genuinely private VPN. The only party
that can see your traffic is your home ISP — which is true regardless of whether
you're using a VPN or not. You are, in effect, making your remote connection
look like you're sitting at home.

**What it's good for:**
- Browsing securely on public WiFi
- Accessing home LAN resources remotely
- Avoiding network-level surveillance or filtering at your current location
- Not wanting your mobile ISP to see where you're browsing

**What it doesn't change:**
- Your home ISP still sees your traffic (as they always did)
- Sites you visit see your home IP

---

### Tier 2 — Connect relay exit

Traffic exits via Ozma Connect relay infrastructure rather than your own
controller. Your controller may be offline, or you may want a different
geographic exit.

| Property | Detail |
|---|---|
| Who can see your traffic | Ozma Connect (explicitly) |
| Can Ozma see your traffic? | **Yes.** This is not zero-knowledge. |
| Logs | Ozma Connect logs connection metadata. We do not log payload content. |
| Requires | Connect subscription |
| Cost | Included in Connect plans (subject to fair use) |

**This mode is explicitly not zero-knowledge for VPN traffic.** The camera
recording and key backup systems are zero-knowledge because encryption happens
before data reaches Connect. VPN exit traffic cannot be zero-knowledge — the exit
node, by definition, decrypts your traffic before forwarding it. Ozma Connect is
the exit node in this mode, so Ozma can see your traffic.

We state this clearly because most VPN providers do not. If you need traffic that
Ozma cannot see, use Tier 1 (your own controller as exit).

**What it's good for:**
- Your controller is offline or unreachable
- You need an exit point in a specific Connect relay region
- You trust Ozma Connect and want convenience over maximum privacy

---

### Tier 3 — Third-party exit nodes (planned)

Traffic routes through partnered exit infrastructure in specific geographic
regions.

| Property | Detail |
|---|---|
| Who can see your traffic | The exit node partner |
| Can Ozma see your traffic? | No — Ozma is only the relay to the exit |
| Logs | Partner-dependent — disclosed per provider |
| Requires | Connect subscription + exit region selection |
| Cost | Additional (exit infrastructure costs money) |

Partners will be chosen based on independent audit history and jurisdiction.
Policies will be disclosed per-provider, not blanket-claimed. We will not partner
with providers who make claims we cannot verify.

---

## Split tunnel vs full tunnel

**Full tunnel** routes all device traffic through the selected exit. This is the
traditional VPN mode.

**Split tunnel** routes only specified traffic through the exit; everything else
goes direct. The Ozma app already uses split tunnel for mesh access (only
service traffic is tunnelled; Android Auto and CarPlay are excluded). The same
mechanism is used for VPN mode.

Configurable options:

| Mode | What goes through the exit |
|---|---|
| Full tunnel | Everything |
| Ozma-only | Only traffic to your Ozma services (current default) |
| Custom | Per-app or per-destination rules |

Full tunnel and Ozma-only can coexist: Ozma services route to the mesh; all
other traffic routes to the VPN exit.

---

## DNS handling

DNS queries reveal browsing intent even if the payload is encrypted. In full
tunnel mode:

- DNS queries are routed through the exit node's resolver by default
- An encrypted resolver (DoH/DoT) can be configured at the controller
- DNS-over-WireGuard ensures queries do not leak to the local network resolver

In split tunnel mode, only DNS queries for Ozma-managed domains are routed
through the tunnel. All other DNS uses the device's default resolver.

---

## Kill switch

When full tunnel mode is active, a kill switch can be enabled: if the WireGuard
tunnel drops, all non-tunnel traffic is blocked rather than falling back to the
local network. This prevents traffic leakage during tunnel interruption.

Disabled by default. Recommended for full tunnel mode if privacy from the local
network is the goal.

---

## What Ozma does not claim

- We do not claim Ozma VPN makes you anonymous.
- We do not claim "military-grade encryption" (WireGuard uses ChaCha20-Poly1305,
  which is strong — we just describe it accurately).
- We do not claim Tier 2 or Tier 3 exit modes are zero-knowledge — they are not.
- We do not claim a VPN protects against all threats — it protects against
  specific, well-defined ones described above.

If a VPN cannot solve your actual threat model, we would rather tell you that
than sell you false confidence.

---

## Comparison with commercial VPN services

| | Commercial VPN | Ozma Tier 1 | Ozma Tier 2 |
|---|---|---|---|
| Exit node operator | Third party | You | Ozma |
| "No logs" claim | Policy (unverifiable) | Actually true (your hardware) | Metadata logged, payload not |
| Can be compelled to log | Yes (jurisdiction-dependent) | Only you can be compelled | Ozma can be compelled |
| Cost | $5–15/month typical | Included | Included in Connect |
| Geo-exit options | Many | Your home location | Connect relay regions |
| Setup | Separate app | Included in Ozma app | Included in Ozma app |
| Protects from local network | Yes | Yes | Yes |
| Anonymous | No | No | No |
