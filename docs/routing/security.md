# Security

**Status**: Draft

## Conformance

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT",
"SHOULD", "SHOULD NOT", "RECOMMENDED", "MAY", and "OPTIONAL" in this
document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

## Abstract

This section specifies the security model for the Ozma routing graph,
covering data plane encryption, identity and authentication, credential
management, multi-factor authentication composition, biometric data
protection, security postures, incident response, and USB device policy.
The model unifies every form of identity proof -- from device mesh
enrollment to biometric recognition to physical badge tap -- into a
single authentication framework that feeds intent bindings, the journal,
and access control decisions across the entire graph.

## Specification

### Data plane encryption

The routing protocol distinguishes between control plane and data plane
encryption:

**Control plane**: All control messages (topology exchange, capability
advertisement, route negotiation, health metrics) travel over TLS or within
the WireGuard mesh. This is not changed by this specification.

**Data plane**: Media streams (video, audio, HID) MUST use transport-level
encryption appropriate to the path:

| Path | Default encryption | Rationale |
|------|-------------------|-----------|
| Same machine (loopback) | None | Kernel boundary is sufficient |
| LAN (same broadcast domain) | XChaCha20-Poly1305 AEAD per packet | Lightweight, no tunnel overhead, no TCP overhead |
| WireGuard mesh (overlay) | WireGuard (ChaCha20-Poly1305) | Already encrypted by tunnel |
| Connect relay (remote) | WireGuard end-to-end | Relay sees only ciphertext |

**LAN encryption detail**: For UDP data plane traffic on the LAN (not tunnelled
through WireGuard), each packet MUST be encrypted with XChaCha20-Poly1305 using a
session key established via a Noise NK handshake at link setup time. This
provides:

- Per-packet authentication and encryption
- No TCP overhead (critical for real-time media)
- No tunnel overhead (WireGuard adds ~60 bytes/packet)
- Forward secrecy (session keys are ephemeral)

The intent's `encryption` constraint controls this:
- `required`: every link MUST encrypt (default for all intents)
- `preferred`: encrypt if the transport supports it, MAY allow unencrypted otherwise
- `none`: explicitly disable encryption (for debugging or trusted networks)

### Identity and Authentication

Authentication in the routing graph is a unified model covering every way
an entity can prove its identity -- from a device enrolling in the mesh to
a person's face recognised by a camera to a fingerprint on a reader to a
passkey in a browser. These are all authentication events in the same
system, and they all affect the graph.

#### Identity model

Two classes of identity exist in the graph:

```yaml
DeviceIdentity:
  # Devices authenticate via cryptographic identity (mesh CA)
  type: "device"
  id: string                    # device's stable Ozma identifier
  identity_key: Ed25519Key      # device's identity keypair
  certificates: Certificate[]?  # issued by controller's mesh CA
  enrollment_state: string      # "enrolled", "pending", "revoked"
  # A device's identity MUST be verified before its ports and capabilities
  # are added to the graph. Unauthenticated devices MUST NOT be routed to.

PersonIdentity:
  # People authenticate via credentials -- physical, biometric, or digital
  type: "person"
  id: string                    # person's stable identifier (opaque, not PII)
  display_name: string?         # "Matt", "Guest 1" (optional, for UI)
  credentials: Credential[]     # all registered credentials for this person
  roles: string[]?              # "admin", "user", "guest", "operator"
  zones: string[]?              # which spatial zones this person can access
  scenarios: string[]?          # which scenarios this person can activate
  preferences: PersonPreferences?  # per-person defaults (intent, audio, lighting, thermal)
```

Devices MUST authenticate before being added to the graph. An
unauthenticated device MUST NOT have its ports or capabilities visible
to the router.

#### Credential types

Every way to prove identity is a credential. Credentials are typed,
have strength levels, and can be combined for multi-factor authentication:

