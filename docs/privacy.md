# Privacy Architecture

Ozma handles some of the most sensitive data that exists on a desk: screen content,
keystrokes, audio, and camera feeds. This document defines how that data is
handled, what Ozma Connect can and cannot see, and what commitments the project
makes — and enforces architecturally — about government access and data sharing.

---

## Inspiration and credit

The privacy stance in this document, and the camera architecture described in
[cameras.md](cameras.md), were directly inspired by the work of **Benn Jordan**,
whose videos documenting the surveillance practices of Ring, Flock, and similar
products are essential viewing for anyone building or buying networked devices
that touch their home or neighbourhood.

His work made concrete what many people vaguely felt: that "smart" home security
products are frequently surveillance products that happen to be useful, and that
the data collected is not under the control of the people who paid for the
hardware. The specific architecture decisions in Ozma — zero-knowledge relay,
default-deny IoT firewall, local Frigate processing, the explicit stance on
government data requests — exist in part because his documentation of what
companies like Ring actually do made it impossible to build something that could
do the same things, even accidentally.

If you haven't watched his videos on this topic, start there before reading this
document. The threat model becomes considerably more real.

---

## The problem we're solving

Products like Ring and Flock have demonstrated what happens when a networked device
company builds convenience on top of user data that the company can access: the data
becomes available to governments, law enforcement, and third parties, often without
the user's knowledge or a warrant. The harm is not theoretical — it is documented
and ongoing.

Ozma sees far more sensitive data than a doorbell camera. A keylogger at the network
level would be less intrusive than what Ozma _could_ do if it were designed like
Ring. It is not. This document describes the design choices that make that
impossible, not just the policies that forbid it.

---

## Two categories of data

Everything Ozma touches falls into one of two categories with completely different
handling:

### Category 1 — User data

**What it is**: screen content, HID input (keystrokes, mouse events), audio streams,
camera feeds, clipboard contents, captured video frames.

**Who can see it**: only the user and the machines the user deliberately connects.

**Can Ozma Connect see it?** No. Architecturally impossible:

- All user data travels inside **WireGuard tunnels** encrypted with ChaCha20-Poly1305.
- When traffic flows through a Connect relay, the relay is a **WireGuard L3 forwarder**.
  It routes encrypted packets. It cannot decrypt them. It never sees plaintext.
- Connect backup is **zero-knowledge**: config and certificates are encrypted
  client-side with a key derived from the user's password before upload. The server
  stores ciphertext. Even with full database access, Ozma cannot read a user's backup.
- The TLS private key for the user's `*.c.ozma.dev` subdomain is **generated on the
  controller and never transmitted to Connect**. Connect provides DNS-01 coordination
  for certificate issuance; it never touches the private key.

**Can Ozma (the company) be compelled to hand it over?** No, because Ozma does not
have it. We cannot hand over data we do not possess. This is the correct answer to
government requests — not a policy, an architecture.

### Category 2 — Metadata

**What it is**: anonymous usage statistics used to improve the service. Examples:

- Controller software version
- Feature flags in use (which modules are enabled)
- Node count and types (hardware/soft/virtual — not identities)
- Error rates and crash signatures (no stack traces containing user paths)
- Connection quality statistics (latency, packet loss — no packet contents)
- Subscription tier

**What it is not**: screen content, keystrokes, audio, device serial numbers, IP
addresses (beyond what TCP requires), user-identifiable information.

**Collection**: opt-in by default for anonymous statistics; opt-out always available
via `OZMA_TELEMETRY=0`. Telemetry is defined in `controller/telemetry.py` — the
code is auditable.

**Retention**: rolling 90-day aggregate. No individual event log. No user linkage.

---

## What Connect can and cannot see

| Data | Connect can see? | Why |
|------|-----------------|-----|
| Screen content | **No** | WireGuard encrypted end-to-end; relay is L3 forwarder only |
| Keystrokes / HID | **No** | Same — encrypted at source, decrypted only at destination node |
| Audio / camera | **No** | Same |
| Clipboard contents | **No** | Same |
| Backup contents | **No** | Zero-knowledge — client-side encrypted before upload |
| TLS private keys | **No** | Generated on controller, never transmitted |
| Which nodes you have | **No** | Node identities are public keys; no name or location metadata is transmitted |
| That you are running Ozma | **Yes** | Required for relay and subscription |
| Software version + features used | **Yes** | Anonymous telemetry (opt-out available) |
| Connection quality statistics | **Yes** | Latency/loss numbers only, no content |
| Subscription status | **Yes** | Required for billing |

---

## Government access

