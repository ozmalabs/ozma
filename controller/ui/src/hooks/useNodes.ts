import { useEffect, useState, useCallback } from 'react'
import type { NodeInfo } from '../types'

const API_BASE = '/api/v1'

export function useNodes() {
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [wsConnected, setWsConnected] = useState(false)

  const fetchNodes = useCallback(async () => {
    try {
      const response = await fetch(`${API_BASE}/nodes`, {
        headers: {
          'Accept': 'application/json',
        },
      })

      if (!response.ok) {
        throw new Error(`Failed to fetch nodes: ${response.statusText}`)
      }

      const data = await response.json()
      setNodes(data.nodes || data)
      setError(null)
    } catch (err) {
      setError(err instanceof Error ? err.message : 'Unknown error')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    fetchNodes()

    // Set up WebSocket for live updates
    const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
    const wsUrl = `${protocol}//${window.location.host}/ws/updates`

    const ws = new WebSocket(wsUrl)

    ws.onopen = () => {
      setWsConnected(true)
      // Request initial state on connect
      ws.send(JSON.stringify({ type: 'subscribe', resource: 'nodes' }))
    }

    ws.onclose = () => {
      setWsConnected(false)
      // Retry connection after delay
      setTimeout(() => {
        if (ws.readyState !== WebSocket.OPEN) {
          ws.close()
          // Recreate WebSocket in next effect cycle
        }
      }, 2000)
    }

    ws.onmessage = (event) => {
      try {
        const message = JSON.parse(event.data)
        if (message.type === 'update' && message.resource === 'nodes') {
          setNodes(message.data.nodes || message.data)
        } else if (message.type === 'sync') {
          setNodes(message.nodes || [])
        }
      } catch (err) {
        console.error('Failed to parse WebSocket message:', err)
      }
    }

    ws.onerror = (err) => {
      console.error('WebSocket error:', err)
    }

    return () => {
      ws.close()
    }
  }, [fetchNodes])

  return { nodes, loading, error, wsConnected, refresh: fetchNodes }
}
