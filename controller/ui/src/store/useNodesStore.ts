import { create } from 'zustand'
import { useEffect } from 'react'

export interface NodeInfo {
  id: string
  name: string
  hostname: string
  machine_class: 'workstation' | 'server' | 'kiosk'
  status: 'online' | 'offline' | 'connecting'
  active: boolean
  last_seen: string
  ip_address: string
  mac_address: string | null
  machine_id: string | null
}

interface NodesStore {
  nodes: NodeInfo[]
  loading: boolean
  error: string | null
  wsConnected: boolean
  fetchNodes: () => Promise<void>
  connectWebSocket: () => () => void
  addNode: (node: NodeInfo) => void
  updateNode: (node: NodeInfo) => void
  removeNode: (id: string) => void
}

const API_BASE = '/api/v1'

export const useNodesStore = create<NodesStore>((set) => ({
  nodes: [],
  loading: true,
  error: null,
  wsConnected: false,

  fetchNodes: async () => {
    try {
      set({ loading: true, error: null })
      const response = await fetch(`${API_BASE}/nodes`)
      if (!response.ok) {
        throw new Error(`Failed to fetch nodes: ${response.statusText}`)
      }
      const data = await response.json()
      set({ nodes: data, loading: false, error: null })
    } catch (error) {
      console.error('Error fetching nodes:', error)
      set({ loading: false, error: 'Failed to load nodes' })
    }
  },

  connectWebSocket: () => {
    const socket = new WebSocket(`ws://localhost:7380/ws?token=${localStorage.getItem('ozma_token') || ''}`)

    socket.onopen = () => {
      console.log('WebSocket connected')
      set({ wsConnected: true })
    }

    socket.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data)
        handleWebSocketMessage(data)
      } catch (error) {
        console.error('Error parsing WebSocket message:', error)
      }
    }

    socket.onclose = () => {
      console.log('WebSocket disconnected')
      set({ wsConnected: false })
    }

    socket.onerror = (error) => {
      console.error('WebSocket error:', error)
    }

    return () => {
      socket.close()
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
    set((state) => ({
      nodes: state.nodes.map((n) => (n.id === node.id ? node : n)),
    }))
  },

  removeNode: (id) => {
    set((state) => ({
      nodes: state.nodes.filter((n) => n.id !== id),
    }))
  },
}))

function handleWebSocketMessage(data: unknown) {
  const store = useNodesStore.getState()
  if (!data || typeof data !== 'object') return

  const { type, node } = data as { type?: string; node?: NodeInfo }

  switch (type) {
    case 'node_added':
      if (node) store.addNode(node)
      break
    case 'node_updated':
      if (node) store.updateNode(node)
      break
    case 'node_removed':
      if (node?.id) store.removeNode(node.id)
      break
    case 'status_update':
      if (node) store.updateNode(node)
      break
  }
}

export function useNodes() {
  const { nodes, loading, error, fetchNodes, connectWebSocket } = useNodesStore()

  useEffect(() => {
    fetchNodes()
    const disconnect = connectWebSocket()
    return disconnect
  }, [fetchNodes, connectWebSocket])

  return { nodes, loading, error }
}