**We will not voluntarily share user data with any government or law enforcement
agency.**

This applies globally — not only to the jurisdiction where Ozma is incorporated.

**If we receive a warrant or legal order:**

1. We will first determine whether it targets Category 1 data (user data) or
   Category 2 data (metadata).
2. For Category 1 data: we will inform the requesting authority that we do not
   possess it. This is true. The architecture makes it true. We will provide
   documentation of the architecture to support this.
3. For Category 2 data: we will evaluate the legal validity of the order, challenge
   it where possible, and notify the affected user to the extent legally permitted.
4. We will never comply with a voluntary information request — only valid legal
   process with proper jurisdiction.
5. We will never install backdoors, weaken encryption, or modify the architecture
   to enable future data collection in response to government pressure.

**Warrant canary**: this document contains a warrant canary statement. As long as
this sentence is present, Ozma has never received a National Security Letter, FISA
order, or equivalent secret legal demand that we are prohibited from disclosing.

**Mass surveillance**: we will not participate in any bulk data collection program,
regardless of legal compulsion. If ordered to do so, we will challenge the order,
disclose it publicly to the extent legally possible, and — if necessary — shut down
the Connect relay rather than operate it as a surveillance tool.

---

## Data separation enforcement

The following table maps each data type to the enforcement mechanism that makes the
privacy commitment technically binding rather than merely a policy:

| Data type | Enforcement mechanism |
|-----------|----------------------|
| HID (keystrokes, mouse) | XChaCha20-Poly1305 AEAD on the mesh; relay is WireGuard L3 only |
| Screen / video | Same; H.265 stream encrypted at controller, decrypted at browser with user JWT |
| Audio | Same; VBAN and RTP streams inside WireGuard tunnel |
| Camera feeds | Same |
| Clipboard | Only transmitted inside WireGuard tunnel; never logged |
| Config backup | Argon2id KDF from user password → AES-GCM encryption client-side before upload |
| TLS private key | Generated by `cryptography` library on controller; never serialised over the network |
| Telemetry events | Code-level: `telemetry.py` fires only the specific fields listed above; no content paths |

---

## Self-hosting and air-gap

Both the controller and nodes run fully without Connect. Nothing requires internet
access except:

- Relay for remote access from outside the LAN
- Connect backup
- ACME certificate issuance for `*.c.ozma.dev` subdomains

Running without Connect provides complete air-gap operation. Users with high
security requirements (healthcare, legal, government) should evaluate this mode.
The relay coordinator protocol is open and self-hostable.

---

## What this means for feature design

These commitments constrain feature design:

- **AI agent features**: the AI agent (`agent_engine.py`) operates on screen content.
  Cloud vision providers (OmniParser, Connect-hosted models) receive only the
  specific screen region the user has designated for analysis — never a continuous
  stream. On-device inference (OPi5 NPU, local Ollama) is always preferred and
  is the default when hardware permits.
- **Session recording**: recordings are stored on the user's controller, not on
  Connect. Connect does not have access to recorded content.
- **Transcription**: live transcription uses local Whisper.cpp by default. Cloud
  fallback is opt-in and disclosed clearly in the UI.
- **Wallpaper sources**: sources that fetch from external services (Unsplash, Reddit)
  make requests from the controller. Connect is not in that path.
- **Audit log**: the hashchained audit log is stored locally. It is not transmitted
  to Connect. A user can provide it to an auditor directly.

---

## The code is the policy

Ozma is published under the AGPL-3.0 licence. Every line that handles user data
is publicly auditable. This matters because it changes the nature of the privacy
commitment from "trust us" to "verify it yourself":

- `controller/telemetry.py` — contains the complete list of fields that are ever
  transmitted to Connect. Read it. If a field isn't in that file, it isn't sent.
- `controller/connect.py` — the Connect client. Every API call Ozma makes to
  Connect servers is in this file.
- `controller/transport.py` — the encryption layer. The XChaCha20-Poly1305
  implementation is visible; there are no undocumented modes or bypass paths.

No privacy policy document — including this one — is a stronger guarantee than
auditable source code under a copyleft licence that requires derivative works
to remain open. If Ozma is ever modified to weaken these guarantees, the
modification is visible in the diff and the licence requires it to be published.

## Reporting concerns

If you believe Ozma is collecting or sharing data inconsistently with this document,
report it:

- GitHub: open an issue at `ozmalabs/ozma` with the `privacy` label
- Security issues: use the security advisory flow (GitHub private vulnerability
  reporting)

Changes to this document that weaken any commitment listed here require a public
announcement with a minimum 90-day notice period before taking effect.