```yaml
Credential:
  id: string                    # credential identifier
  type: CredentialType
  strength: string              # "possession" (something you have),
                                # "knowledge" (something you know),
                                # "biometric" (something you are)
  enrolled_at: timestamp
  last_used: timestamp?
  device_binding: string?       # which device this credential is bound to
                                # (e.g., fingerprint reader ID, camera ID)
  revoked: bool

CredentialType: enum
  # --- Physical (possession) ---
  badge_rfid                    # RFID proximity badge (HID Prox, EM4100)
  badge_nfc                     # NFC smart card (Mifare DESFire, HID iClass SE)
  badge_ble                     # BLE badge/beacon
  phone_ble                     # phone BLE proximity (Ozma app)
  phone_nfc                     # phone NFC tap
  hardware_key                  # FIDO2/WebAuthn hardware key (YubiKey, etc.)
  physical_key                  # traditional physical key (not electronic -- manual only)

  # --- Knowledge ---
  pin                           # numeric PIN code
  password                      # password (API/dashboard login)
  passkey                       # FIDO2 passkey (phone/laptop biometric -> cryptographic)
  totp                          # time-based OTP (authenticator app)

  # --- Biometric ---
  fingerprint                   # fingerprint reader
  face                          # facial recognition (camera + AI)
  iris                          # iris scanner
  voice                         # voice recognition
  palm_vein                     # palm vein scanner

  # --- Network/device ---
  certificate                   # X.509 / Ed25519 certificate (device mesh CA, 802.1X)
  wifi_802_1x                   # WiFi 802.1X authentication (RADIUS)
  vpn_key                       # VPN/WireGuard identity
  ssh_key                       # SSH public key
  oauth_token                   # OAuth2/OIDC token (from IdP)
  api_key                       # static API key
```

#### Authentication events

Every authentication attempt -- success or failure -- MUST be recorded as
an event in the graph that feeds intent bindings, the journal, and access
control decisions:

```yaml
AuthenticationEvent:
  timestamp: timestamp
  person_id: string?            # who (null if unknown/failed identification)
  credential_type: CredentialType  # what method was used
  credential_id: string?        # which specific credential
  device_id: string             # which device performed the authentication
                                # (lock, camera, fingerprint reader, API endpoint)
  location: PhysicalLocation?   # where (derived from the authenticating device's location)
  result: AuthResult
  confidence: float?            # for biometric: match confidence (0.0-1.0)
  multi_factor: bool            # was this part of an MFA sequence?
  mfa_session: string?          # MFA session ID (groups multiple factors)

AuthResult: enum
  success                       # authenticated successfully
  denied_credential             # valid credential, but not authorised for this action
  denied_time                   # valid credential, but outside allowed time window
  denied_zone                   # valid credential, but not allowed in this zone
  failed_no_match               # credential presented but no match found
  failed_expired                # credential matched but expired/revoked
  failed_confidence             # biometric match below confidence threshold
  failed_liveness               # biometric failed liveness check (photo/replay)
  failed_mfa_incomplete         # first factor OK, second factor not provided in time
```

#### Multi-factor composition

Credentials combine for multi-factor authentication. Multi-factor
authentication MUST require credentials from different strength classes
-- two credentials of the same strength class (e.g., two possession
factors) MUST NOT satisfy a two-factor requirement.

```yaml
MfaPolicy:
  name: string                  # "face_and_phone", "badge_and_pin", "any_two_factors"
  required_factors: MfaFactor[]
  timeout_s: uint               # all factors MUST complete within this window
  order: string                 # "any_order", "sequential" (first factor MUST precede second)

MfaFactor:
  credential_types: CredentialType[]  # any of these satisfies this factor
  strength: string                    # "possession", "knowledge", "biometric"
  # Each factor MUST require a different strength class -- face (biometric) +
  # phone proximity (possession) = two-factor. Two badges (possession +
  # possession) = only one factor despite two credentials.
```

**Standard MFA policies**:

| Policy | Factor 1 | Factor 2 | Use case |
|--------|----------|----------|----------|
| `face_and_phone` | Face recognition (biometric) | Phone BLE proximity (possession) | Front door auto-unlock -- recognise the person AND their phone is in range |
| `badge_and_pin` | Badge tap (possession) | PIN entry (knowledge) | Server room -- two-factor physical access |
| `fingerprint_and_badge` | Fingerprint (biometric) | Badge (possession) | High-security zone |
| `passkey` | Passkey (biometric + possession in one -- phone/laptop authenticates) | -- | Dashboard login -- single gesture, two factors |
| `any_biometric` | Any biometric credential | -- | Desk unlock -- face, fingerprint, or voice |

#### Identity in the routing graph

A person's identity affects the graph:

1. **Intent binding**: "When Matt authenticates at his desk, activate his
   personal scenario (monitor layout, audio routing, lighting, thermal)."
   Different people at the same desk get different environments.

2. **Zone-based routing**: "Matt is authenticated in Zone A (desk) -- route
   his keyboard and mouse to his gaming PC. When he authenticates in Zone B
   (couch via phone BLE), route to the TV."

3. **Access control on pipelines**: "Guest users can view the preview
   stream but MUST NOT activate the gaming scenario or access the server
   room's KVM."

4. **Audit trail**: Every authentication event MUST go into the state change
   journal. The audit log (hashchained) MUST record security-relevant
   events with full credential and location detail.

5. **Hot-desking**: Person authenticates at any desk (badge, fingerprint,
   phone) -> their workspace profile activates at that physical location.
   Monitors switch to their layout, audio routes to their preferences,
   their cloud sessions resume.

#### Authentication devices in the graph

Every device that can authenticate a person is a device in the routing
graph with a control path and specific capabilities:

| Device | Credential types | Strength | Connection |
|--------|-----------------|----------|-----------|
| Smart lock | Badge, PIN, fingerprint, BLE | Possession + knowledge + biometric | Z-Wave, Zigbee, BLE, IP |
| Camera (Frigate) | Face recognition | Biometric | IP (existing camera infrastructure) |
| Fingerprint reader (USB) | Fingerprint | Biometric | USB HID |
| NFC reader (USB/desk) | Badge NFC, phone NFC | Possession | USB HID, I2C |
| Intercom (door station) | Face, voice, PIN | Biometric + knowledge | IP, SIP |
| Laptop (agent) | Passkey, fingerprint (built-in), face (Windows Hello/Face ID) | Biometric + possession | Agent reports auth events |
| Phone (KDE Connect) | BLE proximity, NFC tap | Possession | BLE, NFC |
| WiFi AP | 802.1X certificate | Certificate (device/person) | RADIUS |
| VPN endpoint | WireGuard key, certificate | Certificate | Network |
| Dashboard (browser) | Password, passkey, OAuth/OIDC, hardware key | Knowledge + possession + biometric | HTTPS |

These are not new device types -- they are capabilities on existing devices.
A camera is already in the graph for video; face recognition is a
capability. A laptop agent already reports presence; fingerprint/passkey
authentication is an event it can emit. A smart lock is already modelled
(LockSpec). The authentication model unifies them under one identity
framework.

#### PersonPreferences

When a person is identified, their preferences MAY drive the environment:

```yaml
PersonPreferences:
  default_intent: string?       # preferred intent when this person is active
  scenarios: { zone: string, scenario: string }[]?  # per-zone scenario preference
  audio: PersonAudioPrefs?
  display: PersonDisplayPrefs?
  thermal: PersonThermalPrefs?
  lighting: PersonLightingPrefs?

PersonAudioPrefs:
  volume_db: float?             # preferred volume
  output: string?               # preferred speaker set ("headphones", "monitors_a")
  eq_profile: string?           # preferred EQ profile

PersonDisplayPrefs:
  brightness: float?            # preferred display brightness (0-100)
  color_temp_k: uint?           # preferred color temperature
  layout: string?               # preferred multi-monitor layout

PersonThermalPrefs:
  target_temp_c: float?         # preferred room temperature
  fan_profile: string?          # preferred fan noise level ("silent", "balanced")

PersonLightingPrefs:
  scene: string?                # preferred lighting scene
  brightness: float?            # preferred brightness
  color_temp_k: uint?           # preferred color temperature
```

