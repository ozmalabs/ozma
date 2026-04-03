/**
 * Ozma API client.
 *
 * All requests are routed through the Connect relay URL configured in settings.
 * Auth tokens are managed by AuthContext; this module reads them from MMKV and
 * will trigger a refresh on 401.
 */

import {MMKV} from 'react-native-mmkv';
import {
  CameraListResponse,
  Camera,
  NodeListResponse,
  NodeInfo,
  WoLResponse,
  NotificationListResponse,
  NotificationRecord,
  GuestInvite,
  GuestInviteRequest,
  GuestInviteResponse,
  GuestListResponse,
  PushRegisterRequest,
  PushRegisterResponse,
  ControllerInfo,
  OzmaApiError,
  OIDCTokens,
} from './types';

// Storage key constants — keep in sync with AuthContext.
const STORAGE_KEY_TOKENS = 'ozma.auth.tokens';
const STORAGE_KEY_CONTROLLER_URL = 'ozma.controller_url';

const storage = new MMKV({id: 'ozma-api'});

// Singleton refresh lock to avoid parallel refresh races.
let refreshPromise: Promise<OIDCTokens | null> | null = null;

// ── Helpers ───────────────────────────────────────────────────────────────────

function getControllerUrl(): string {
  const url = storage.getString(STORAGE_KEY_CONTROLLER_URL);
  if (!url) {
    throw new Error('Controller URL not configured. Open Settings to set it.');
  }
  // Strip trailing slash
  return url.replace(/\/$/, '');
}

function getTokens(): OIDCTokens | null {
  const raw = storage.getString(STORAGE_KEY_TOKENS);
  if (!raw) {
    return null;
  }
  try {
    return JSON.parse(raw) as OIDCTokens;
  } catch {
    return null;
  }
}

function saveTokens(tokens: OIDCTokens): void {
  storage.set(STORAGE_KEY_TOKENS, JSON.stringify(tokens));
}

async function refreshTokens(
  baseUrl: string,
  tokens: OIDCTokens,
): Promise<OIDCTokens | null> {
  if (!tokens.refresh_token) {
    return null;
  }
  try {
    // Use the controller's OIDC token endpoint via the discovery doc.
    const discoveryRes = await fetch(
      `${baseUrl}/.well-known/openid-configuration`,
    );
    if (!discoveryRes.ok) {
      return null;
    }
    const discovery = (await discoveryRes.json()) as {token_endpoint: string};
    const body = new URLSearchParams({
      grant_type: 'refresh_token',
      refresh_token: tokens.refresh_token,
    });
    const res = await fetch(discovery.token_endpoint, {
      method: 'POST',
      headers: {'Content-Type': 'application/x-www-form-urlencoded'},
      body: body.toString(),
    });
    if (!res.ok) {
      return null;
    }
    const data = (await res.json()) as {
      access_token: string;
      refresh_token?: string;
      id_token?: string;
      expires_in: number;
      scope?: string;
    };
    const refreshed: OIDCTokens = {
      access_token: data.access_token,
      refresh_token: data.refresh_token ?? tokens.refresh_token,
      id_token: data.id_token ?? tokens.id_token,
      expires_at: Math.floor(Date.now() / 1000) + (data.expires_in ?? 3600),
      scopes: data.scope ? data.scope.split(' ') : tokens.scopes,
    };
    saveTokens(refreshed);
    return refreshed;
  } catch {
    return null;
  }
}

// ── Core fetch wrapper ────────────────────────────────────────────────────────

async function apiFetch<T>(
  path: string,
  options: RequestInit = {},
  retried = false,
): Promise<T> {
  const baseUrl = getControllerUrl();
  let tokens = getTokens();

  // Proactively refresh if token expires within 60 seconds.
  if (tokens && tokens.expires_at - Math.floor(Date.now() / 1000) < 60) {
    if (!refreshPromise) {
      refreshPromise = refreshTokens(baseUrl, tokens).finally(() => {
        refreshPromise = null;
      });
    }
    const refreshed = await refreshPromise;
    if (refreshed) {
      tokens = refreshed;
    }
  }

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    Accept: 'application/json',
    ...(options.headers as Record<string, string> | undefined),
  };

  if (tokens?.access_token) {
    headers['Authorization'] = `Bearer ${tokens.access_token}`;
  }

  const url = `${baseUrl}${path}`;
  const res = await fetch(url, {...options, headers});

  // On 401 attempt one token refresh.
  if (res.status === 401 && !retried && tokens?.refresh_token) {
    if (!refreshPromise) {
      refreshPromise = refreshTokens(baseUrl, tokens).finally(() => {
        refreshPromise = null;
      });
    }
    const refreshed = await refreshPromise;
    if (refreshed) {
      return apiFetch<T>(path, options, true);
    }
  }

  if (!res.ok) {
    let detail = `HTTP ${res.status}`;
    try {
      const err = (await res.json()) as {detail?: string};
      detail = err.detail ?? detail;
    } catch {
      // ignore parse error
    }
    throw new OzmaApiError(res.status, detail);
  }

  // 204 No Content
  if (res.status === 204) {
    return {} as T;
  }

  return res.json() as Promise<T>;
}

// ── Public API surface ────────────────────────────────────────────────────────

