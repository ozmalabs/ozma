export interface Node {
  id: string
  name: string
  hostname: string
  ip: string
  status: 'online' | 'offline' | 'connecting' | 'unknown'
  active: boolean
  machine_class: 'workstation' | 'server' | 'kiosk'
  last_seen: string
  metadata?: Record<string, string>
}

export interface NodesState {
  nodes: Node[]
  loading: boolean
  error: string | null
  refresh: () => void
}

export interface AuthState {
  token: string | null
  setToken: (token: string | null) => void
  isLoggedIn: boolean
  login: (token: string) => void
  logout: () => void
}