**Example -- hot-desking with person authentication**:

```
Matt badges in at Desk 3 (NFC reader on desk)
  -> AuthenticationEvent: person=matt, credential=badge_nfc, device=desk_3_reader
  -> PersonIdentity lookup: matt -> preferences loaded
  -> Intent binding fires:
    -> Scenario: matt_desktop (monitors to his layout, audio to his preferences)
    -> Lighting: 4000K, 70% brightness (matt's preference)
    -> Thermal: 22C target (matt's preference)
    -> Audio: volume -25dB, output monitors_a (matt's preference)
  -> When Matt leaves (badge out, or occupancy timeout):
    -> Desk returns to neutral state
    -> Scenario deactivates
    -> Ready for next person
```

#### Security considerations

**Biometric data protection**: Biometric data (face templates, fingerprint
minutiae, voiceprints) is special-category personal data under GDPR
Article 9 and equivalent legislation. The following requirements apply:

- **Local storage only**: Biometric templates MUST be stored on the device
  that captured them (camera's local storage, fingerprint reader's secure
  element) or on the local controller. Biometric templates MUST NOT be
  transmitted to Connect. Biometric templates MUST NOT be stored in the
  cloud.
- **Explicit consent**: Biometric enrollment MUST require informed consent
  with clear explanation of what is captured, where it is stored, and how
  to delete it. Implementations MUST NOT perform silent enrollment.
- **Right to erasure**: A person MUST be able to request deletion of all their
  biometric data, authentication history, and associated preferences.
  The system MUST be able to honour this completely.
- **Data minimisation**: Implementations MUST store the minimum necessary --
  comparison templates, not raw images. `credential_id` in
  AuthenticationEvent MUST be an opaque reference, not a template.

**Biometric confidence and false acceptance**: This specification does not
prescribe a default confidence threshold because the acceptable false
acceptance rate (FAR) depends on the deployment:

| Context | Acceptable FAR | Confidence needed | Notes |
|---------|---------------|-------------------|-------|
| Desk personalisation | 1:100 | ~0.85 | Low risk -- wrong person gets wrong wallpaper |
| Front door (residential) | 1:10,000 | ~0.95 + second factor | Medium risk -- MFA REQUIRED |
| Server room | 1:100,000 | ~0.99 + second factor | High risk -- critical infrastructure |
| Access control SHOULD NOT rely on face alone | -- | -- | Implementations SHOULD always require a second factor for physical access |

The biometric provider MUST publish FAR/FRR curves at each confidence
threshold. The deployment configures the threshold based on their risk
tolerance. Biometrics SHOULD NOT be the sole factor for physical access
control -- they SHOULD always be combined with possession (badge, phone) or
knowledge (PIN).

**BLE proximity relay attacks**: BLE signal strength (RSSI) can be relayed
by an attacker with two radios, extending apparent proximity from hundreds
of metres. BLE proximity is a **convenience factor, not a security factor**.
For security-critical access:

- Implementations SHOULD prefer UWB (Ultra-Wideband) -- time-of-flight
  measurement that cannot be relayed without adding detectable latency
- Implementations MUST require a second factor of a different type
  (biometric, knowledge) when BLE proximity is used for security-critical
  access
- Both factors SHOULD be constrained to the same physical zone (intent
  binding condition: `zone_of(face_detection) == zone_of(ble_proximity)`)

**Anti-spoofing for biometrics**:

| Biometric | Attack | Mitigation REQUIRED |
|-----------|--------|-------------------|
| Face | Printed photo | 3D depth sensing (structured light, ToF) |
| Face | Screen replay | 3D depth + liveness (blink, head turn) |
| Face | Deepfake video | 3D depth (deepfakes are 2D) + frame consistency checks |
| Fingerprint | Lifted print on gelatin | Capacitive sensor (detects sub-surface, not surface) |
| Fingerprint | 3D-printed finger | Multi-spectral sensor (blood flow detection) |
| Voice | Recording | Challenge-response (say a random phrase) |
| Voice | Real-time synthesis | Acoustic environment consistency (room signature) |

Biometric authentication devices MUST declare their anti-spoofing
capabilities in the device database (`LockSpec` or sensor spec). The MFA
policy MAY require specific anti-spoofing levels:
`require_3d_liveness: true` for face, `require_capacitive: true` for
fingerprint.

**Credential lifecycle**:

```yaml
CredentialLifecycle:
  max_age_days: uint?           # credential expires after this many days (re-enrollment required)
  max_uses: uint?               # credential expires after this many uses
  lockout: LockoutPolicy?       # what happens on repeated failures
  rotation: RotationPolicy?     # when to re-enroll (biometric templates age)
  recovery: RecoveryPolicy?     # what happens when all credentials are lost

LockoutPolicy:
  max_failures: uint            # lock out after this many consecutive failures
  lockout_duration_s: uint      # lock out for this long (0 = until admin reset)
  escalation: string?           # "notify_admin", "alarm", "camera_snapshot"
  per_credential: bool          # lock out just this credential, or all access?

RotationPolicy:
  biometric_refresh_months: uint?  # re-enroll biometrics this often (face changes)
  badge_expiry_months: uint?       # badges expire and MUST be re-issued
  password_rotation: string?       # "never" (passkeys are better), or interval

RecoveryPolicy:
  methods: string[]             # ["admin_reset", "recovery_code", "supervisor_override",
                                # "physical_key_backup"]
  # Recovery is the hardest part of credential management. If someone
  # loses their badge AND forgets their PIN AND their phone is dead,
  # they need a way in. Options:
  # - Admin reset: another admin unlocks and re-enrolls
  # - Recovery code: pre-generated one-time codes (printed, stored safely)
  # - Supervisor override: a supervisor's credentials unlock on behalf
  # - Physical key: traditional key backup (always works, no batteries)
```

**Enrollment security**: Enrolling a new credential is a privileged
operation -- an attacker who compromises enrollment registers their face as
a legitimate user. The following requirements apply:

- Initial enrollment MUST require admin-level authentication
- Self-enrollment of additional credentials MUST require an existing
  authenticated session (you MUST prove who you are before adding a new
  way to prove who you are)
- All enrollment events MUST be logged in the hashchained audit trail
- Biometric enrollment SHOULD capture an audit photo (separate from the
  template) showing who was physically present during enrollment

**External identity federation**: In enterprise deployments, Ozma is not
the source of truth for identity. The identity model MUST support
federation:

```yaml
IdentityFederation:
  provider: string              # "entra_id", "okta", "google_workspace",
                                # "jumpcloud", "freeipa", "ldap", "oidc_generic"
  person_mapping: string        # how external identity maps to PersonIdentity
                                # ("email", "employee_id", "upn", "sub_claim")
  credential_sync: bool         # sync external credentials (AD badge enrollment -> Ozma lock)
  group_mapping: GroupMapping[]? # external groups -> Ozma roles/zones
  # Ozma SHOULD NOT require being the sole IdP. In enterprise:
  # - Identity comes from AD/Entra/Okta
  # - Ozma maps external identity to its PersonIdentity model
  # - Group membership determines roles, zones, scenarios
  # - Credential management MAY be delegated (HR enrolls badges in AD,
  #   Ozma syncs badge->lock access automatically)
```

**Physical security separation**: Authentication for physical access
(locks) and authentication for digital access (dashboard, API, SSH) MUST
be independently configurable. Unlocking the front door MUST NOT grant
API admin access. Logging into the dashboard MUST NOT unlock the server
room. The `roles` and `zones` on PersonIdentity enforce this -- but this
specification explicitly states: **physical access and digital access MUST
be independent authorization domains**. A person MAY be authorized for
physical access to a zone without having any digital access to the systems
in that zone.

