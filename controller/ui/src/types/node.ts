export interface Node {
  id: string
  name: string
  hostname: string
  status: 'online' | 'offline' | 'connecting' | 'error'
  machine_class: 'workstation' | 'server' | 'kiosk'
  active: boolean
  last_seen: string
  ip_address?: string
  mac_address?: string
  tags?: string[]
  metadata?: Record<string, unknown>
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
