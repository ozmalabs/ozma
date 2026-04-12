// ---------------------------------------------------------------------------
// Shared API response types used by the REST client and stores
// ---------------------------------------------------------------------------

export interface AuthResponse {
  token: string
  expires_at: string
  user: {
    id: string
    username: string
    email: string
    roles: string[]
  }
}

export interface NodeInfo {
  id: string
  name: string
  host: string
  online: boolean
  machine_class: 'workstation' | 'server' | 'kiosk' | 'camera'
  last_seen: string | null
  tags: string[]
}

export interface NodesResponse {
  nodes: NodeInfo[]
}

export interface NodeUpdateEvent {
  type: 'node_added' | 'node_updated' | 'node_removed' | 'status_changed'
  node: NodeInfo
}
