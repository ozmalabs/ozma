/**
 * RoutingGraphPage — interactive routing topology visualizer.
 *
 * Fetches /api/v1/graph/topology on mount and on every WebSocket
 * "graph.*" event, then renders an SVG force-directed graph with:
 *   - Nodes coloured by device type
 *   - Edges coloured by link status (active/warm/standby/failed/unknown)
 *   - Live metric badges on edges (latency, loss)
 *   - Click-to-inspect panel for devices and links
 *   - Auto-refresh every 10 s
 */

import { useState, useEffect, useRef, useCallback } from 'react'

const API_BASE = import.meta.env.VITE_API_BASE ?? ''
const WS_BASE = API_BASE.replace(/^http/, 'ws')

// ── Types ────────────────────────────────────────────────────────────────────

interface TopologyPort {
  id: string
  direction: 'source' | 'sink'
  media_type: string
  active: boolean
  label: string | null
}

interface TopologyDevice {
  id: string
  name: string
  type: string
  assurance_level: number
  port_count: number
  ports: TopologyPort[]
}

interface LiveMetrics {
  latency_ms?: number
  loss_rate?: number
  jitter_p99_ms?: number
  bandwidth_bps?: number
  [key: string]: number | undefined
}

interface TopologyLink {
  id: string
  source_device: string
  source_port: string
  sink_device: string
  sink_port: string
  transport: string
  status: 'active' | 'warm' | 'standby' | 'failed' | 'unknown'
  bidirectional: boolean
  live_metrics?: LiveMetrics
}

interface Topology {
  generation: number
  device_count: number
  link_count: number
  devices: TopologyDevice[]
  links: TopologyLink[]
}

interface LinkMetrics {
  link_id: string
  status: string
  last_measured: number
  live_metrics: LiveMetrics
  sparklines: Record<string, Array<{ t: number; v: number; n: number }>>
  tier: number
}

// ── Layout helpers ────────────────────────────────────────────────────────────

interface NodePos {
  id: string
  x: number
  y: number
  vx: number
  vy: number
}

const DEVICE_TYPE_COLOR: Record<string, string> = {
  controller:        '#6366f1',
  node:              '#22c55e',
  target:            '#f59e0b',
  display:           '#3b82f6',
  audio_interface:   '#a855f7',
  capture_card:      '#ec4899',
  network_interface: '#14b8a6',
  camera:            '#f97316',
  speaker:           '#84cc16',
  microphone:        '#06b6d4',
  virtual:           '#94a3b8',
  service:           '#64748b',
}

const LINK_STATUS_COLOR: Record<string, string> = {
  active:  '#22c55e',
  warm:    '#f59e0b',
  standby: '#94a3b8',
  failed:  '#ef4444',
  unknown: '#64748b',
}

function deviceColor(type: string): string {
  return DEVICE_TYPE_COLOR[type] ?? '#94a3b8'
}

function linkColor(status: string): string {
  return LINK_STATUS_COLOR[status] ?? '#64748b'
}

/** Simple force-directed layout — runs a fixed number of iterations. */
function forceLayout(
  devices: TopologyDevice[],
  links: TopologyLink[],
  width: number,
  height: number,
  iterations = 120,
): Map<string, { x: number; y: number }> {
  const positions = new Map<string, NodePos>()
  const cx = width / 2
  const cy = height / 2

  // Initialise in a circle
  devices.forEach((d, i) => {
    const angle = (2 * Math.PI * i) / Math.max(devices.length, 1)
    const r = Math.min(width, height) * 0.35
    positions.set(d.id, {
      id: d.id,
      x: cx + r * Math.cos(angle),
      y: cy + r * Math.sin(angle),
      vx: 0,
      vy: 0,
    })
  })

  const k = Math.sqrt((width * height) / Math.max(devices.length, 1))
  const repulsion = k * k
  const attraction = 0.05

  for (let iter = 0; iter < iterations; iter++) {
    const cooling = 1 - iter / iterations

    // Repulsion between all pairs
    const ids = Array.from(positions.keys())
    for (let a = 0; a < ids.length; a++) {
      for (let b = a + 1; b < ids.length; b++) {
        const pa = positions.get(ids[a])!
        const pb = positions.get(ids[b])!
        const dx = pa.x - pb.x
        const dy = pa.y - pb.y
        const dist = Math.max(Math.sqrt(dx * dx + dy * dy), 1)
        const force = repulsion / dist
        pa.vx += (dx / dist) * force
        pa.vy += (dy / dist) * force
        pb.vx -= (dx / dist) * force
        pb.vy -= (dy / dist) * force
      }
    }

    // Attraction along links
    for (const lnk of links) {
      const pa = positions.get(lnk.source_device)
      const pb = positions.get(lnk.sink_device)
      if (!pa || !pb) continue
      const dx = pb.x - pa.x
      const dy = pb.y - pa.y
      const dist = Math.max(Math.sqrt(dx * dx + dy * dy), 1)
      const force = dist * attraction
      pa.vx += (dx / dist) * force
      pa.vy += (dy / dist) * force
      pb.vx -= (dx / dist) * force
      pb.vy -= (dy / dist) * force
    }

    // Gravity toward centre
    for (const p of positions.values()) {
      p.vx += (cx - p.x) * 0.01
      p.vy += (cy - p.y) * 0.01
    }

    // Apply velocity with cooling
    for (const p of positions.values()) {
      p.x += p.vx * cooling
      p.y += p.vy * cooling
      p.vx *= 0.5
      p.vy *= 0.5
      // Clamp to canvas
      p.x = Math.max(40, Math.min(width - 40, p.x))
      p.y = Math.max(40, Math.min(height - 40, p.y))
    }
  }

  const result = new Map<string, { x: number; y: number }>()
  for (const [id, p] of positions) {
    result.set(id, { x: p.x, y: p.y })
  }
  return result
}

