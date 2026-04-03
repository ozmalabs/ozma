/**
 * WHIP (WebRTC HTTP Ingest Protocol) client.
 *
 * WHIP is intentionally simple:
 *   POST  <endpoint>  application/sdp  → 201 Created, Location header, SDP answer body
 *   PATCH <sessionUrl> application/trickle-ice-sdpfrag  → 204
 *   DELETE <sessionUrl> → 200 / 204
 *
 * The mobile client sends the SDP offer to the WHIP endpoint and receives an
 * answer.  ICE candidates trickled after the initial exchange are sent via
 * PATCH to the session URL returned in the Location header.
 *
 * For Ozma controller sessions, pass bearerToken so the JWT auth header is
 * included.  For external WHIP endpoints (Cloudflare Stream, OBS 29+, Mux)
 * leave bearerToken undefined.
 */

import {
  RTCPeerConnection,
  RTCSessionDescription,
  mediaDevices,
  type MediaStream,
} from 'react-native-webrtc';

// ── Types ─────────────────────────────────────────────────────────────────────

export interface WHIPSession {
  /** URL returned in the Location header from the initial POST response. */
  sessionUrl: string;
  peerConnection: RTCPeerConnection;
}

export interface WHIPOptions {
  /** Bearer token for Ozma controller authentication. */
  bearerToken?: string;
  /**
   * ICE server configuration.  Defaults to Google STUN if not provided.
   */
  iceServers?: RTCIceServer[];
  /**
   * Maximum video bitrate in kbps.  Defaults to 2000 (2 Mbps).
   * Injected into the SDP offer via b=AS: line.
   */
  maxBitrateKbps?: number;
}

interface RTCIceServer {
  urls: string | string[];
  username?: string;
  credential?: string;
}

// ── Default ICE servers ───────────────────────────────────────────────────────

const DEFAULT_ICE_SERVERS: RTCIceServer[] = [
  {urls: 'stun:stun.l.google.com:19302'},
  {urls: 'stun:stun1.l.google.com:19302'},
];

// ── SDP helpers ───────────────────────────────────────────────────────────────

/**
 * Inject a bandwidth constraint into the SDP offer.
 * Finds the video m= section and adds b=AS:<kbps> if not already present.
 */
function injectBandwidth(sdp: string, maxKbps: number): string {
  const lines = sdp.split('\r\n');
  const result: string[] = [];
  let inVideo = false;

  for (const line of lines) {
    if (line.startsWith('m=video')) {
      inVideo = true;
    } else if (line.startsWith('m=')) {
      inVideo = false;
    }

    result.push(line);

    // Insert b=AS after the c= line in the video section
    if (inVideo && line.startsWith('c=') && !lines.includes(`b=AS:${maxKbps}`)) {
      result.push(`b=AS:${maxKbps}`);
      result.push(`b=TIAS:${maxKbps * 1000}`);
      inVideo = false; // only inject once
    }
  }

  return result.join('\r\n');
}

/**
 * Force send-only direction in the SDP offer.
 * Changes "sendrecv" → "sendonly" and removes "recvonly" entries.
 */
function forceSendOnly(sdp: string): string {
  return sdp
    .replace(/a=sendrecv\r\n/g, 'a=sendonly\r\n')
    .replace(/a=recvonly\r\n/g, 'a=sendonly\r\n');
}

// ── WHIPClient ────────────────────────────────────────────────────────────────

export class WHIPClient {
  private readonly _defaultIceServers: RTCIceServer[];

  constructor(extraStunUrls?: string[]) {
    this._defaultIceServers = [...DEFAULT_ICE_SERVERS];
    if (extraStunUrls) {
      for (const url of extraStunUrls) {
        this._defaultIceServers.push({urls: url});
      }
    }
  }

