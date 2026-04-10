import React from 'react'
import { create } from 'zustand'
import type { Node, NodesState } from '../types'

const fetchNodes = async (): Promise<Node[]> => {
  const response = await fetch('/api/v1/nodes')
  if (!response.ok) {
    throw new Error(`Failed to fetch nodes: ${response.statusText}`)
  }
  return response.json()
}

const useNodesStore = create<NodesState & { wsRef: React.MutableRefObject<WebSocket | null> }>((set, get) => ({
  nodes: [],
  loading: false,
  error: null,
  wsRef: { current: null },

  refresh: async () => {
    const { loading } = get()
    if (loading) return

    set({ loading: true, error: null })
    try {
      const nodes = await fetchNodes()
      set({ nodes, loading: false, error: null })
    } catch (error) {
      set({ loading: false, error: error instanceof Error ? error.message : 'Unknown error' })
    }
  },
}))

export const useNodes = () => {
  const { nodes, loading, error, refresh, wsRef } = useNodesStore()

  React.useEffect(() => {
    const wsUrl = window.location.protocol === 'https:'
      ? 'wss://localhost:7380/ws/nodes'
      : 'ws://localhost:7380/ws/nodes'

    const ws = new WebSocket(wsUrl)
    wsRef.current = ws

    ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data)
        if (data.type === 'node_update' || data.type === 'nodes_refresh') {
          useNodesStore.setState({ nodes: data.nodes || [], loading: false })
        }
      } catch (e) {
        console.error('Failed to parse WebSocket message:', e)
      }
    }

    ws.onerror = (error) => {
      console.error('WebSocket error:', error)
    }

    ws.onclose = () => {
      console.log('WebSocket closed, will retry...')
      setTimeout(() => {
        refresh()
      }, 3000)
    }

    return () => {
      ws.close()
    }
  }, [refresh, wsRef])

  return { nodes, loading, error, refresh }
}
