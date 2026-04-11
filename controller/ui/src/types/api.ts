import { NodeInfo } from './node'

export interface ApiResponse<T> {
  data?: T
  error?: string
  message?: string
}

export interface AuthResponse {
  token: string
  expires_at: string
}

export interface NodesResponse {
  nodes: NodeInfo[]
  total: number
}

export interface NodeUpdateEvent {
  type: 'node_added' | 'node_updated' | 'node_removed' | 'status_changed'
  node: NodeInfo
}
