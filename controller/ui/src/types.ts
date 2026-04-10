export interface NodeInfo {
  id: string
  name: string
  hostname: string
  ip_address: string
  active: boolean
  machine_class: 'workstation' | 'server' | 'kiosk'
  last_seen: string
  status: 'online' | 'offline' | 'connecting'
  metadata?: Record<string, unknown>
}

export interface ApiError {
  error: string
  message?: string
}

export interface AuthToken {
  token: string
  expires_at: number
}
