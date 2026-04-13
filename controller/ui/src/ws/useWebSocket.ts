/**
 * useWebSocket — connects to /api/v1/events?token=<jwt> and dispatches
 * incoming events into the Zustand ozmaStore.
 *
 * Reconnects with exponential backoff: 1 s → 2 s → 4 s → … → 30 s (max).
 */
import { useEffect, useRef, useCallback } from 'react'
import { useOzmaStore } from '../store/ozmaStore'

const WS_BASE =
  ((import.meta.env.VITE_API_BASE as string | undefined) ?? '').replace(/^http/, 'ws') ||
  `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}`

const BACKOFF_INITIAL = 1_000
const BACKOFF_MAX = 30_000

function getToken(): string {
  try {
    return localStorage.getItem('ozma_token') ?? ''
  } catch {
    return ''
  }
}

export function useWebSocket(): void {
  const handleEvent = useOzmaStore((s) => s.handleWsEvent)
  const setConnected = useOzmaStore((s) => s.setWsConnected)

  const wsRef = useRef<WebSocket | null>(null)
  const backoffRef = useRef(BACKOFF_INITIAL)
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const unmountedRef = useRef(false)

  const connect = useCallback(() => {
    if (unmountedRef.current) return

    const token = getToken()
    const url = `${WS_BASE}/api/v1/events${token ? `?token=${encodeURIComponent(token)}` : ''}`

    const ws = new WebSocket(url)
    wsRef.current = ws

    ws.onopen = () => {
      backoffRef.current = BACKOFF_INITIAL
      setConnected(true)
    }

    ws.onmessage = (ev: MessageEvent) => {
      try {
        const event = JSON.parse(ev.data as string) as Record<string, unknown>
        handleEvent(event)
      } catch {
        // ignore malformed frames
      }
    }

    ws.onerror = () => {
      ws.close()
    }

    ws.onclose = () => {
      setConnected(false)
      if (unmountedRef.current) return
      const delay = backoffRef.current
      backoffRef.current = Math.min(delay * 2, BACKOFF_MAX)
      timerRef.current = setTimeout(connect, delay)
    }
  }, [handleEvent, setConnected])

  useEffect(() => {
    unmountedRef.current = false
    connect()

    return () => {
      unmountedRef.current = true
      if (timerRef.current !== null) clearTimeout(timerRef.current)
      wsRef.current?.close()
    }
  }, [connect])
}
/**
 * useWebSocket — connects to /api/v1/events?token=<jwt> and dispatches
 * incoming events into the Zustand ozmaStore.
 *
 * Reconnects with exponential backoff: 1 s → 2 s → 4 s → … → 30 s (max).
 */
import { useEffect, useRef, useCallback } from 'react'
import { useOzmaStore } from '../store/ozmaStore'

const WS_BASE =
  ((import.meta.env.VITE_API_BASE as string | undefined) ?? '').replace(/^http/, 'ws') ||
  `${window.location.protocol === 'https:' ? 'wss' : 'ws'}://${window.location.host}`

const BACKOFF_INITIAL = 1_000
const BACKOFF_MAX = 30_000

function getToken(): string {
  try {
    return localStorage.getItem('ozma_token') ?? ''
  } catch {
    return ''
  }
}

export function useWebSocket(): void {
  const handleEvent = useOzmaStore((s) => s.handleWsEvent)
  const setConnected = useOzmaStore((s) => s.setWsConnected)

  const wsRef = useRef<WebSocket | null>(null)
  const backoffRef = useRef(BACKOFF_INITIAL)
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const unmountedRef = useRef(false)

  const connect = useCallback(() => {
    if (unmountedRef.current) return

    const token = getToken()
    const url = `${WS_BASE}/api/v1/events${token ? `?token=${encodeURIComponent(token)}` : ''}`

    const ws = new WebSocket(url)
    wsRef.current = ws

    ws.onopen = () => {
      backoffRef.current = BACKOFF_INITIAL
      setConnected(true)
    }

    ws.onmessage = (ev: MessageEvent) => {
      try {
        const event = JSON.parse(ev.data as string) as Record<string, unknown>
        handleEvent(event)
      } catch {
        // ignore malformed frames
      }
    }

    ws.onerror = () => {
      ws.close()
    }

    ws.onclose = () => {
      setConnected(false)
      if (unmountedRef.current) return
      const delay = backoffRef.current
      backoffRef.current = Math.min(delay * 2, BACKOFF_MAX)
      timerRef.current = setTimeout(connect, delay)
    }
  }, [handleEvent, setConnected])

  useEffect(() => {
    unmountedRef.current = false
    connect()

    return () => {
      unmountedRef.current = true
      if (timerRef.current !== null) clearTimeout(timerRef.current)
      wsRef.current?.close()
    }
  }, [connect])
}
