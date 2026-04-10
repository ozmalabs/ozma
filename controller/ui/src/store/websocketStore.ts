import { create } from 'zustand'
import { Node } from '../types'
import { useNodeStore } from './nodeStore'

interface WebSocketStore {
  ws: WebSocket | null
  isConnected: boolean
  connect: () => void
  disconnect: () => void
}

export const useWebSocketStore = create<WebSocketStore>((set, get) => ({
  ws: null,
  isConnected: false,

  connect: () => {
    const token = localStorage.getItem('ozma_auth_token')
    const url = token
      ? `ws://localhost:7380/ws/node-updates?token=${encodeURIComponent(token)}`
      : `ws://localhost:7380/ws/node-updates`

    const ws = new WebSocket(url)

    ws.onopen = () => {
      set({ isConnected: true, ws })
    }

    ws.onclose = () => {
      set({ isConnected: false, ws: null })
      // Auto-reconnect after delay
      setTimeout(() => get().connect(), 3000)
    }

    ws.onerror = (error) => {
      console.error('WebSocket error:', error)
      set({ isConnected: false, ws: null })
    }

    ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data)
        handleNodeUpdate(data)
      } catch (err) {
        console.error('Failed to parse WebSocket message:', err)
      }
    }
  },

  disconnect: () => {
    const { ws } = get()
    if (ws) {
      ws.close()
      set({ ws: null, isConnected: false })
    }
  },
}))

// Helper function to handle node updates from WebSocket
const handleNodeUpdate = (data: { type: string; node: Node }) => {
  const nodeStore = useNodeStore.getState()

  switch (data.type) {
    case 'node_added':
    case 'node_updated':
      nodeStore.updateNode(data.node)
      break
    case 'node_removed':
      nodeStore.removeNode(data.node.id)
      break
  }
}