// ── Sparkline component ───────────────────────────────────────────────────────

function Sparkline({ points, color = '#22c55e', width = 80, height = 24 }: {
  points: Array<{ t: number; v: number }>
  color?: string
  width?: number
  height?: number
}) {
  if (points.length < 2) return null
  const vals = points.map(p => p.v)
  const min = Math.min(...vals)
  const max = Math.max(...vals)
  const range = max - min || 1
  const pts = points.map((p, i) => {
    const x = (i / (points.length - 1)) * width
    const y = height - ((p.v - min) / range) * height
    return `${x},${y}`
  }).join(' ')
  return (
    <svg width={width} height={height} className="inline-block">
      <polyline points={pts} fill="none" stroke={color} strokeWidth="1.5" />
    </svg>
  )
}

// ── Main component ────────────────────────────────────────────────────────────

export default function RoutingGraphPage() {
  const [topology, setTopology] = useState<Topology | null>(null)
  const [positions, setPositions] = useState<Map<string, { x: number; y: number }>>(new Map())
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [selectedDevice, setSelectedDevice] = useState<TopologyDevice | null>(null)
  const [selectedLink, setSelectedLink] = useState<TopologyLink | null>(null)
  const [linkMetrics, setLinkMetrics] = useState<LinkMetrics | null>(null)
  const [dragging, setDragging] = useState<string | null>(null)
  const [dragOffset, setDragOffset] = useState({ x: 0, y: 0 })
  const svgRef = useRef<SVGSVGElement>(null)
  const generationRef = useRef<number>(-1)
  const CANVAS_W = 900
  const CANVAS_H = 600
  const NODE_R = 22

  const fetchTopology = useCallback(async () => {
    try {
      const res = await fetch(`${API_BASE}/api/v1/graph/topology?include_metrics=1&include_ports=1`)
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      const data: Topology = await res.json()
      setTopology(data)
      setError(null)
      // Re-run layout only when topology generation changes
      if (data.generation !== generationRef.current) {
        generationRef.current = data.generation
        const pos = forceLayout(data.devices, data.links, CANVAS_W, CANVAS_H)
        setPositions(pos)
      }
    } catch (e: any) {
      setError(e.message ?? 'Failed to load topology')
    } finally {
      setLoading(false)
    }
  }, [])

  const fetchLinkMetrics = useCallback(async (linkId: string) => {
    try {
      const res = await fetch(`${API_BASE}/api/v1/graph/links/${encodeURIComponent(linkId)}/metrics?tier=1&limit=60`)
      if (!res.ok) return
      const data: LinkMetrics = await res.json()
      setLinkMetrics(data)
    } catch {
      // non-fatal
    }
  }, [])

  // Initial load + 10 s auto-refresh
  useEffect(() => {
    fetchTopology()
    const interval = setInterval(fetchTopology, 10_000)
    return () => clearInterval(interval)
  }, [fetchTopology])

  // WebSocket: re-fetch on graph change events
  useEffect(() => {
    const wsUrl = `${WS_BASE}/api/v1/events`
    let ws: WebSocket
    let reconnectTimer: ReturnType<typeof setTimeout>

    function connect() {
      ws = new WebSocket(wsUrl)
      ws.onmessage = (ev) => {
        try {
          const msg = JSON.parse(ev.data)
          if (typeof msg.type === 'string' && msg.type.startsWith('graph.')) {
            fetchTopology()
          }
        } catch { /* ignore */ }
      }
      ws.onclose = () => {
        reconnectTimer = setTimeout(connect, 5000)
      }
    }
    connect()
    return () => {
      ws?.close()
      clearTimeout(reconnectTimer)
    }
  }, [fetchTopology])

  // Fetch link metrics when a link is selected
  useEffect(() => {
    if (selectedLink) {
      fetchLinkMetrics(selectedLink.id)
    } else {
      setLinkMetrics(null)
    }
  }, [selectedLink, fetchLinkMetrics])

  // ── Drag handlers ──────────────────────────────────────────────────────────

  function onNodeMouseDown(e: React.MouseEvent, deviceId: string) {
    e.stopPropagation()
    const pos = positions.get(deviceId)
    if (!pos) return
    const svgRect = svgRef.current?.getBoundingClientRect()
    if (!svgRect) return
    const scaleX = CANVAS_W / svgRect.width
    const scaleY = CANVAS_H / svgRect.height
    setDragging(deviceId)
    setDragOffset({
      x: (e.clientX - svgRect.left) * scaleX - pos.x,
      y: (e.clientY - svgRect.top) * scaleY - pos.y,
    })
  }

  function onSvgMouseMove(e: React.MouseEvent) {
    if (!dragging) return
    const svgRect = svgRef.current?.getBoundingClientRect()
    if (!svgRect) return
    const scaleX = CANVAS_W / svgRect.width
    const scaleY = CANVAS_H / svgRect.height
    const nx = Math.max(NODE_R, Math.min(CANVAS_W - NODE_R, (e.clientX - svgRect.left) * scaleX - dragOffset.x))
    const ny = Math.max(NODE_R, Math.min(CANVAS_H - NODE_R, (e.clientY - svgRect.top) * scaleY - dragOffset.y))
    setPositions(prev => {
      const next = new Map(prev)
      next.set(dragging, { x: nx, y: ny })
      return next
    })
  }

  function onSvgMouseUp() {
    setDragging(null)
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin mx-auto mb-4" />
          <p className="text-muted-foreground">Loading routing graph…</p>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center max-w-md">
          <p className="text-destructive font-semibold mb-2">Failed to load topology</p>
          <p className="text-muted-foreground mb-4">{error}</p>
          <button
            onClick={fetchTopology}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90"
          >
            Retry
          </button>
        </div>
      </div>
    )
  }

  const topo = topology!

  return (
    <div className="flex flex-col h-full gap-4">
      {/* Header */}
      <div className="flex justify-between items-center">
        <div>
          <h2 className="text-2xl font-bold">Routing Graph</h2>
          <p className="text-muted-foreground text-sm">
            {topo.device_count} devices · {topo.link_count} links
            {topo.generation > 0 && (
              <span className="ml-2 text-xs text-muted-foreground/60">gen {topo.generation}</span>
            )}
          </p>
        </div>
        <button
          onClick={fetchTopology}
          className="px-3 py-1.5 bg-secondary text-foreground rounded-lg hover:bg-secondary/80 text-sm flex items-center gap-1.5"
        >
          <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24"
            fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
            <path d="M21 12a9 9 0 0 0-9-9 9.75 9.75 0 0 0-6.74 2.74L3 8" />
            <path d="M3 3v5h5" />
            <path d="M3 12a9 9 0 0 0 9 9 9.75 9.75 0 0 0 6.74-2.74L21 16" />
            <path d="M16 16h5v5" />
          </svg>
          Refresh
        </button>
      </div>

      <div className="flex gap-4 flex-1 min-h-0">
        {/* SVG canvas */}
        <div className="flex-1 bg-card border rounded-xl overflow-hidden relative">
          <svg
            ref={svgRef}
            viewBox={`0 0 ${CANVAS_W} ${CANVAS_H}`}
            className="w-full h-full select-none"
            style={{ cursor: dragging ? 'grabbing' : 'default' }}
            onMouseMove={onSvgMouseMove}
            onMouseUp={onSvgMouseUp}
            onMouseLeave={onSvgMouseUp}
          >
            {/* Link edges */}
            {topo.links.map(lnk => {
              const src = positions.get(lnk.source_device)
              const dst = positions.get(lnk.sink_device)
              if (!src || !dst) return null
              const mx = (src.x + dst.x) / 2
              const my = (src.y + dst.y) / 2
              const color = linkColor(lnk.status)
              const isSelected = selectedLink?.id === lnk.id
              const latency = lnk.live_metrics?.latency_ms
              const loss = lnk.live_metrics?.loss_rate

              return (
                <g key={lnk.id} onClick={() => {
                  setSelectedLink(lnk)
                  setSelectedDevice(null)
                }}>
                  <line
                    x1={src.x} y1={src.y}
                    x2={dst.x} y2={dst.y}
                    stroke={color}
                    strokeWidth={isSelected ? 3 : 1.5}
                    strokeDasharray={lnk.status === 'standby' ? '6 3' : undefined}
                    opacity={lnk.status === 'failed' ? 0.5 : 1}
                    className="cursor-pointer"
                  />
                  {/* Invisible wider hit area */}
                  <line
                    x1={src.x} y1={src.y}
                    x2={dst.x} y2={dst.y}
                    stroke="transparent"
                    strokeWidth={12}
                    className="cursor-pointer"
                  />
                  {/* Metric badge */}
                  {(latency !== undefined || loss !== undefined) && (
                    <g transform={`translate(${mx},${my})`}>
                      <rect x={-28} y={-10} width={56} height={20} rx={4}
                        fill="var(--background, #1e1e2e)" opacity={0.85} />
                      <text textAnchor="middle" dominantBaseline="middle"
                        fontSize={9} fill={color} fontFamily="monospace">
                        {latency !== undefined ? `${latency.toFixed(1)}ms` : ''}
                        {latency !== undefined && loss !== undefined ? ' ' : ''}
                        {loss !== undefined ? `${(loss * 100).toFixed(1)}%` : ''}
                      </text>
                    </g>
                  )}
                  {/* Transport label */}
                  {!latency && !loss && (
                    <text x={mx} y={my - 6} textAnchor="middle"
                      fontSize={9} fill={color} opacity={0.7} fontFamily="sans-serif">
                      {lnk.transport}
                    </text>
                  )}
                </g>
              )
            })}

            {/* Device nodes */}
            {topo.devices.map(dev => {
              const pos = positions.get(dev.id)
              if (!pos) return null
              const color = deviceColor(dev.type)
              const isSelected = selectedDevice?.id === dev.id
              const activePorts = dev.ports.filter(p => p.active).length

              return (
                <g
                  key={dev.id}
                  transform={`translate(${pos.x},${pos.y})`}
                  className="cursor-grab"
                  onMouseDown={e => onNodeMouseDown(e, dev.id)}
                  onClick={e => {
                    e.stopPropagation()
                    setSelectedDevice(dev)
                    setSelectedLink(null)
                  }}
                >
                  {/* Selection ring */}
                  {isSelected && (
                    <circle r={NODE_R + 5} fill="none" stroke={color} strokeWidth={2} opacity={0.5} />
                  )}
                  {/* Node circle */}
                  <circle
                    r={NODE_R}
                    fill={color}
                    opacity={0.15}
                    stroke={color}
                    strokeWidth={isSelected ? 2.5 : 1.5}
                  />
                  {/* Active port indicator */}
                  {activePorts > 0 && (
                    <circle r={5} cx={NODE_R - 4} cy={-(NODE_R - 4)}
                      fill="#22c55e" stroke="var(--background, #1e1e2e)" strokeWidth={1.5} />
                  )}
                  {/* Device type initial */}
                  <text textAnchor="middle" dominantBaseline="middle"
                    fontSize={11} fontWeight="600" fill={color} fontFamily="sans-serif">
                    {dev.type.slice(0, 2).toUpperCase()}
                  </text>
                  {/* Name label */}
                  <text y={NODE_R + 13} textAnchor="middle"
                    fontSize={10} fill="var(--foreground, #e2e8f0)" fontFamily="sans-serif"
                    style={{ pointerEvents: 'none' }}>
                    {dev.name.length > 14 ? dev.name.slice(0, 13) + '…' : dev.name}
                  </text>
                </g>
              )
            })}
          </svg>

          {/* Legend */}
          <div className="absolute bottom-3 left-3 flex flex-wrap gap-2 text-xs">
            {Object.entries(LINK_STATUS_COLOR).map(([status, color]) => (
              <span key={status} className="flex items-center gap-1 bg-card/80 px-1.5 py-0.5 rounded">
                <span className="w-3 h-0.5 inline-block rounded" style={{ background: color }} />
                {status}
              </span>
            ))}
          </div>
        </div>

        {/* Inspector panel */}
        {(selectedDevice || selectedLink) && (
          <div className="w-72 bg-card border rounded-xl p-4 overflow-y-auto text-sm flex flex-col gap-3">
            <div className="flex justify-between items-start">
              <h3 className="font-semibold text-base">
                {selectedDevice ? 'Device' : 'Link'}
              </h3>
              <button
                onClick={() => { setSelectedDevice(null); setSelectedLink(null) }}
                className="text-muted-foreground hover:text-foreground"
              >
                ✕
              </button>
            </div>

            {selectedDevice && (
              <DeviceInspector device={selectedDevice} />
            )}

            {selectedLink && (
              <LinkInspector link={selectedLink} metrics={linkMetrics} />
            )}
          </div>
        )}
      </div>
    </div>
  )
}