  /**
   * Start a WHIP session.
   *
   * 1. Creates an RTCPeerConnection with H.264 + Opus constraints.
   * 2. Adds the provided local MediaStream tracks.
   * 3. Creates an SDP offer (send-only, H.264 Baseline, max 2 Mbps).
   * 4. POSTs the offer to whipEndpoint.
   * 5. Sets the remote answer from the 201 response body.
   * 6. Wires up ICE candidate trickle via PATCH.
   *
   * Returns a WHIPSession containing the sessionUrl and RTCPeerConnection.
   */
  async startSession(
    whipEndpoint: string,
    stream: MediaStream,
    options: WHIPOptions = {},
  ): Promise<WHIPSession> {
    const {
      bearerToken,
      iceServers = this._defaultIceServers,
      maxBitrateKbps = 2000,
    } = options;

    const pc = new RTCPeerConnection({
      iceServers,
      // Prefer H.264 for widest server-side compatibility
      sdpSemantics: 'unified-plan',
    } as any);

    // Add all tracks from the local stream as send-only transceivers
    for (const track of stream.getTracks()) {
      pc.addTransceiver(track, {direction: 'sendonly'});
    }

    // Create SDP offer
    const offerInit = await pc.createOffer({
      offerToReceiveAudio: false,
      offerToReceiveVideo: false,
    });

    if (!offerInit.sdp) {
      await pc.close();
      throw new Error('Failed to generate SDP offer');
    }

    let offerSdp = forceSendOnly(offerInit.sdp);
    offerSdp = injectBandwidth(offerSdp, maxBitrateKbps);

    await pc.setLocalDescription(
      new RTCSessionDescription({type: 'offer', sdp: offerSdp}),
    );

    // POST offer to WHIP endpoint
    const headers: Record<string, string> = {
      'Content-Type': 'application/sdp',
    };
    if (bearerToken) {
      headers['Authorization'] = `Bearer ${bearerToken}`;
    }

    const response = await fetch(whipEndpoint, {
      method: 'POST',
      headers,
      body: offerSdp,
    });

    if (response.status !== 201) {
      await pc.close();
      const body = await response.text();
      throw new WHIPError(response.status, body || `WHIP offer rejected: HTTP ${response.status}`);
    }

    const locationHeader = response.headers.get('Location');
    if (!locationHeader) {
      await pc.close();
      throw new Error('WHIP server did not return a Location header');
    }

    // Resolve relative Location URLs against the endpoint base
    const sessionUrl = resolveUrl(whipEndpoint, locationHeader);

    const answerSdp = await response.text();
    await pc.setRemoteDescription(
      new RTCSessionDescription({type: 'answer', sdp: answerSdp}),
    );

    // Wire up ICE trickle — send candidates to the session URL via PATCH
    pc.onicecandidate = async (event: any) => {
      if (event.candidate) {
        await this._trickleCandidates(sessionUrl, [event.candidate], bearerToken);
      }
    };

    return {sessionUrl, peerConnection: pc};
  }

  /**
   * Gracefully terminate a WHIP session.
   * Sends DELETE to the session URL and closes the RTCPeerConnection.
   */
  async endSession(session: WHIPSession, bearerToken?: string): Promise<void> {
    const headers: Record<string, string> = {};
    if (bearerToken) {
      headers['Authorization'] = `Bearer ${bearerToken}`;
    }

    try {
      await fetch(session.sessionUrl, {method: 'DELETE', headers});
    } catch {
      // Best-effort; the server may already be gone
    }

    try {
      await session.peerConnection.close();
    } catch {
      // Already closed
    }
  }

  /**
   * Send ICE candidates via PATCH (trickle ICE).
   * Constructs an SDP fragment from the candidates.
   */
  private async _trickleCandidates(
    sessionUrl: string,
    candidates: RTCIceCandidate[],
    bearerToken?: string,
  ): Promise<void> {
    if (!candidates.length) {
      return;
    }

    const sdpFrag = candidates
      .map(c => {
        const candStr = (c as any).candidate ?? String(c);
        return candStr.startsWith('a=') ? candStr : `a=${candStr}`;
      })
      .join('\r\n');

    const headers: Record<string, string> = {
      'Content-Type': 'application/trickle-ice-sdpfrag',
    };
    if (bearerToken) {
      headers['Authorization'] = `Bearer ${bearerToken}`;
    }

    try {
      await fetch(sessionUrl, {
        method: 'PATCH',
        headers,
        body: sdpFrag,
      });
    } catch {
      // Non-fatal — ICE will still complete via host candidates in the offer
    }
  }
}

// ── Errors ────────────────────────────────────────────────────────────────────

export class WHIPError extends Error {
  constructor(
    public readonly status: number,
    public readonly detail: string,
  ) {
    super(`WHIP error ${status}: ${detail}`);
    this.name = 'WHIPError';
  }
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/**
 * Resolve a possibly-relative Location URL against the endpoint that produced it.
 */
function resolveUrl(base: string, location: string): string {
  if (location.startsWith('http://') || location.startsWith('https://')) {
    return location;
  }
  // Parse base URL to extract origin
  try {
    const url = new URL(base);
    if (location.startsWith('/')) {
      return `${url.protocol}//${url.host}${location}`;
    }
    // Relative path — resolve against directory
    const dir = url.pathname.substring(0, url.pathname.lastIndexOf('/') + 1);
    return `${url.protocol}//${url.host}${dir}${location}`;
  } catch {
    return location;
  }
}

/**
 * Helper: acquire camera + microphone MediaStream with the requested constraints.
 * Exported so WebcamScreen and useWebcam can share the same constraints logic.
 */
export async function getUserMedia(
  facingMode: 'user' | 'environment',
  resolution: '480p' | '720p' | '1080p',
): Promise<MediaStream> {
  const resolutions = {
    '480p': {width: 854, height: 480},
    '720p': {width: 1280, height: 720},
    '1080p': {width: 1920, height: 1080},
  };
  const {width, height} = resolutions[resolution];

  return mediaDevices.getUserMedia({
    audio: {
      echoCancellation: true,
      noiseSuppression: true,
      sampleRate: 48000,
      channelCount: 1,
    },
    video: {
      facingMode,
      width: {ideal: width},
      height: {ideal: height},
      frameRate: {ideal: 30, max: 30},
    },
  }) as Promise<MediaStream>;
}
