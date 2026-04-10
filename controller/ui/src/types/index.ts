export interface Node {
  id: string
  name: string
  hostname: string
  status: 'online' | 'offline' | 'connecting' | 'error'
  machine_class: 'workstation' | 'server' | 'kiosk'
  last_seen: string
  active: boolean
  ip_address?: string
  mac_address?: string
  metadata?: Record<string, unknown>
}

export interface NodeListResponse {
  nodes: Node[]
  total: number
}

export interface AuthState {
  token: string | null
  setToken: (token: string | null) => void
  isAuthenticated: boolean
  login: (token: string) => void
  logout: () => void
}

export interface NodeState {
  nodes: Node[]
  loading: boolean
  error: string | null
  fetchNodes: () => Promise<void>
  addNode: (node: Node) => void
  updateNode: (node: Node) => void
  removeNode: (id: string) => void
}