// ── Sub-components ────────────────────────────────────────────────────────────

function DeviceInspector({ device }: { device: TopologyDevice }) {
  return (
    <>
      <div>
        <p className="text-muted-foreground text-xs mb-0.5">Name</p>
        <p className="font-medium">{device.name}</p>
      </div>
      <div>
        <p className="text-muted-foreground text-xs mb-0.5">Type</p>
        <span className="px-2 py-0.5 rounded-full text-xs font-medium"
          style={{ background: deviceColor(device.type) + '33', color: deviceColor(device.type) }}>
          {device.type}
        </span>
      </div>
      <div>
        <p className="text-muted-foreground text-xs mb-0.5">ID</p>
        <p className="font-mono text-xs break-all">{device.id}</p>
      </div>
      <div>
        <p className="text-muted-foreground text-xs mb-0.5">Assurance level</p>
        <p>{device.assurance_level}</p>
      </div>
      {device.ports.length > 0 && (
        <div>
          <p className="text-muted-foreground text-xs mb-1">Ports ({device.ports.length})</p>
          <div className="space-y-1">
            {device.ports.map(p => (
              <div key={p.id} className="flex items-center justify-between bg-secondary/40 rounded px-2 py-1">
                <span className="font-mono text-xs truncate max-w-[120px]">{p.label ?? p.id}</span>
                <div className="flex items-center gap-1.5">
                  <span className="text-xs text-muted-foreground">{p.media_type}</span>
                  <span className={`w-1.5 h-1.5 rounded-full ${p.active ? 'bg-green-500' : 'bg-muted'}`} />
                </div>
              </div>
            ))}
          </div>
        </div>
      )}
    </>
  )
}

