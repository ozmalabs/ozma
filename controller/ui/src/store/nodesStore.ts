import { create } from 'zustand'
import { Node, NodesState } from '../types'

const API_BASE = '/api/v1'

interface WebSocketMessage {
  type: 'nodes_updated'
  nodes: Node[]
  timestamp: number
}

interface NodesStoreState extends NodesState {
  fetchNodes: () => Promise<void>
  subscribeToUpdates: () => () => void
}

export const useNodesStore = create<NodesStoreState>((_set, _get) => ({
  nodes: [],
  loading: true,
  error: null,
  lastUpdate: null,

  fetchNodes: async () => {
    try {
      const response = await fetch(`${API_BASE}/nodes`)
      if (!response.ok) {
        throw new Error(`HTTP ${response.status}`)
      }
      const nodes: Node[] = await response.json()
      _set({
        nodes,
        loading: false,
        error: null,
        lastUpdate: Date.now(),
      })
    } catch (error) {
      _set({
        loading: false,
        error: error instanceof Error ? error.message : 'Failed to fetch nodes',
      })
    }
  },

  subscribeToUpdates: () => {
    let ws: WebSocket | null = null

    const connect = () => {
      try {
        const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
        ws = new WebSocket(`${protocol}//${window.location.host}/ws/nodes`)

        ws.onopen = () => {
          console.log('[WebSocket] Connected to /ws/nodes')
        }

        ws.onmessage = (event) => {
          try {
            const data: WebSocketMessage = JSON.parse(event.data)
            if (data.type === 'nodes_updated') {
              _set({
                nodes: data.nodes,
                lastUpdate: data.timestamp,
              })
            }
          } catch (e) {
            console.error('[WebSocket] Invalid message format:', e)
          }
        }

        ws.onclose = (event) => {
          console.log('[WebSocket] Disconnected:', event.code, event.reason)
          // Reconnect with exponential backoff
          let attempts = 0
          const reconnect = () => {
            attempts++
            setTimeout(() => {
              console.log(`[WebSocket] Reconnecting... attempt ${attempts}`)
              connect()
            }, Math.min(1000 * 2 ** attempts, 30000))
          }
          reconnect()
        }

        ws.onerror = (error) => {
          console.error('[WebSocket] Error:', error)
        }
      } catch (error) {
        console.error('[WebSocket] Connection failed:', error)
      }
    }

    connect()

    return () => {
      if (ws) {
        ws.close()
      }
    }
  },
}))
