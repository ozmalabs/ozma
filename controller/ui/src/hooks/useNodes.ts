import { create } from 'zustand'
import { Node, NodesState } from '../types/node'

const API_BASE = '/api/v1'

const getWsUrl = () => {
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  const baseUrl = import.meta.env.VITE_API_URL || 'http://localhost:7380'
  return `${protocol}//${baseUrl.replace('http://', '').replace('https://', '')}/ws/notifications`
}

export const useNodesStore = create<NodesState>((set, get) => {
  let ws: WebSocket | null = null
  let retryTimeout: number | null = null

  const setWs = (newWs: WebSocket | null) => {
    ws = newWs
  }

  return {
    nodes: [],
    loading: false,
    error: null,
    ws: null,

    refreshNodes: async () => {
      set({ loading: true, error: null })
      try {
        const response = await fetch(`${API_BASE}/nodes`)
        if (!response.ok) {
          throw new Error(`HTTP ${response.status}`)
        }
        const data: Node[] = await response.json()
        set({ nodes: data, loading: false, error: null, ws })
      } catch (err) {
        set({ loading: false, error: err instanceof Error ? err.message : 'Failed to fetch nodes', ws })
      }
    },

    addNode: (node) => {
      set((state) => {
        const existing = state.nodes.find((n) => n.id === node.id)
        if (existing) {
          return state
        }
        return { nodes: [...state.nodes, node], ws }
      })
    },

    updateNode: (node) => {
      set((state) => ({
        nodes: state.nodes.map((n) => (n.id === node.id ? node : n)),
        ws,
      }))
    },

    removeNode: (id) => {
      set((state) => ({
        nodes: state.nodes.filter((n) => n.id !== id),
        ws,
      }))
    },

    connectWebSocket: async () => {
      if (ws && ws.readyState === WebSocket.OPEN) {
        return
      }

      const token = localStorage.getItem('ozma_auth_token')
      const wsUrl = getWsUrl() + (token ? `?token=${token}` : '')

      ws = new WebSocket(wsUrl)

      ws.onopen = () => {
        console.log('WebSocket connected')
        if (retryTimeout) {
          clearTimeout(retryTimeout)
          retryTimeout = null
        }
        get().refreshNodes()
      }

      ws.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data)
          if (data.type === 'node_state') {
            const node = data.payload as Node
            get().updateNode(node)
          }
        } catch (err) {
          console.error('Failed to parse WebSocket message:', err)
        }
      }

      ws.onerror = (error) => {
        console.error('WebSocket error:', error)
      }

      ws.onclose = () => {
        console.log('WebSocket disconnected, retrying...')
        // Schedule reconnection with backoff
        retryTimeout = window.setTimeout(() => {
          get().connectWebSocket()
        }, 3000)
      }

      setWs(ws)
      set({ ws })
    },

    disconnectWebSocket: () => {
      if (ws) {
        ws.close()
        ws = null
      }
      set({ ws: null })
    },
  }
})

export const useNodes = () => {
  const { nodes, loading, error, refreshNodes, addNode, updateNode, removeNode, connectWebSocket, disconnectWebSocket } =
    useNodesStore()

  return { nodes, loading, error, refreshNodes, addNode, updateNode, removeNode, connectWebSocket, disconnectWebSocket }
}
