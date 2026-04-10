export interface NodeInfo {
  id: string
  name: string
  hostname: string
  ip_address?: string
  status: 'online' | 'offline' | 'connecting' | 'error'
  machine_class: 'workstation' | 'server' | 'kiosk'
  last_seen: string
  capabilities: {
    hid: boolean
    video: boolean
    audio: boolean
  }
  metadata?: Record<string, unknown>
}

export interface NodesResponse {
  nodes: NodeInfo[]
  total: number
}

export interface NodeResponse {
  node: NodeInfo
}
