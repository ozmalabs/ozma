import { create } from 'zustand'
import { Node, NodeListResponse, WebSocketMessage } from '../types'

interface NodeStore {
  nodes: Node[]
  loading: boolean
  error: string | null
  lastUpdate: number | null
  webSocket: WebSocket | null
  fetchNodes: () => Promise<void>
  connectWebSocket: () => void
  disconnectWebSocket: () => void
  updateNode: (node: Node) => void
}

const NODE_API_URL = '/api/v1/nodes'

export const useNodeStore = create<NodeStore>((set, get) => ({
  nodes: [],
  loading: true,
  error: null,
  lastUpdate: null,
  webSocket: null,

  fetchNodes: async () => {
    try {
      const response = await fetch(NODE_API_URL)
      if (!response.ok) {
        throw new Error(`HTTP ${response.status}`)
      }
      const data: NodeListResponse = await response.json()
      set({
        nodes: data.nodes,
        loading: false,
        error: null,
        lastUpdate: Date.now(),
      })
    } catch (err) {
      set({ loading: false, error: err instanceof Error ? err.message : 'Failed to fetch nodes' })
    }
  },

  connectWebSocket: () => {
    const ws = new WebSocket(`ws://localhost:7380/ws?token=${localStorage.getItem('ozma_token') || ''}`)

    ws.onmessage = (event) => {
      try {
        const message: WebSocketMessage = JSON.parse(event.data)
        if (message.type === 'nodes_update') {
          const payload = message.payload as NodeListResponse
          set({
            nodes: payload.nodes,
            lastUpdate: Date.now(),
          })
        } else if (message.type === 'status_update') {
          const node = message.payload as Node
          set((state) => {
            const nodes = state.nodes.map((n) => (n.id === node.id ? node : n))
            return { nodes, lastUpdate: Date.now() }
          })
        }
      } catch (err) {
        console.error('WebSocket message parse error:', err)
      }
    }

    ws.onerror = (err) => {
      console.error('WebSocket error:', err)
    }

    ws.onclose = () => {
      console.log('WebSocket closed, reconnecting in 3s...')
      setTimeout(() => get().connectWebSocket(), 3000)
    }

    ws.onopen = () => {
      console.log('WebSocket connected')
    }

    set({ webSocket: ws })
  },

  disconnectWebSocket: () => {
    const { webSocket } = get()
    if (webSocket) {
      webSocket.close()
      set({ webSocket: null })
    }
  },

  updateNode: (node) => {
    set((state) => ({
      nodes: state.nodes.map((n) => (n.id === node.id ? node : n)),
    }))
  },
}))
