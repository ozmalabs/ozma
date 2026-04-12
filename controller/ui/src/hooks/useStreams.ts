import { useState, useEffect, useCallback, useRef } from 'react'

export interface StreamInfo {
  node_id: string
  url: string
  active: boolean
  type: 'hls-remote' | 'mjpeg' | 'none'
}

export interface StreamStats {
  node_id: string
  encoder: string
  hw_type: string
  codec_family: string
  fps_actual: number
  bitrate_target: string
  frames_sent: number
  uptime_s: number
  restarts: number
  active: boolean
}

export interface RecordingStatus {
  node_id: string
  recording: boolean
  filename?: string
  duration_s?: number
  size_bytes?: number
}

export interface OverlayConfig {
  node_id: string
  overlays: OverlayItem[]
}

export interface OverlayItem {
  id: string
  type: 'text' | 'image' | 'timestamp'
  label: string
  x: number
  y: number
  enabled: boolean
}

export function useStreams(pollIntervalMs = 3000) {
  const [streams, setStreams] = useState<StreamInfo[]>([])
  const [stats, setStats] = useState<Record<string, StreamStats>>({})
  const [recordings, setRecordings] = useState<Record<string, RecordingStatus>>({})
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null)

  const fetchStreams = useCallback(async () => {
    try {
      const res = await fetch('/api/v1/streams')
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data: StreamInfo[] = await res.json()
      setStreams(data)
      setError(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to fetch streams')
    } finally {
      setLoading(false)
    }
  }, [])

  const fetchStats = useCallback(async () => {
    try {
      const res = await fetch('/api/v1/streams/stats')
      if (!res.ok) return
      const data: Record<string, StreamStats> = await res.json()
      setStats(data)
    } catch {
      // stats are best-effort
    }
  }, [])

  const fetchRecordings = useCallback(async () => {
    try {
      const res = await fetch('/api/v1/streams/recordings')
      if (!res.ok) return
      const data: RecordingStatus[] = await res.json()
      const map: Record<string, RecordingStatus> = {}
      for (const r of data) map[r.node_id] = r
      setRecordings(map)
    } catch {
      // best-effort
    }
  }, [])

  const refresh = useCallback(() => {
    fetchStreams()
    fetchStats()
    fetchRecordings()
  }, [fetchStreams, fetchStats, fetchRecordings])

  useEffect(() => {
    refresh()
    timerRef.current = setInterval(refresh, pollIntervalMs)
    return () => {
      if (timerRef.current) clearInterval(timerRef.current)
    }
  }, [refresh, pollIntervalMs])

  const switchCodec = useCallback(async (nodeId: string, codec: string, hwAccel: string) => {
    const res = await fetch(`/api/v1/streams/${encodeURIComponent(nodeId)}/codec`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ codec, hw_accel: hwAccel, bitrate: 'auto', max_fps: 30, max_width: 1920 }),
    })
    if (!res.ok) throw new Error(`Codec switch failed: HTTP ${res.status}`)
    await fetchStats()
  }, [fetchStats])

  const startRecording = useCallback(async (nodeId: string) => {
    const res = await fetch(`/api/v1/streams/${encodeURIComponent(nodeId)}/record/start`, {
      method: 'POST',
    })
    if (!res.ok) throw new Error(`Start recording failed: HTTP ${res.status}`)
    await fetchRecordings()
  }, [fetchRecordings])

  const stopRecording = useCallback(async (nodeId: string) => {
    const res = await fetch(`/api/v1/streams/${encodeURIComponent(nodeId)}/record/stop`, {
      method: 'POST',
    })
    if (!res.ok) throw new Error(`Stop recording failed: HTTP ${res.status}`)
    await fetchRecordings()
  }, [fetchRecordings])

  return {
    streams,
    stats,
    recordings,
    loading,
    error,
    refresh,
    switchCodec,
    startRecording,
    stopRecording,
  }
}
