export interface Node {
  id: string
  name: string
  hostname: string
  status: 'online' | 'offline' | 'connecting' | 'error'
  machine_class: 'workstation' | 'server' | 'kiosk'
  active: boolean
  last_seen: string
  ip?: string
  ip_address?: string
  mac_address?: string
  tags?: string[]
  metadata?: Record<string, unknown>
  port?: number
  uptime?: number
  cpu_usage?: number
  memory_usage?: number
  video_enabled?: boolean
  audio_enabled?: boolean
  usb_enabled?: boolean
  hids?: string[]
  displays?: DisplayInfo[]
}

export interface DisplayInfo {
  id: string
  name: string
  width: number
  height: number
  refresh_rate: number
  primary: boolean
}

export interface NodesState {
  nodes: Node[]
  loading: boolean
  error: string | null
  refreshNodes: () => Promise<void>
  addNode: (node: Node) => void
  updateNode: (node: Node) => void
  removeNode: (id: string) => void
  connectWebSocket: () => Promise<void>
  disconnectWebSocket: () => void
  ws: WebSocket | null
}
