export interface Node {
  id: string
  name: string
  hostname: string
  ip_address: string | null
  status: 'online' | 'offline' | 'connecting' | 'error'
  active: boolean
  machine_class: 'workstation' | 'server' | 'kiosk'
  last_seen: string
  version: string
  metadata: Record<string, unknown>
}

export interface NodesResponse {
  nodes: Node[]
  total: number
}

export interface WebSocketEvent {
  type: 'nodes_update' | 'node_status_change'
  payload: {
    nodes: Node[]
  }
}
