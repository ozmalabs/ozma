import { create } from 'zustand'
import { Node, NodesState } from '../types'

const API_BASE = '/api/v1'

export const useNodesStore = create<NodesState>((set, get) => {
  let socket: WebSocket | null = null
  const callbacks = new Set<(node: Node) => void>()

  return {
    nodes: [],
    loading: false,
    error: null,
    lastUpdated: null,

    fetchNodes: async () => {
      set({ loading: true, error: null })
      try {
        const response = await fetch(`${API_BASE}/nodes`)
        if (!response.ok) {
          throw new Error(`Failed to fetch nodes: ${response.status}`)
        }
        const data: Node[] = await response.json()
        set({ nodes: data, loading: false, lastUpdated: Date.now() })
      } catch (error) {
        set({ loading: false, error: error instanceof Error ? error.message : 'Unknown error' })
      }
    },

    updateNode: (node) => {
      set((state) => ({
        nodes: state.nodes.map((n) => (n.id === node.id ? node : n)),
        lastUpdated: Date.now(),
      }))
      callbacks.forEach((cb) => cb(node))
    },

    removeNode: (id) => {
      set((state) => ({
        nodes: state.nodes.filter((n) => n.id !== id),
        lastUpdated: Date.now(),
      }))
    },

    setError: (error) => {
      set({ error })
    },

    subscribeToUpdates: (callback) => {
      callbacks.add(callback)
      return () => callbacks.delete(callback)
    },
  }
})

// WebSocket connection function - called from outside store
export function connectWebSocket() {
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const url = `${protocol}//${window.location.host}/ws`

  const socket = new WebSocket(url)

  socket.onopen = () => {
    console.log('WebSocket connected')
  }

  socket.onmessage = (event) => {
    try {
      const data = JSON.parse(event.data)
      if (data.type === 'node_update' || data.type === 'node_created' || data.type === 'node_deleted') {
        const store = useNodesStore.getState()
        store.updateNode(data.node)
      }
    } catch (error) {
      console.error('Failed to parse WebSocket message:', error)
    }
  }

  socket.onerror = (error) => {
    console.error('WebSocket error:', error)
  }

  socket.onclose = () => {
    console.log('WebSocket disconnected, reconnecting...')
    setTimeout(connectWebSocket, 3000)
  }

  return socket
}
