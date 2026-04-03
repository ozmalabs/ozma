/**
 * Stream target definitions and persistence.
 *
 * A StreamTarget describes where to send the camera stream:
 *   ozma        — POST SDP offer to the controller's WHIP endpoint
 *   whip_external — POST SDP offer directly to an external WHIP server
 *                   (OBS 29+, Cloudflare Stream, Mux, etc.)
 *   rtmp_relay  — POST SDP offer to the controller's WHIP endpoint;
 *                   controller ffmpegs the stream to the supplied RTMP URL.
 *                   Mobile never needs an RTMP library.
 *
 * Saved targets are persisted to MMKV as a JSON array under STORAGE_KEY.
 */

import {MMKV} from 'react-native-mmkv';

// ── Types ─────────────────────────────────────────────────────────────────────

export type StreamTargetType = 'ozma' | 'whip_external' | 'rtmp_relay';

export interface StreamTarget {
  id: string;
  name: string;
  type: StreamTargetType;
  /**
   * For ozma: the WHIP endpoint URL (derived from controller base URL at
   * runtime — see buildOzmaTarget).
   * For whip_external: the external WHIP endpoint URL.
   * For rtmp_relay: the RTMP URL sent as the relay_rtmp query parameter
   * to the controller's WHIP endpoint.
   */
  url: string;
  /**
   * rtmp_relay only.  The WHIP endpoint on the controller that accepts the
   * WebRTC ingest and pipes it to url via ffmpeg.
   * Populated automatically when building an rtmp_relay target.
   */
  relayWhipEndpoint?: string;
}

// ── Storage ───────────────────────────────────────────────────────────────────

const STORAGE_KEY = 'webcam_targets';

const storage = new MMKV({id: 'ozma-webcam'});

export function loadTargets(): StreamTarget[] {
  try {
    const raw = storage.getString(STORAGE_KEY);
    if (!raw) {
      return [];
    }
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) {
      return [];
    }
    return parsed as StreamTarget[];
  } catch {
    return [];
  }
}

export function saveTargets(targets: StreamTarget[]): void {
  storage.set(STORAGE_KEY, JSON.stringify(targets));
}

export function addTarget(target: StreamTarget): StreamTarget[] {
  const targets = loadTargets();
  // Overwrite if same id
  const idx = targets.findIndex(t => t.id === target.id);
  if (idx >= 0) {
    targets[idx] = target;
  } else {
    targets.push(target);
  }
  saveTargets(targets);
  return targets;
}

export function removeTarget(id: string): StreamTarget[] {
  const targets = loadTargets().filter(t => t.id !== id);
  saveTargets(targets);
  return targets;
}

// ── Factory helpers ───────────────────────────────────────────────────────────

/**
 * Build an Ozma controller target from the stored controller base URL.
 * The WHIP endpoint is always at /api/v1/cameras/whip on the controller.
 */
export function buildOzmaTarget(
  controllerBaseUrl: string,
  name = 'Ozma Controller',
): StreamTarget {
  const base = controllerBaseUrl.replace(/\/$/, '');
  return {
    id: 'ozma-default',
    name,
    type: 'ozma',
    url: `${base}/api/v1/cameras/whip`,
  };
}

/**
 * Build an external WHIP target (Cloudflare Stream, OBS 29+, Mux, etc.).
 */
export function buildExternalWhipTarget(
  id: string,
  name: string,
  whipUrl: string,
): StreamTarget {
  return {id, name, type: 'whip_external', url: whipUrl};
}

/**
 * Build an RTMP relay target.
 * The mobile sends WHIP to the controller; the controller's ffmpeg pushes
 * to rtmpUrl.
 */
export function buildRtmpRelayTarget(
  id: string,
  name: string,
  rtmpUrl: string,
  controllerBaseUrl: string,
): StreamTarget {
  const base = controllerBaseUrl.replace(/\/$/, '');
  const relayWhipEndpoint = `${base}/api/v1/cameras/whip?relay_rtmp=${encodeURIComponent(rtmpUrl)}`;
  return {
    id,
    name,
    type: 'rtmp_relay',
    url: rtmpUrl,
    relayWhipEndpoint,
  };
}

/**
 * Return the actual WHIP endpoint URL to POST the SDP offer to for a given target.
 * For rtmp_relay targets the relay_rtmp parameter is baked into relayWhipEndpoint.
 */
export function getWhipEndpoint(target: StreamTarget): string {
  switch (target.type) {
    case 'ozma':
      return target.url;
    case 'whip_external':
      return target.url;
    case 'rtmp_relay':
      return target.relayWhipEndpoint ?? target.url;
  }
}