function LinkInspector({ link, metrics }: { link: TopologyLink; metrics: LinkMetrics | null }) {
  const color = linkColor(link.status)
  return (
    <>
      <div>
        <p className="text-muted-foreground text-xs mb-0.5">Status</p>
        <span className="px-2 py-0.5 rounded-full text-xs font-medium"
          style={{ background: color + '33', color }}>
          {link.status}
        </span>
      </div>
      <div>
        <p className="text-muted-foreground text-xs mb-0.5">Transport</p>
        <p className="font-mono text-xs">{link.transport}</p>
      </div>
      <div>
        <p className="text-muted-foreground text-xs mb-0.5">Direction</p>
        <p>{link.bidirectional ? 'Bidirectional' : 'Unidirectional'}</p>
      </div>
      <div>
        <p className="text-muted-foreground text-xs mb-0.5">Source → Sink</p>
        <p className="font-mono text-xs break-all">{link.source_device}</p>
        <p className="text-muted-foreground text-xs">↓</p>
        <p className="font-mono text-xs break-all">{link.sink_device}</p>
      </div>

      {/* Live metrics */}
      {metrics && Object.keys(metrics.live_metrics).length > 0 && (
        <div>
          <p className="text-muted-foreground text-xs mb-1">Live metrics</p>
          <div className="space-y-1">
            {Object.entries(metrics.live_metrics).map(([k, v]) => (
              <div key={k} className="flex justify-between bg-secondary/40 rounded px-2 py-1">
                <span className="text-xs text-muted-foreground">{k}</span>
                <span className="text-xs font-mono">{typeof v === 'number' ? v.toFixed(3) : v}</span>
              </div>
            ))}
          </div>
        </div>
      )}

      {/* Sparklines */}
      {metrics && Object.keys(metrics.sparklines).length > 0 && (
        <div>
          <p className="text-muted-foreground text-xs mb-1">History (1 h)</p>
          <div className="space-y-2">
            {Object.entries(metrics.sparklines).map(([key, pts]) => (
              <div key={key}>
                <p className="text-xs text-muted-foreground mb-0.5">{key}</p>
                <Sparkline points={pts} color={color} width={220} height={32} />
              </div>
            ))}
          </div>
        </div>
      )}

      {metrics && Object.keys(metrics.live_metrics).length === 0 &&
        Object.keys(metrics.sparklines).length === 0 && (
        <p className="text-xs text-muted-foreground italic">
          No metrics available yet — probing may not be configured for this link.
        </p>
      )}
    </>
  )
}
