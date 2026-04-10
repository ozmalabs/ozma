export interface Node {
  id: string
  name: string
  status: 'connected' | 'disconnected' | 'error'
  address: string
  last_seen: string
  machine_class: 'workstation' | 'server' | 'kiosk'
  active: boolean
  metadata: {
    hostname: string
    os: string
    cpu: string
    memory: string
    disk: string
  }
}

export interface NodesState {
  nodes: Node[]
  loading: boolean
  error: string | null
  lastUpdate: number | null
}
