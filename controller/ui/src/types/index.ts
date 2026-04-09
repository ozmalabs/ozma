export interface Node {
  id: string
  name: string
  hostname: string
  ip: string
  mac: string
  status: 'online' | 'offline' | 'connecting' | 'error'
  active: boolean
  machine_class: 'workstation' | 'server' | 'kiosk'
  last_seen: string
  uptime: number
  cpu_usage: number
  memory_usage: number
  disk_usage: number
  temperature: number
  video_active: boolean
  audio_active: boolean
  usb_active: boolean
  session_id?: string
  tags: string[]
}

export interface NodeListResponse {
  nodes: Node[]
  total: number
  online: number
}

export interface WebSocketMessage {
  type: 'nodes_update' | 'status_update' | 'heartbeat'
  payload: Node | NodeListResponse
  timestamp: number
}
