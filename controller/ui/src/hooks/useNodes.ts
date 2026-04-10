import { create } from 'zustand'
import { NodeInfo, NodesResponse, WebSocketEvent } from '../types'

interface NodesStore {
  nodes: NodeInfo[]
  activeNodeId: string | null
  isLoading: boolean
  error: string | null
  lastUpdate: number | null
  fetchNodes: () => Promise<void>
  setNodes: (nodes: NodeInfo[]) => void
  updateNode: (node: NodeInfo) => void
  setActiveNodeId: (id: string | null) => void
  setError: (error: string | null) => void
  ws: WebSocket | null
  connectWebSocket: () => void
  disconnectWebSocket: () => void
}

export const useNodesStore = create<NodesStore>((set, get) => {
  let ws: WebSocket | null = null

  const setupWebSocket = () => {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const wsUrl = `${protocol}//${window.location.host}/ws`
    
    ws = new WebSocket(wsUrl)
    
    ws.onopen = () => {
      console.log('WebSocket connected')
    }
    
    ws.onmessage = (event) => {
      try {
        const data: WebSocketEvent = JSON.parse(event.data)
        handleWebSocketMessage(data)
      } catch (err) {
        console.error('Failed to parse WebSocket message:', err)
      }
    }
    
    ws.onclose = () => {
      console.log('WebSocket disconnected, reconnecting in 3s...')
      setTimeout(() => {
        if (!get().ws || get().ws === ws) {
          setupWebSocket()
        }
      }, 3000)
    }
    
    ws.onerror = (error) => {
      console.error('WebSocket error:', error)
    }
    
    set({ ws })
  }

  const handleWebSocketMessage = (data: WebSocketEvent) => {
    if (data.type === 'node_update') {
      set((state) => {
        const existingIndex = state.nodes.findIndex(n => n.id === data.payload.node.id)
        if (existingIndex >= 0) {
          const newNodes = [...state.nodes]
          newNodes[existingIndex] = data.payload.node
          return { nodes: newNodes, lastUpdate: Date.now() }
        }
        return { nodes: [...state.nodes, data.payload.node], lastUpdate: Date.now() }
      })
    } else if (data.type === 'system_update') {
      set({ activeNodeId: data.payload.active_node_id, lastUpdate: Date.now() })
    }
  }

  const fetchNodes = async () => {
    try {
      set({ isLoading: true, error: null })
      const response = await fetch('/api/v1/nodes')
      if (!response.ok) {
        throw new Error(`Failed to fetch nodes: ${response.statusText}`)
      }
      const data: NodesResponse = await response.json()
      set({ nodes: data.nodes, isLoading: false, lastUpdate: Date.now() })
    } catch (err) {
      set({ isLoading: false, error: err instanceof Error ? err.message : 'Unknown error' })
    }
  }

  const connectWebSocket = () => {
    if (!ws) {
      setupWebSocket()
    }
  }

  const disconnectWebSocket = () => {
    if (ws) {
      ws.close()
      ws = null
      set({ ws: null })
    }
  }

  return {
    nodes: [],
    activeNodeId: null,
    isLoading: false,
    error: null,
    lastUpdate: null,
    fetchNodes,
    setNodes: (nodes) => set({ nodes, lastUpdate: Date.now() }),
    updateNode: (node) => set((state) => {
      const existingIndex = state.nodes.findIndex(n => n.id === node.id)
      if (existingIndex >= 0) {
        const newNodes = [...state.nodes]
        newNodes[existingIndex] = node
        return { nodes: newNodes, lastUpdate: Date.now() }
      }
      return { nodes: [...state.nodes, node], lastUpdate: Date.now() }
    }),
    setActiveNodeId: (id) => set({ activeNodeId: id }),
    setError: (error) => set({ error }),
    ws: null,
    connectWebSocket,
    disconnectWebSocket,
  }
})

export const useNodes = () => {
  const { fetchNodes, connectWebSocket, disconnectWebSocket, ...rest } = useNodesStore()
  
  return {
    ...rest,
    fetchNodes,
    connectWebSocket,
    disconnectWebSocket,
  }
}
