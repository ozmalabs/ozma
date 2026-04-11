import { useEffect, useState, useCallback } from 'react'
import { NodeInfo } from '../types/node'

const API_BASE = '/api/v1'

export function useNodes() {
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [wsConnected, setWsConnected] = useState(false)

  const fetchNodes = useCallback(async () => {
    try {
      setLoading(true)
      const response = await fetch(`${API_BASE}/nodes`)
      if (!response.ok) {
        throw new Error(`Failed to fetch nodes: ${response.statusText}`)
      }
      const data = await response.json()
      setNodes(data.nodes || [])
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Unknown error')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    fetchNodes()
  }, [fetchNodes])

  useEffect(() => {
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const wsUrl = `${protocol}//${window.location.hostname}:5173/ws/notifications`

    const ws = new WebSocket(wsUrl)

    ws.onopen = () => {
      setWsConnected(true)
    }

    ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data)
        if (data.type && data.node) {
          setNodes((prevNodes) => {
            const existingIndex = prevNodes.findIndex((n) => n.id === data.node.id)
            if (existingIndex >= 0) {
              const updatedNodes = [...prevNodes]
              updatedNodes[existingIndex] = data.node
              return updatedNodes
            }
            return [...prevNodes, data.node]
          })
        }
      } catch (err) {
        console.error('Failed to parse WebSocket message:', err)
      }
    }

    ws.onerror = (err) => {
      console.error('WebSocket error:', err)
    }

    ws.onclose = () => {
      setWsConnected(false)
    }

    return () => {
      ws.close()
    }
  }, [])

  return {
    nodes,
    loading,
    error,
    wsConnected,
    refetch: fetchNodes,
  }
}
