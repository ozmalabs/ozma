/**
 * TypeScript types for the Ozma controller API.
 * All types map directly to JSON responses from the controller's FastAPI endpoints.
 */

// ── Auth ─────────────────────────────────────────────────────────────────────

export interface AuthTokenResponse {
  access_token: string;
  token_type: string;
  expires_in: number;
  scope: string;
}

export interface OIDCTokens {
  access_token: string;
  refresh_token: string | null;
  id_token: string | null;
  expires_at: number; // Unix timestamp in seconds
  scopes: string[];
}

// ── Cameras ──────────────────────────────────────────────────────────────────

export type PrivacyLevel = 'disabled' | 'local_only' | 'network' | 'public';

export interface PrivacyZone {
  x: number;
  y: number;
  width: number;
  height: number;
  mode: 'blur' | 'blackout';
}

export interface Camera {
  id: string;
  name: string;
  node_id: string | null;
  /** Camera source type: v4l2, rtsp, onvif, ndi, virtual */
  type: string;
  /**
   * Relative HLS stream path, e.g. /cameras/{id}/stream.m3u8
   * Null when not capturing. Resolve against the controller base URL.
   */
  stream_path: string | null;
  /** Relative MJPEG path. Null when not capturing. */
  mjpeg_path: string | null;
  /** Relative JPEG snapshot path, always present. */
  snapshot_path: string;
  privacy: {
    level: PrivacyLevel;
    acknowledged: boolean;
    zones: PrivacyZone[];
  };
  /** true when ffmpeg is actively capturing */
  active: boolean;
  /** Frigate camera name if backed by Frigate */
  frigate_name: string | null;
  /** Pixel dimensions */
  width: number;
  height: number;
  fps: number;
  tags: string[];
}

export interface CameraListResponse {
  cameras: Camera[];
  privacy_notice: string;
}

export interface CameraAccessLogEntry {
  camera_id: string;
  client: string;
  accessed_at: string;
  action: string;
}

// ── Machines / Nodes ─────────────────────────────────────────────────────────

export type MachineClass = 'workstation' | 'server' | 'kiosk' | 'camera';

export interface NodeInfo {
  id: string;
  name: string;
  host: string;
  port: number;
  machine_class: MachineClass;
  /** ISO 8601 timestamp of last heartbeat */
  last_seen: string | null;
  online: boolean;
  mac_address: string | null;
  camera_streams: CameraStream[];
  frigate_host: string | null;
  frigate_port: number | null;
  direct_registered: boolean;
  agent_connected: boolean;
  ip_address: string | null;
  platform: string | null;
  os_version: string | null;
}

export interface CameraStream {
  url: string;
  name: string;
  type: 'hls' | 'mjpeg' | 'rtsp';
}

export interface NodeListResponse {
  nodes: NodeInfo[];
  active_node_id: string | null;
}

export interface WoLResponse {
  ok: boolean;
  message: string;
  mac: string | null;
}

// ── Notifications ─────────────────────────────────────────────────────────────

export type NotificationPlatform = 'ios' | 'android';

export interface PushRegisterRequest {
  device_token: string;
  platform: NotificationPlatform;
  /** Optional label shown in device list */
  device_name?: string;
}

export interface PushRegistration {
  id: string;
  device_token: string;
  platform: NotificationPlatform;
  device_name: string | null;
  registered_at: string;
  last_used: string | null;
}

export interface PushRegisterResponse {
  ok: boolean;
  registration_id: string;
}

export interface NotificationRecord {
  id: string;
  title: string;
  body: string;
  /** ISO 8601 */
  created_at: string;
  /** Optional JPEG snapshot attached to the notification */
  snapshot_url: string | null;
  camera_id: string | null;
  node_id: string | null;
  event_type: string;
  read: boolean;
}

export interface NotificationListResponse {
  notifications: NotificationRecord[];
  unread_count: number;
}

// ── Guest invites ─────────────────────────────────────────────────────────────

export interface GuestInvite {
  id: string;
  invite_url: string;
  /** ISO 8601 */
  expires_at: string;
  /** Camera IDs this guest can view. Empty = all cameras. */
  camera_ids: string[];
  /** Display name for the invite */
  label: string | null;
  created_by: string;
  created_at: string;
  /** null = not yet accepted */
  accepted_at: string | null;
  /** null = not yet accepted */
  accepted_by_email: string | null;
  revoked: boolean;
}

export interface GuestInviteRequest {
  label?: string;
  camera_ids?: string[];
  /** Duration in seconds. Defaults to 7 days. */
  ttl?: number;
}

export interface GuestInviteResponse {
  ok: boolean;
  invite: GuestInvite;
}

export interface GuestListResponse {
  invites: GuestInvite[];
}

// ── Controller info ───────────────────────────────────────────────────────────

export interface ControllerInfo {
  name: string;
  version: string;
  edition: string;
  node_count: number;
  active_node_id: string | null;
}

// ── API error ─────────────────────────────────────────────────────────────────

export interface ApiError {
  detail: string;
  status: number;
}

export class OzmaApiError extends Error {
  constructor(
    public readonly status: number,
    public readonly detail: string,
  ) {
    super(`API error ${status}: ${detail}`);
    this.name = 'OzmaApiError';
  }
}