export const ozmaClient = {
  // -- Configuration helpers --

  setControllerUrl(url: string): void {
    storage.set(STORAGE_KEY_CONTROLLER_URL, url.replace(/\/$/, ''));
  },

  getControllerUrl(): string | undefined {
    return storage.getString(STORAGE_KEY_CONTROLLER_URL);
  },

  setTokens(tokens: OIDCTokens): void {
    saveTokens(tokens);
  },

  clearTokens(): void {
    storage.delete(STORAGE_KEY_TOKENS);
  },

  // -- Controller info --

  async getInfo(): Promise<ControllerInfo> {
    return apiFetch<ControllerInfo>('/api/v1/info');
  },

  // -- Cameras --

  async listCameras(): Promise<CameraListResponse> {
    return apiFetch<CameraListResponse>('/api/v1/cameras');
  },

  async getCamera(cameraId: string): Promise<Camera> {
    return apiFetch<Camera>(`/api/v1/cameras/${encodeURIComponent(cameraId)}`);
  },

  /**
   * Returns an absolute HLS stream URL for use in VideoPlayer.
   * Falls back to MJPEG if HLS is not available.
   * Returns null if the camera is not currently capturing.
   */
  buildStreamUrl(camera: Camera): string | null {
    const path = camera.stream_path ?? camera.mjpeg_path;
    if (!path) {
      return null;
    }
    const base = getControllerUrl();
    if (path.startsWith('http')) {
      return path;
    }
    return `${base}${path}`;
  },

  /** Returns an absolute snapshot URL. */
  buildSnapshotUrl(camera: Camera): string {
    const base = getControllerUrl();
    if (camera.snapshot_path.startsWith('http')) {
      return camera.snapshot_path;
    }
    return `${base}${camera.snapshot_path}`;
  },

  /** Direct snapshot endpoint (returns JPEG bytes URL). */
  getSnapshotUrl(cameraId: string): string {
    const base = getControllerUrl();
    return `${base}/api/v1/cameras/${encodeURIComponent(cameraId)}/snapshot`;
  },

  // -- Nodes / Machines --

  async listNodes(): Promise<NodeListResponse> {
    return apiFetch<NodeListResponse>('/api/v1/nodes');
  },

  async getNode(nodeId: string): Promise<NodeInfo> {
    return apiFetch<NodeInfo>(`/api/v1/nodes/${encodeURIComponent(nodeId)}`);
  },

  async sendWoL(nodeId: string): Promise<WoLResponse> {
    return apiFetch<WoLResponse>(
      `/api/v1/nodes/${encodeURIComponent(nodeId)}/wol`,
      {method: 'POST'},
    );
  },

  // -- Notifications --

  async listNotifications(params?: {
    limit?: number;
    offset?: number;
    unread_only?: boolean;
  }): Promise<NotificationListResponse> {
    const qs = new URLSearchParams({format: 'mobile'});
    if (params?.limit !== undefined) {
      qs.set('limit', String(params.limit));
    }
    if (params?.offset !== undefined) {
      qs.set('offset', String(params.offset));
    }
    if (params?.unread_only) {
      qs.set('unread_only', 'true');
    }
    return apiFetch<NotificationListResponse>(`/api/v1/notifications?${qs.toString()}`);
  },

  async markNotificationRead(notificationId: string): Promise<void> {
    await apiFetch<Record<string, never>>(
      `/api/v1/notifications/${encodeURIComponent(notificationId)}/read`,
      {method: 'POST'},
    );
  },

  // -- Push registration --

  async registerPushToken(req: PushRegisterRequest): Promise<PushRegisterResponse> {
    return apiFetch<PushRegisterResponse>('/api/v1/push/register', {
      method: 'POST',
      body: JSON.stringify(req),
    });
  },

  async unregisterPushToken(deviceToken: string): Promise<void> {
    await apiFetch<Record<string, never>>('/api/v1/push/unregister', {
      method: 'DELETE',
      body: JSON.stringify({device_token: deviceToken}),
    });
  },

  async sendTestPush(): Promise<{ok: boolean; message: string}> {
    return apiFetch<{ok: boolean; message: string}>('/api/v1/push/test', {
      method: 'POST',
    });
  },

  // -- Guest invites --

  async listGuests(): Promise<GuestListResponse> {
    return apiFetch<GuestListResponse>('/api/v1/guests');
  },

  async createGuestInvite(req: GuestInviteRequest): Promise<GuestInviteResponse> {
    return apiFetch<GuestInviteResponse>('/api/v1/guests/invite', {
      method: 'POST',
      body: JSON.stringify(req),
    });
  },

  async revokeGuestInvite(inviteId: string): Promise<void> {
    await apiFetch<Record<string, never>>(
      `/api/v1/guests/invite/${encodeURIComponent(inviteId)}`,
      {method: 'DELETE'},
    );
  },

  async getGuestInvite(inviteId: string): Promise<GuestInvite> {
    return apiFetch<GuestInvite>(
      `/api/v1/guests/invite/${encodeURIComponent(inviteId)}`,
    );
  },

  // -- Connectivity check --

  async ping(): Promise<boolean> {
    try {
      const res = await fetch(`${getControllerUrl()}/api/v1/info`, {
        signal: AbortSignal.timeout(5000),
      });
      return res.ok;
    } catch {
      return false;
    }
  },
};
