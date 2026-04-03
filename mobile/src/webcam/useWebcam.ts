/**
 * useWebcam — React hook managing the full phone-as-webcam lifecycle.
 *
 * States:
 *   idle              — nothing happening; no camera permission yet checked
 *   requesting_permission — asking the OS for camera/mic permission
 *   previewing        — camera open, local stream displayed, not streaming
 *   connecting        — WHIP handshake in progress
 *   streaming         — connected and sending media to a WHIP endpoint
 *   error             — something went wrong; errorMessage is set
 *
 * Stats polling: every 2 s while streaming, using peerConnection.getStats().
 * Reconnection: exponential backoff from 3 s to 30 s max on network errors.
 */

import {useCallback, useEffect, useRef, useState} from 'react';
import {
  mediaDevices,
  type MediaStream,
} from 'react-native-webrtc';

import {WHIPClient, WHIPSession, getUserMedia, WHIPError} from './WHIPClient';
import {StreamTarget, getWhipEndpoint} from './StreamTargets';
import {useAuth} from '../auth/useAuth';

// ── Types ─────────────────────────────────────────────────────────────────────

export type WebcamStatus =
  | 'idle'
  | 'requesting_permission'
  | 'previewing'
  | 'connecting'
  | 'streaming'
  | 'error';

export type Resolution = '480p' | '720p' | '1080p';

export interface WebcamStats {
  bytesOut: number;
  packetsLost: number;
  fps: number;
  bitrate: number; // kbps
}

export interface WebcamState {
  status: WebcamStatus;
  localStream: MediaStream | null;
  activeSession: WHIPSession | null;
  activeTarget: StreamTarget | null;
  errorMessage: string | null;
  stats: WebcamStats | null;
  facingMode: 'user' | 'environment';
  resolution: Resolution;
  streamDuration: number; // seconds
}

export interface WebcamActions {
  requestPermission(): Promise<boolean>;
  startPreview(): Promise<void>;
  stopPreview(): void;
  startStreaming(target: StreamTarget): Promise<void>;
  stopStreaming(): Promise<void>;
  toggleCamera(): void;
  setResolution(r: Resolution): void;
  refreshStats(): Promise<void>;
}

// ── Constants ─────────────────────────────────────────────────────────────────

const STATS_POLL_INTERVAL_MS = 2000;
const RECONNECT_BASE_MS = 3000;
const RECONNECT_MAX_MS = 30000;

// ── Hook ──────────────────────────────────────────────────────────────────────

