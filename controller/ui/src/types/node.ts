export interface NodeInfo {
  id: string
  name: string
  hostname: string
  ip_address: string
  status: 'online' | 'offline' | 'connecting' | 'error'
  active: boolean
  machine_class: 'workstation' | 'server' | 'kiosk'
  last_seen: string
  uptime?: number
  cpu_usage?: number
  memory_usage?: number
  video_active?: boolean
  stream_url?: string
  capabilities: {
    usb_hid: boolean
    video_capture: boolean
    audio: boolean
    rgb_leds?: boolean
  }
  tags: string[]
}

export interface NodesState {
  nodes: NodeInfo[]
  loading: boolean
  error: string | null
  lastUpdate: number | null
  refreshNodes: () => void
  connectWebSocket: () => void
}
