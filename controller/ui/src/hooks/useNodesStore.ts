import { create } from 'zustand'
import { NodeInfo, NodesState } from '../types/node'

const API_BASE = '/api/v1'

export const useNodesStore = create<NodesState>((set, get) => {
  let socket: WebSocket | null = null

  return {
    nodes: [],
    loading: true,
    error: null,
    lastUpdate: null,

    refreshNodes: async () => {
      set({ loading: true, error: null })
      try {
        const response = await fetch(`${API_BASE}/nodes`)
        if (!response.ok) {
          throw new Error(`Failed to fetch nodes: ${response.statusText}`)
        }
        const data: NodeInfo[] = await response.json()
        set({ nodes: data, loading: false, lastUpdate: Date.now() })
      } catch (error) {
        set({ loading: false, error: error instanceof Error ? error.message : 'Unknown error' })
      }
    },

    connectWebSocket: () => {
      if (socket) {
        socket.close()
      }

      socket = new WebSocket(`ws://localhost:7380/api/v1/events`)

      socket.onopen = () => {
        console.log('WebSocket connected for node events')
        // Request initial state on connect
        get().refreshNodes()
      }

      socket.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data)
          if (data.type === 'nodes') {
            // Update nodes with the new data
            set({ nodes: data.nodes, lastUpdate: Date.now() })
          } else if (data.type === 'node_update') {
            // Update a single node
            const node = data.node as NodeInfo
            set((state) => ({
              nodes: state.nodes.map((n) => (n.id === node.id ? node : n)),
              lastUpdate: Date.now(),
            }))
          }
        } catch (error) {
          console.error('Failed to parse WebSocket message:', error)
        }
      }

      socket.onerror = (error) => {
        console.error('WebSocket error:', error)
      }

      socket.onclose = () => {
        console.log('WebSocket closed, reconnecting in 3 seconds...')
        setTimeout(() => {
          const currentSocket = get().nodes.length > 0 ? socket : null
          if (currentSocket === socket) {
            get().connectWebSocket()
          }
        }, 3000)
      }
    },
  }
})

// Initialize WebSocket connection on module load
if (typeof window !== 'undefined') {
  useNodesStore.getState().connectWebSocket()
}
