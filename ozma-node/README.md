# ozma-node

The Ozma node binary: capture, input, mDNS/controller registration, the HTTP
API, WebRTC/HLS media, USB gadget control, and the optional RFB/VNC server.

## RFB/VNC server

The node can serve its first capture source as an RFB 3.8 endpoint (accepting
3.3, 3.7, and 3.8 clients) with `--vnc-server`. It is opt-in and registers only
the endpoint it actually bound.

```sh
# Loopback, no password (default).
ozma-node --vnc-server

# Loopback with RFB VNC authentication (security type 2).
install -m 0600 /dev/null /run/ozma/vnc.pass
printf 'my-secret' > /run/ozma/vnc.pass
ozma-node --vnc-server --vnc-password-file /run/ozma/vnc.pass

# Non-loopback requires a password (preferred) or an explicit override.
ozma-node --vnc-server --vnc-bind 0.0.0.0 --vnc-password-file /run/ozma/vnc.pass
ozma-node --vnc-server --vnc-bind 0.0.0.0 --vnc-allow-unauthenticated
```

| Flag | Env | Default | Notes |
|---|---|---|---|
| `--vnc-server` | `OZMA_VNC_SERVER` | off | Requires a capture source. |
| `--vnc-bind` | `OZMA_VNC_BIND` | `127.0.0.1` | Any non-loopback bind is an explicit widening. |
| `--vnc-server-port` | `OZMA_VNC_SERVER_PORT` | `0` | `0` asks the OS for a port; the real port is registered. |
| `--vnc-password-file` | `OZMA_VNC_PASSWORD_FILE` | unset | Read once at startup; enables security type 2. |
| `--vnc-allow-unauthenticated` | `OZMA_VNC_ALLOW_UNAUTHENTICATED` | off | Non-loopback bind without a password. |
| `--vnc-max-clients` | `OZMA_VNC_MAX_CLIENTS` | `4` | 1..=32 concurrent viewers. |
| `--vnc-idle-timeout-seconds` | `OZMA_VNC_IDLE_TIMEOUT_SECONDS` | `0` | 1..=86400, or `0` to disable. |

### Password file policy

- The file is trimmed of exactly one trailing newline (`\n` or `\r\n`).
- Empty files are rejected.
- Passwords longer than 8 bytes are rejected: RFB VNC authentication uses one
  DES key, so extra bytes would be ignored.
- On Unix the file must be `0600` or stricter (no group/other permissions and
  no owner execute bit).

### Security warning

VNC password authentication is a DES challenge/response that proves knowledge
of a shared password. **It is not encryption.** The RFB transport is plain TCP:
pixels, input, and clipboard data are not protected, and classic VNC DES is
weak by modern standards. Use loopback, a trusted network, WireGuard/mesh, or
an SSH tunnel when the link is not trusted. There is no TLS or VeNCrypt.

The password is never logged and never appears in `/api/v1/connection`, mDNS
TXT records, registration payloads, or `Debug` output. Registration advertises
only the host/port; the controller must be configured with the password.
