// Machine class - what kind of machine is this node plugged into
export type MachineClass = 'workstation' | 'server' | 'kiosk' | 'camera'

// Node status
export type NodeStatus = 'online' | 'offline' | 'connecting' | 'unknown'

// Display output information
export interface DisplayOutput {
  index: number
  source_type: 'dbus' | 'vnc' | 'ivshmem' | 'agent' | string
  capture_source_id: string
  width: number
  height: number
}

// Camera stream information
export interface CameraStream {
  name: string
  rtsp_inbound: string
  backchannel: string
  hls: string
}

// HID statistics
export interface HIDStats {
  total_keys: number
  total_clicks: number
  total_scrolls: number
  last_activity: string
}

// Scenario information
export interface Scenario {
  id: string
  name: string
  color: string
}

export interface NodeInfo {
  // Core identification
  id: string                    // mDNS instance name, used as stable identifier
  name?: string                 // Human-readable name
  hostname: string
  host: string                  // IP address
  port: number                  // UDP port (always 7331 per spec)
  
  // Hardware info
  role: string                  // "compute", "presence", "room-mic", "display", etc.
  hw: string                    // hardware type, e.g. "milkv-duos", "rpi-zero2w"
  fw_version: string            // firmware version
  proto_version: number         // protocol version
  
  // Machine classification
  machine_class: MachineClass   // workstation | server | kiosk | camera
  last_seen: string
  
  // Display outputs (for multi-display setups)
  display_outputs: DisplayOutput[]
  
  // VNC configuration
  vnc_host?: string
  vnc_port?: number
  
  // Streaming configuration
  stream_port?: number          // HTTP port on the node for HLS
  stream_path?: string          // path, e.g. /stream/stream.m3u8
  
  // HTTP API port
  api_port?: number
  
  // Audio routing
  audio_type?: string           // "pipewire" | "vban" | null
  audio_sink?: string           // PW null-sink name (pipewire nodes)
  audio_vban_port?: number      // UDP port node emits VBAN on
  mic_vban_port?: number        // UDP port node listens for mic VBAN
  
  // Virtual capture device (soft nodes)
  capture_device?: string       // /dev/videoN path on controller host
  
  // Camera node fields
  camera_streams?: CameraStream[]
  frigate_host?: string         // hostname/IP of Frigate API
  frigate_port?: number         // Frigate API port (default 5000)
  
  // Ownership and sharing
  owner_user_id?: string        // User who owns this node
  owner_id?: string             // User ID who owns this node/seat
  shared_with?: string[]        // User IDs who have access
  share_permissions?: Record<string, string>  // user_id -> "use"|"manage"|"admin"
  parent_node_id?: string       // If this is a seat, the machine it belongs to
  
  // Game streaming (Sunshine/Moonlight)
  sunshine_port?: number        // Sunshine stream base port
  
  // Seat configuration
  seat_count?: number
  seat_config?: Record<string, any>
  
  // Active status
  active?: boolean
  
  // Additional computed fields
  status?: NodeStatus
  uptime_seconds?: number
  ip_address?: string
  mac_address?: string
  platform?: string
  version?: string
}

export interface PowerAction {
  type: 'power_state' | 'power_action'
  action?: 'on' | 'off' | 'reboot' | 'hard_reset' | 'hard_off'
  status?: 'success' | 'error'
  message?: string
}

export interface HealthStatus {
  cpu_usage: number
  memory_usage: number
  disk_usage: number
  temperature: number
  uptime: number
}

export interface NodeEvent {
  type: 'node_added' | 'node_updated' | 'node_removed' | 'status_changed'
  node: NodeInfo
}