#### Security posture

A home user who is happy with face recognition at 0.85 confidence for their
front door is making a reasonable decision for their context. An auditor
reviewing a SOC 2 environment would reject the same configuration. Both
are valid -- they have different security postures.

A security posture is a named policy that cascades through the entire
system -- not just authentication, but encryption, audit, access control,
data retention, and operational security. The posture determines the
acceptable trade-off between convenience and rigour.

```yaml
SecurityPosture:
  id: string                    # "home", "small_business", "regulated", "high_security"
  name: string
  description: string

  # --- Authentication ---
  auth: PostureAuthPolicy
  # --- Encryption ---
  encryption: PostureEncryptionPolicy
  # --- Audit ---
  audit: PostureAuditPolicy
  # --- Data retention ---
  retention: PostureRetentionPolicy
  # --- Operational ---
  operational: PostureOperationalPolicy

PostureAuthPolicy:
  min_factors_physical: uint    # minimum factors for physical access control
  min_factors_digital: uint     # minimum factors for digital access (API, dashboard)
  min_factors_admin: uint       # minimum factors for admin/destructive operations
  biometric_min_confidence: float?  # minimum confidence threshold for biometric match
  biometric_require_liveness: bool  # require anti-spoofing liveness check
  biometric_require_3d: bool?   # require 3D depth sensing for face
  ble_proximity_allowed_as_factor: bool  # allow BLE proximity as a security factor
  password_min_length: uint?
  password_require_complexity: bool?
  session_max_age_hours: uint?  # maximum session duration before re-auth
  idle_timeout_minutes: uint?   # lock after idle
  credential_max_age_days: uint?  # maximum credential lifetime
  lockout_after_failures: uint  # lock after this many failures
  lockout_duration_s: uint      # lockout time
  enrollment_requires_admin: bool  # new credential enrollment needs admin approval
  federation_allowed: bool      # allow external IdP federation

PostureEncryptionPolicy:
  data_plane: string            # "required", "preferred", "none"
  control_plane: string         # "tls_required", "tls_preferred"
  at_rest: string               # "required" (all stored data), "sensitive_only", "none"
  key_rotation_days: uint?      # rotate session keys this often
  min_key_strength: string      # "128bit", "256bit"
  approved_ciphers: string[]?   # whitelist of acceptable ciphers
  pfs_required: bool            # require perfect forward secrecy

PostureAuditPolicy:
  journal_enabled: bool         # state change journal REQUIRED
  journal_retention_days: uint  # minimum journal retention
  hashchain_enabled: bool       # hashchained audit log REQUIRED
  auth_events_logged: bool      # all authentication events logged
  access_events_logged: bool    # all access control events logged
  config_changes_logged: bool   # all configuration changes logged
  export_required: bool?        # MUST export to external SIEM
  tamper_detection: bool        # detect and alert on journal tampering
  review_interval_days: uint?   # how often audit logs MUST be reviewed

PostureRetentionPolicy:
  auth_log_retention_days: uint # how long to keep authentication logs
  access_log_retention_days: uint
  video_retention_days: uint?   # camera footage retention
  metric_retention_days: uint   # monitoring metric retention
  min_backup_frequency_days: uint  # maximum time between backups
  backup_encryption_required: bool

PostureOperationalPolicy:
  auto_update_allowed: bool     # can devices auto-update without approval
  remote_access_allowed: bool   # is remote access via Connect relay permitted
  remote_access_requires_mfa: bool
  agent_resource_budget_max_percent: float  # max resources agent MAY use on target machines
  unmanaged_devices_allowed: bool  # can unmanaged devices join the network
  usb_device_policy: string     # "allow_all", "allow_known", "block_unknown"
  firmware_policy: string       # "auto_update", "notify", "manual", "pin_versions"
  vulnerability_response: string  # "auto_patch", "notify_and_schedule", "manual"
```

