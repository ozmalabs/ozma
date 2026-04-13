/**
 * useWebSocket — connects to /api/v1/events?token=<jwt> and dispatches
 * incoming events into the Zustand ozmaStore.
 *
 * Reconnects with exponential backoff: 1 s → 2 s → 4 s → … → 30 s (max).
 */
import { useEffect, useRef } from 'react'
import { useOzmaStore } from '../store/useOzmaStore'

const BACKOFF_INITIAL = 1000
const BACKOFF_MAX = 30000

export function useWebSocket(): void {
  const handleEvent = useOzmaStore((s) => s.handleWsEvent)
  const setConnected = useOzmaStore((s) => s.setWsConnected)

  const wsRef = useRef<WebSocket | null>(null)
  const backoffRef = useRef(BACKOFF_INITIAL)
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const unmountedRef = useRef(false)

  const connect = () => {
    if (unmountedRef.current) return

    const token = localStorage.getItem('ozma_token') ?? ''
    const wsUrl = `${window.location.protocol === 'https:' ? 'wss:' : 'ws:'}//${window.location.host}/api/v1/events${token ? `?token=${encodeURIComponent(token)}` : ''}`
    
    const ws = new WebSocket(wsUrl)
    wsRef.current = ws

    ws.onopen = () => {
      console.log('WebSocket connected')
      setConnected(true)
      backoffRef.current = BACKOFF_INITIAL
    }

    ws.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data)
        handleEvent(data)
      } catch (e) {
        console.error('Error parsing WebSocket message:', e)
      }
    }

    ws.onclose = () => {
      if (unmountedRef.current) return
      
      setConnected(false)
      console.log('WebSocket disconnected, reconnecting in', backoffRef.current, 'ms')
      
      timerRef.current = setTimeout(() => {
        backoffRef.current = Math.min(backoffRef.current * 2, BACKOFF_MAX)
        connect()
      }, backoffRef.current)
    }

    ws.onerror = (error) => {
      console.error('WebSocket error:', error)
      ws.close()
    }
  }

  useEffect(() => {
    connect()

    return () => {
      unmountedRef.current = true
      if (timerRef.current) {
        clearTimeout(timerRef.current)
      }
      if (wsRef.current) {
        wsRef.current.close()
      }
    }
  }, [])
}
