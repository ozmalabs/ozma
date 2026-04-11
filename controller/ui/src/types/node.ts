export interface NodeInfo {
  id: string
  name: string
  address?: string
  port?: number
  status: 'online' | 'offline' | 'connecting' | 'unknown'
  machine_class?: 'workstation' | 'server' | 'kiosk' | 'camera'
  last_seen?: string
  active?: boolean
  ip_address?: string
  mac_address?: string
  hostname?: string
  platform?: string
  version?: string
  host?: string
  role?: string
  hw?: string
  fw_version?: string
  proto_version?: string
  capabilities?: string[]
  port?: number
  stream_port?: number
  stream_path?: string
  video_enabled?: boolean
  vnc_host?: string
  vnc_port?: number
  audio_type?: string
  audio_sink?: string
  audio_vban_port?: number
  mic_vban_port?: number
  capture_device?: string
  camera_streams?: CameraStream[]
  frigate_host?: string
  frigate_port?: number
  owner_user_id?: string
  owner_id?: string
  shared_with?: string[]
  share_permissions?: string
  parent_node_id?: string
  sunshine_port?: number
  seat_count?: number
  seat_config?: string
  display_outputs?: DisplayOutput[]
  scenario?: Scenario
  hid_stats?: HidStats
}

export interface CameraStream {
  name: string
  rtsp_inbound: string
  backchannel: string
  hls: string
}

export interface DisplayOutput {
  index: number
  source_type: string
  capture_source_id: string
  width: number
  height: number
}

export interface Scenario {
  id: string
  name: string
  color: string
}

export interface HidStats {
  total_keys: number
  total_clicks: number
  total_scrolls: number
  last_activity: string
}

export interface NodeEvent {
  type: 'node_added' | 'node_updated' | 'node_removed' | 'status_changed'
  node: NodeInfo
}