**Built-in postures**:

| Posture | Auth | Encryption | Audit | Target environment |
|---------|------|-----------|-------|-------------------|
| `home` | Single factor for everything. Face at 0.85. BLE as factor allowed. No idle timeout. Passwords optional. | Preferred, not required. | Journal on, 30-day retention, no hashchain. | Personal home, homelab. Convenience first. |
| `small_business` | MFA for admin. Single factor for physical. Face at 0.90. 30-min idle timeout. Passwords required. | Required on LAN. | Journal + hashchain. 90-day retention. Auth events logged. | Small office, 5-50 people. Balance of security and convenience. |
| `regulated` | MFA for everything. Face at 0.95 + second factor. No BLE as security factor. 15-min idle timeout. Credential rotation. | Required everywhere. 256-bit minimum. PFS required. | Full audit trail. 1-year retention. SIEM export required. Review every 90 days. | SOC 2, ISO 27001, HIPAA, PCI-DSS. Compliance-driven. |
| `high_security` | MFA with hardware token for admin. Face at 0.99 + badge + PIN for physical. 5-min idle timeout. Admin enrollment only. No federation. | Required, approved ciphers only. At-rest encryption required. 30-day key rotation. | Full hashchained audit. 7-year retention. SIEM + tamper detection. Monthly review. | Government, defence, critical infrastructure. Rigour above all. |
| `custom` | Per-field configuration. | Per-field. | Per-field. | Anything that does not fit the built-in postures. |

**Posture is inherited and overridable**: A site has a posture. A space
within the site MAY override specific fields (server room within a
`small_business` site uses `high_security` authentication for physical
access). A device MAY override its posture for specific operations
(this lock requires MFA even though the site posture does not). The
cascade is: site -> space -> zone -> device, with each level able to
**tighten** but never **loosen** the parent's posture. A child posture
MUST NOT loosen a parent posture's constraints.

**Posture validation**: The system MUST validate the current configuration
against the declared posture and report compliance gaps:

```
GET /api/v1/security/posture                # current posture and compliance status
GET /api/v1/security/posture/gaps           # where current config does not meet posture
POST /api/v1/security/posture/validate      # validate a hypothetical config change
```

Example gap report:
```
Posture: regulated
Gaps:
  - FAIL: Lock "server_room_door" has single-factor authentication (badge only).
    Required: min_factors_physical = 2. Fix: add PIN or biometric.
  - FAIL: Dashboard session timeout is 60 minutes.
    Required: idle_timeout_minutes = 15. Fix: reduce to 15.
  - WARN: 3 devices have firmware older than 90 days.
    Policy: vulnerability_response = "notify_and_schedule". Action: schedule updates.
  - PASS: All data plane encryption is enabled.
  - PASS: Hashchained audit log is active with 365-day retention.
  - PASS: All authentication events are logged.
```

This is how auditors interact with the system -- they set the posture to
`regulated`, run the gap report, and see exactly what needs to change.
It is also how a home user interacts -- they set `home` and never think
about it again. The posture is the policy; the gap report is the evidence;
the spec defines both.

**Posture enforcement**: The tighten-only inheritance rule MUST be
**enforced at write time** -- an API call that would loosen the effective
posture at any level MUST be rejected with an error explaining which parent
posture field constrains it. To intentionally deviate from posture, an
admin creates a **posture exception**:

```yaml
PostureException:
  id: string
  target: string                # device/zone/space the exception applies to
  field: string                 # which posture field is overridden ("auth.min_factors_physical")
  override_value: any           # the exception value
  justification: string         # why this exception exists (REQUIRED, auditable)
  approved_by: string           # who approved it (person identity)
  created_at: timestamp
  expires_at: timestamp?        # when the exception auto-expires (null = permanent until revoked)
  review_interval_days: uint?   # how often this exception MUST be re-approved
```

