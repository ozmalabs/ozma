import { create } from 'zustand'
import { useEffect, useCallback } from 'react'

export interface NodeInfo {
  id: string
  name: string
  hostname: string
  machine_class: 'workstation' | 'server' | 'kiosk' | 'camera'
  status: 'online' | 'offline' | 'connecting'
  active: boolean
  last_seen: string
  ip_address: string
  mac_address: string | null
  machine_id: string | null
  uptime_seconds?: number
  hid_stats?: {
    total_keys: number
    total_clicks: number
    total_scrolls: number
    last_activity: number | null
  }
  scenario?: {
    id: string
    name: string
    color: string
  }
  role?: string
  hw?: string
  fw_version?: string
  proto_version?: number
  capabilities?: string[]
  stream_port?: number
  vnc_host?: string
  vnc_port?: number
}

interface NodesStore {
  nodes: NodeInfo[]
  loading: boolean
  error: string | null
  wsConnected: boolean
  activeNodeId: string | null
  fetchNodes: () => Promise<void>
  connectWebSocket: (onNodeUpdate?: (node: NodeInfo) => void) => () => void
  addNode: (node: NodeInfo) => void
  updateNode: (node: NodeInfo) => void
  removeNode: (id: string) => void
  setActiveNode: (nodeId: string | null) => void
}

const API_BASE = '/api/v1'

export const useNodesStore = create<NodesStore>((set, get) => ({
  nodes: [],
  loading: true,
  error: null,
  wsConnected: false,
  activeNodeId: null,

  fetchNodes: async () => {
    try {
      set({ loading: true, error: null })
      const response = await fetch(`${API_BASE}/nodes`)
      if (!response.ok) {
        throw new Error(`Failed to fetch nodes: ${response.statusText}`)
      }
      const data = await response.json()
      
      // Transform API response to NodeInfo format
      const nodes = Array.isArray(data) ? data : (data.nodes || [])
      const transformedNodes = nodes.map((node: any) => ({
        id: node.id || node.node_id || '',
        name: node.name || 'Unknown',
        hostname: node.host || node.hostname || '',
        machine_class: (node.machine_class || node.machineClass || 'workstation') as any,
        status: node.active ? 'online' : 'offline',
        active: !!node.active,
        last_seen: node.last_seen ? new Date(node.last_seen * 1000).toISOString() : new Date().toISOString(),
        ip_address: node.ip_address || node.ipAddress || node.host || '',
        mac_address: node.mac_address || node.macAddress || null,
        machine_id: node.machine_id || node.machineId || null,
        uptime_seconds: node.uptime_seconds || node.uptimeSeconds || undefined,
        hid_stats: node.hid_stats || node.hidStats || undefined,
        scenario: node.scenario || undefined,
        role: node.role || undefined,
        hw: node.hw || undefined,
        fw_version: node.fw_version || node.fwVersion || undefined,
        proto_version: node.proto_version || node.protoVersion || undefined,
        capabilities: node.capabilities || undefined,
        stream_port: node.stream_port || node.streamPort || undefined,
        vnc_host: node.vnc_host || node.vncHost || undefined,
        vnc_port: node.vnc_port || node.vncPort || undefined,
      }))
      
      set({ 
        nodes: transformedNodes, 
        loading: false, 
        error: null 
      })
    } catch (error) {
      console.error('Error fetching nodes:', error)
      set({ loading: false, error: 'Failed to load nodes' })
    }
  },

  connectWebSocket: (onNodeUpdate) => {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const wsUrl = `${protocol}//${window.location.hostname}:7380/api/v1/ws`
    
    // Fallback to localhost for development
    let ws: WebSocket | null = null
    
    const connect = () => {
      try {
        const token = localStorage.getItem('ozma_token') || ''
        ws = new WebSocket(`${wsUrl}?token=${token}`)

        ws.onopen = () => {
          console.log('WebSocket connected')
          set({ wsConnected: true })
        }

        ws.onmessage = (event) => {
          try {
            const data = JSON.parse(event.data)
            handleWebSocketMessage(data, onNodeUpdate)
          } catch (error) {
            console.error('Error parsing WebSocket message:', error)
          }
        }

        ws.onclose = () => {
          console.log('WebSocket disconnected')
          set({ wsConnected: false })
          // Reconnect after delay
          setTimeout(connect, 3000)
        }

        ws.onerror = (error) => {
          console.error('WebSocket error:', error)
        }
      } catch (error) {
        console.error('WebSocket connection failed:', error)
        setTimeout(connect, 3000)
      }
    }

    connect()

    return () => {
      if (ws) {
        ws.close()
      }
    }
  },

  addNode: (node) => {
    set((state) => {
      const exists = state.nodes.find((n) => n.id === node.id)
      if (exists) return state
      return { nodes: [...state.nodes, node] }
    })
  },

  updateNode: (node) => {
    set((state) => {
      const index = state.nodes.findIndex((n) => n.id === node.id)
      if (index === -1) {
        // Node doesn't exist, add it
        return { nodes: [...state.nodes, node] }
      }
      const updatedNodes = [...state.nodes]
      updatedNodes[index] = node
      return { nodes: updatedNodes }
    })
  },

  removeNode: (id) => {
    set((state) => ({
      nodes: state.nodes.filter((n) => n.id !== id),
    }))
  },

  setActiveNode: (nodeId) => {
    set({ activeNodeId: nodeId })
  },
}))

function handleWebSocketMessage(data: unknown, onNodeUpdate?: (node: NodeInfo) => void) {
  if (!data || typeof data !== 'object') return

  const { type, node, node_id, active } = data as { 
    type?: string 
    node?: NodeInfo 
    node_id?: string
    active?: boolean
  }

  switch (type) {
    case 'node_added':
    case 'node.online':
      if (node) {
        const store = useNodesStore.getState()
        store.addNode(node)
        onNodeUpdate?.(node)
      }
      break
    case 'node_updated':
    case 'node.updated':
    case 'node_state_update':
      if (node) {
        const store = useNodesStore.getState()
        store.updateNode(node)
        onNodeUpdate?.(node)
      }
      break
    case 'node_removed':
    case 'node.offline':
      if (node_id) {
        const store = useNodesStore.getState()
        store.removeNode(node_id)
      }
      break
    case 'status_update':
    case 'node.status':
      if (node) {
        const store = useNodesStore.getState()
        store.updateNode(node)
        onNodeUpdate?.(node)
      }
      break
    case 'node.switched':
      if (node_id) {
        const store = useNodesStore.getState()
        store.setActiveNode(node_id)
        // Also update all nodes to reflect active state
        const activeNode = store.nodes.find(n => n.id === node_id)
        if (activeNode) {
          store.updateNode({ ...activeNode, active: true })
        }
      }
      break
  }
}

export function useNodes() {
  const { nodes, loading, error, wsConnected, activeNodeId, fetchNodes, connectWebSocket } = useNodesStore()

  useEffect(() => {
    fetchNodes()
    const disconnect = connectWebSocket()
    return disconnect
  }, [fetchNodes, connectWebSocket])

  return { nodes, loading, error, wsConnected, activeNodeId, refetch: fetchNodes }
}
