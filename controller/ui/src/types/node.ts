export interface NodeInfo {
  id: string
  name: string
  address: string
  port: number
  status: 'online' | 'offline' | 'connecting'
  machine_class: 'workstation' | 'server' | 'kiosk'
  last_seen: string
  active: boolean
  ip_address?: string
  mac_address?: string
  hostname?: string
  platform?: string
  version?: string
}

export interface NodeEvent {
  type: 'node_added' | 'node_updated' | 'node_removed' | 'status_changed'
  node: NodeInfo
}