export function useWebcam(): WebcamState & WebcamActions {
  const {tokens} = useAuth();

  const [status, setStatus] = useState<WebcamStatus>('idle');
  const [localStream, setLocalStream] = useState<MediaStream | null>(null);
  const [activeSession, setActiveSession] = useState<WHIPSession | null>(null);
  const [activeTarget, setActiveTarget] = useState<StreamTarget | null>(null);
  const [errorMessage, setErrorMessage] = useState<string | null>(null);
  const [stats, setStats] = useState<WebcamStats | null>(null);
  const [facingMode, setFacingMode] = useState<'user' | 'environment'>('environment');
  const [resolution, setResolution] = useState<Resolution>('720p');
  const [streamDuration, setStreamDuration] = useState(0);

  // Refs for mutable values accessed inside callbacks
  const whipClient = useRef(new WHIPClient());
  const statsTimer = useRef<ReturnType<typeof setInterval> | null>(null);
  const durationTimer = useRef<ReturnType<typeof setInterval> | null>(null);
  const reconnectTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const reconnectDelay = useRef(RECONNECT_BASE_MS);
  const activeTargetRef = useRef<StreamTarget | null>(null);
  const activeSessionRef = useRef<WHIPSession | null>(null);
  const localStreamRef = useRef<MediaStream | null>(null);
  const facingModeRef = useRef(facingMode);
  const resolutionRef = useRef(resolution);
  const tokenRef = useRef(tokens?.access_token ?? null);
  const isMounted = useRef(true);

  // Keep refs in sync
  useEffect(() => { activeTargetRef.current = activeTarget; }, [activeTarget]);
  useEffect(() => { activeSessionRef.current = activeSession; }, [activeSession]);
  useEffect(() => { localStreamRef.current = localStream; }, [localStream]);
  useEffect(() => { facingModeRef.current = facingMode; }, [facingMode]);
  useEffect(() => { resolutionRef.current = resolution; }, [resolution]);
  useEffect(() => { tokenRef.current = tokens?.access_token ?? null; }, [tokens]);

  useEffect(() => {
    isMounted.current = true;
    return () => {
      isMounted.current = false;
      _clearTimers();
    };
  }, []);

  // ── Helpers ─────────────────────────────────────────────────────────────────

  function _setError(msg: string) {
    if (!isMounted.current) { return; }
    setStatus('error');
    setErrorMessage(msg);
  }

  function _clearTimers() {
    if (statsTimer.current) { clearInterval(statsTimer.current); statsTimer.current = null; }
    if (durationTimer.current) { clearInterval(durationTimer.current); durationTimer.current = null; }
    if (reconnectTimer.current) { clearTimeout(reconnectTimer.current); reconnectTimer.current = null; }
  }

  function _stopLocalStream() {
    const stream = localStreamRef.current;
    if (stream) {
      stream.getTracks().forEach(t => t.stop());
      setLocalStream(null);
    }
  }

  // ── Public actions ───────────────────────────────────────────────────────────

  const requestPermission = useCallback(async (): Promise<boolean> => {
    setStatus('requesting_permission');
    setErrorMessage(null);
    try {
      // getUserMedia triggers the OS permission dialog
      const stream = await getUserMedia(facingModeRef.current, resolutionRef.current);
      // Immediately stop tracks — we only wanted the permission
      stream.getTracks().forEach(t => t.stop());
      if (isMounted.current) { setStatus('idle'); }
      return true;
    } catch (err: any) {
      const msg = err?.message ?? String(err);
      if (msg.includes('Permission') || msg.includes('NotAllowed')) {
        _setError('Camera or microphone permission denied. Please enable in Settings.');
      } else {
        _setError(`Permission request failed: ${msg}`);
      }
      return false;
    }
  }, []);

  const startPreview = useCallback(async (): Promise<void> => {
    if (status === 'streaming') { return; }
    setErrorMessage(null);

    // Stop any existing stream first
    _stopLocalStream();

    try {
      const stream = await getUserMedia(facingModeRef.current, resolutionRef.current);
      if (!isMounted.current) {
        stream.getTracks().forEach(t => t.stop());
        return;
      }
      setLocalStream(stream);
      setStatus('previewing');
    } catch (err: any) {
      const msg = err?.message ?? String(err);
      if (msg.includes('Permission') || msg.includes('NotAllowed')) {
        _setError('Camera or microphone permission denied. Please enable in Settings.');
      } else {
        _setError(`Failed to open camera: ${msg}`);
      }
    }
  }, [status]);

  const stopPreview = useCallback((): void => {
    _stopLocalStream();
    if (isMounted.current) {
      setStatus('idle');
    }
  }, []);

  const startStreaming = useCallback(async (target: StreamTarget): Promise<void> => {
    if (status === 'streaming' || status === 'connecting') { return; }
    setErrorMessage(null);
    setStatus('connecting');

    // Ensure we have a local stream
    let stream = localStreamRef.current;
    if (!stream) {
      try {
        stream = await getUserMedia(facingModeRef.current, resolutionRef.current);
        if (!isMounted.current) { stream.getTracks().forEach(t => t.stop()); return; }
        setLocalStream(stream);
      } catch (err: any) {
        _setError(`Failed to open camera: ${err?.message ?? err}`);
        return;
      }
    }

    const whipEndpoint = getWhipEndpoint(target);
    const bearerToken = target.type !== 'whip_external' ? (tokenRef.current ?? undefined) : undefined;

    try {
      const session = await whipClient.current.startSession(whipEndpoint, stream, {
        bearerToken,
        maxBitrateKbps: resolutionRef.current === '1080p' ? 4000 : resolutionRef.current === '720p' ? 2000 : 1000,
      });

      if (!isMounted.current) {
        await whipClient.current.endSession(session, bearerToken);
        return;
      }

      setActiveSession(session);
      setActiveTarget(target);
      setStatus('streaming');
      setStreamDuration(0);
      reconnectDelay.current = RECONNECT_BASE_MS;

      // Wire up disconnect detection via ICE state
      session.peerConnection.oniceconnectionstatechange = () => {
        const state = (session.peerConnection as any).iceConnectionState;
        if (state === 'disconnected' || state === 'failed') {
          _handleDisconnect(target);
        }
      };

      // Start stats polling
      _startStatsPolling(session);
      // Start duration timer
      durationTimer.current = setInterval(() => {
        if (isMounted.current) {
          setStreamDuration(prev => prev + 1);
        }
      }, 1000);

    } catch (err: any) {
      if (!isMounted.current) { return; }
      const msg = err instanceof WHIPError
        ? `Server error ${err.status}: ${err.detail}`
        : (err?.message ?? String(err));
      _setError(`Failed to start streaming: ${msg}`);
    }
  }, [status]);

  const stopStreaming = useCallback(async (): Promise<void> => {
    _clearTimers();

    const session = activeSessionRef.current;
    const target = activeTargetRef.current;
    const bearerToken = target?.type !== 'whip_external' ? (tokenRef.current ?? undefined) : undefined;

    if (session) {
      try {
        await whipClient.current.endSession(session, bearerToken);
      } catch { /* best effort */ }
    }

    if (isMounted.current) {
      setActiveSession(null);
      setActiveTarget(null);
      setStats(null);
      setStreamDuration(0);
      setStatus(localStreamRef.current ? 'previewing' : 'idle');
    }
  }, []);

  const toggleCamera = useCallback((): void => {
    const next: 'user' | 'environment' = facingModeRef.current === 'environment' ? 'user' : 'environment';
    setFacingMode(next);
    // Restart preview with new facing mode if currently previewing
    if (status === 'previewing' || status === 'streaming') {
      void startPreview();
    }
  }, [status, startPreview]);

  const setResolutionCb = useCallback((r: Resolution): void => {
    setResolution(r);
    // Restart preview with new resolution if currently active
    if (status === 'previewing') {
      void startPreview();
    }
  }, [status, startPreview]);

  const refreshStats = useCallback(async (): Promise<void> => {
    const session = activeSessionRef.current;
    if (!session) { return; }
    const newStats = await _pollStats(session);
    if (isMounted.current && newStats) {
      setStats(newStats);
    }
  }, []);

  // ── Internal helpers ─────────────────────────────────────────────────────────

  function _startStatsPolling(session: WHIPSession) {
    if (statsTimer.current) { clearInterval(statsTimer.current); }
    statsTimer.current = setInterval(async () => {
      if (!isMounted.current) { return; }
      const newStats = await _pollStats(session);
      if (newStats && isMounted.current) {
        setStats(newStats);
      }
    }, STATS_POLL_INTERVAL_MS);
  }

  function _handleDisconnect(target: StreamTarget) {
    if (!isMounted.current) { return; }
    const delay = reconnectDelay.current;
    reconnectDelay.current = Math.min(delay * 2, RECONNECT_MAX_MS);

    setStatus('connecting');
    setStats(null);
    _clearTimers();

    reconnectTimer.current = setTimeout(() => {
      if (!isMounted.current) { return; }
      void startStreaming(target);
    }, delay);
  }

  return {
    // State
    status,
    localStream,
    activeSession,
    activeTarget,
    errorMessage,
    stats,
    facingMode,
    resolution,
    streamDuration,
    // Actions
    requestPermission,
    startPreview,
    stopPreview,
    startStreaming,
    stopStreaming,
    toggleCamera,
    setResolution: setResolutionCb,
    refreshStats,
  };
}

// ── Stats extraction ──────────────────────────────────────────────────────────

let _lastBytesOut = 0;
let _lastStatsTime = 0;

async function _pollStats(session: WHIPSession): Promise<WebcamStats | null> {
  try {
    const report: RTCStatsReport = await session.peerConnection.getStats();
    let bytesOut = 0;
    let packetsLost = 0;
    let fps = 0;

    report.forEach((stat: any) => {
      if (stat.type === 'outbound-rtp') {
        bytesOut += stat.bytesSent ?? 0;
        packetsLost += stat.packetsLost ?? 0;
        if (stat.framesPerSecond != null) {
          fps = Math.round(stat.framesPerSecond);
        }
      }
    });

    const now = Date.now();
    const elapsed = (now - _lastStatsTime) / 1000;
    const bytesDelta = bytesOut - _lastBytesOut;
    const bitrate = elapsed > 0 ? Math.round((bytesDelta * 8) / elapsed / 1000) : 0;

    _lastBytesOut = bytesOut;
    _lastStatsTime = now;

    return {bytesOut, packetsLost, fps, bitrate};
  } catch {
    return null;
  }
}
