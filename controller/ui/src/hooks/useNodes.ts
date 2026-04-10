import React from 'react'
import { create } from 'zustand'
import { Node, WebSocketEvent } from '../types/api'

interface NodesStore {
  nodes: Node[]
  loading: boolean
  error: string | null
  lastUpdate: Date | null
  fetchNodes: () => Promise<void>
  addNode: (node: Node) => void
  updateNode: (node: Node) => void
  removeNode: (id: string) => void
  clearError: () => void
}

const API_BASE_URL = '/api/v1'

export const useNodesStore = create<NodesStore>((set) => ({
  nodes: [],
  loading: false,
  error: null,
  lastUpdate: null,

  fetchNodes: async () => {
    set({ loading: true, error: null })
    try {
      const response = await fetch(`${API_BASE_URL}/nodes`)
      if (!response.ok) {
        throw new Error(`Failed to fetch nodes: ${response.status}`)
      }
      const data = await response.json()
      set({ nodes: data.nodes, loading: false, lastUpdate: new Date() })
    } catch (error) {
      console.error('Error fetching nodes:', error)
      set({ loading: false, error: 'Failed to fetch nodes from server' })
    }
  },

  addNode: (node) => {
    set((state) => ({
      nodes: [...state.nodes, node],
      lastUpdate: new Date(),
    }))
  },

  updateNode: (node) => {
    set((state) => ({
      nodes: state.nodes.map((n) => (n.id === node.id ? node : n)),
      lastUpdate: new Date(),
    }))
  },

  removeNode: (id) => {
    set((state) => ({
      nodes: state.nodes.filter((n) => n.id !== id),
      lastUpdate: new Date(),
    }))
  },

  clearError: () => set({ error: null }),
}))

export function useWebSocket() {
  const { addNode, updateNode } = useNodesStore()

  React.useEffect(() => {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const wsUrl = `${protocol}//localhost:7380/ws/node-updates`

    let ws: WebSocket | null = null
    let reconnectTimeout: number | null = null

    const connect = () => {
      try {
        ws = new WebSocket(wsUrl)

        ws.onopen = () => {
          console.log('WebSocket connected')
        }

        ws.onmessage = (event) => {
          try {
            const data: WebSocketEvent = JSON.parse(event.data)
            if (data.type === 'nodes_update') {
              data.payload.nodes.forEach((node) => {
                const nodesState = useNodesStore.getState().nodes
                const existingNode = nodesState.find((n) => n.id === node.id)
                if (existingNode) {
                  updateNode(node)
                } else {
                  addNode(node)
                }
              })
            } else if (data.type === 'node_status_change') {
              updateNode(data.payload.nodes[0])
            }
          } catch (error) {
            console.error('Error parsing WebSocket message:', error)
          }
        }

        ws.onclose = () => {
          console.log('WebSocket disconnected, reconnecting in 3s...')
          reconnectTimeout = window.setTimeout(connect, 3000)
        }

        ws.onerror = (error) => {
          console.error('WebSocket error:', error)
        }
      } catch (error) {
        console.error('Failed to connect to WebSocket:', error)
        reconnectTimeout = window.setTimeout(connect, 3000)
      }
    }

    connect()

    return () => {
      if (ws) {
        ws.close()
      }
      if (reconnectTimeout) {
        clearTimeout(reconnectTimeout)
      }
    }
  }, [addNode, updateNode])

  return null
}
