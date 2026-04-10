export interface Node {
  id: string
  name: string
  description?: string
  machine_class: 'workstation' | 'server' | 'kiosk'
  status: 'online' | 'offline' | 'connecting' | 'error'
  active: boolean
  last_seen: string
  ip_address?: string
  hostname?: string
  version?: string
  machine_id?: string
  tags?: string[]
  metadata?: Record<string, unknown>
}

export interface NodesState {
  nodes: Node[]
  loading: boolean
  error: string | null
  lastUpdated: number | null
  fetchNodes: () => Promise<void>
  updateNode: (node: Node) => void
  removeNode: (id: string) => void
  setError: (error: string | null) => void
  subscribeToUpdates: (callback: (node: Node) => void) => () => void
}