Posture exceptions MUST have a justification, an approver, and an expiry
(or explicit permanent status with periodic review). Exceptions appear in
the gap report as a separate category -- distinct from violations. An
auditor sees: "3 exceptions granted (with justifications, approvers, and
expiry dates)" vs "2 violations (no exception, must be fixed)". Expired
exceptions automatically become violations.

**Continuous posture monitoring**: Posture compliance MUST be evaluated on
every state change, not just when the gap report is run. Events MUST fire
in real time:

```
security.posture.violation      # a change made the system non-compliant
security.posture.exception_created  # an approved exception was granted
security.posture.exception_expired  # an exception expired -> now a violation
security.posture.restored       # a previous violation was fixed
security.posture.drift          # gradual drift -- multiple small changes aggregate
```

These feed notification sinks (Slack alert on violation), the hashchained
audit log, and the monitoring dashboard. The gap report is a snapshot; the
events are the continuous feed.

**Incident response posture**: What happens when a violation is detected
depends on the posture:

```yaml
PostureIncidentResponse:
  on_violation: string          # "log_only", "alert", "alert_and_require_ack",
                                # "lock_affected", "lock_zone", "emergency_lockdown"
  ack_required_within_hours: uint?  # time to acknowledge before escalation
  escalation: string?           # "notify_admin", "notify_all_admins", "lock_zone",
                                # "disable_affected_device", "external_webhook"
  auto_remediate: bool?         # attempt to fix automatically (e.g., re-enable MFA
                                # that was disabled, re-lock a door that was held open)
```

| Posture | on_violation | ack_required | escalation |
|---------|-------------|--------------|------------|
| `home` | `log_only` | -- | -- |
| `small_business` | `alert` | 48h | `notify_admin` |
| `regulated` | `alert_and_require_ack` | 24h | `notify_all_admins` + `external_webhook` |
| `high_security` | `lock_affected` | 1h | `lock_zone` + `external_webhook` |

**Network segmentation posture**: Added to `PostureOperationalPolicy`:

```yaml
  # --- Network segmentation ---
  require_management_vlan: bool     # management traffic on separate VLAN
  require_iot_isolation: bool       # IoT devices on isolated VLAN (default-deny)
  require_camera_isolation: bool    # camera traffic segregated
  require_server_no_direct_internet: bool  # servers MUST NOT reach internet directly
  require_guest_isolation: bool     # guest WiFi isolated from production
```

These map directly to the existing IoT VLAN management (V1.6) and router
mode. The posture validates that the network configuration meets the
declared segmentation requirements.

**USB device policy -- granular**:

```yaml
  usb_device_policy: UsbDevicePolicy

UsbDevicePolicy:
  default_action: string        # "allow", "block", "prompt"
  rules: UsbPolicyRule[]        # per-class or per-device rules

UsbPolicyRule:
  match: string                 # "class:hid" (keyboards/mice), "class:storage",
                                # "class:wireless" (WiFi/BT adapters), "class:video",
                                # "vid_pid:1234:5678" (specific device),
                                # "db:known" (in device database), "db:unknown"
  action: string                # "allow", "block", "prompt", "read_only" (storage)
  zones: string[]?              # applies only in these zones (null = everywhere)
  log: bool                     # log when this rule triggers
  # Example regulated policy:
  # - HID devices: allow (keyboards and mice always work)
  # - Known storage: prompt (user MUST approve, logged)
  # - Unknown storage: block (no unknown USB drives)
  # - Wireless adapters: block in server zones (rogue AP risk)
  # - Video devices (capture cards): allow
  # - Everything else: block
```

**Home posture hardening**: Even the `home` posture MUST require MFA for
destructive admin operations (factory reset, credential enrollment, audit
log deletion, posture change). Single factor remains acceptable for daily
use (physical access, scenario switching, dashboard viewing), but actions
that cannot be undone MUST require a second factor. This prevents:
compromised phone BLE = full admin access including credential enrollment
of attacker's face.
