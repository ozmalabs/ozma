import { create } from 'zustand'
import { useEffect, useState } from 'react'

export interface NodeInfo {
  id: string
  name: string
  hostname: string
  active: boolean
  ip: string
  last_seen: string
  status: 'online' | 'offline' | 'connecting'
  machine_class: 'workstation' | 'server' | 'kiosk'
}

interface NodeState {
  nodes: NodeInfo[]
  loading: boolean
  error: string | null
  wsConnected: boolean
  fetchNodes: () => Promise<void>
  clearError: () => void
  setNodes: (nodes: NodeInfo[]) => void
}

const API_BASE = '/api/v1'

async function fetchNodesData(): Promise<NodeInfo[]> {
  const response = await fetch(`${API_BASE}/nodes`)
  if (!response.ok) {
    throw new Error(`Failed to fetch nodes: ${response.statusText}`)
  }
  const data = await response.json()
  return data.nodes || data
}

export const useNodeStore = create<NodeState>((set) => ({
  nodes: [],
  loading: false,
  error: null,
  wsConnected: false,

  fetchNodes: async () => {
    set({ loading: true, error: null })
    try {
      const nodes = await fetchNodesData()
      set({ nodes, loading: false, error: null })
    } catch (error) {
      set({
        loading: false,
        error: error instanceof Error ? error.message : 'Failed to fetch nodes',
      })
    }
  },

  clearError: () => set({ error: null }),
  setNodes: (nodes) => set({ nodes }),
}))

export function useNodes() {
  const { nodes, loading, error, fetchNodes, clearError, setNodes } = useNodeStore()
  const [ws, setWs] = useState<WebSocket | null>(null)

  useEffect(() => {
    fetchNodes()
  }, [fetchNodes])

  useEffect(() => {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const wsUrl = `${protocol}//${window.location.host}/ws`

    const socket = new WebSocket(wsUrl)

    socket.onopen = () => {
      console.log('WebSocket connected')
      setWs(socket)
    }

    socket.onclose = (event) => {
      console.log('WebSocket closed:', event)
      setWs(null)
    }

    socket.onerror = (error) => {
      console.error('WebSocket error:', error)
    }

    socket.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data)
        if (data.type === 'nodes') {
          setNodes(data.nodes)
        }
      } catch {
        // Ignore non-JSON messages
      }
    }

    return () => {
      socket.close()
    }
  }, [setNodes])

  return { nodes, loading, error, wsConnected: ws !== null, fetchNodes, clearError }
}
