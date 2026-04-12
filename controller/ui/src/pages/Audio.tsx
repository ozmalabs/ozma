import { useEffect, useState, useCallback } from 'react'

const API = '/api/v1'

interface PipeWireNode {
  id: number
  name: string
  nick?: string
  media_class?: string
  volume: number
  mute: boolean
}

interface AudioLink {
  from: string | null
  to: string | null
  link_id: number | null
}

interface VBANNodeInfo {
  node_id: string
  host: string
  port: number | null
  enabled: boolean
}

interface NodeInfo {
  id: string
  host: string
  audio_vban_port?: number | null
}

function useAudioData() {
  const [pwNodes, setPwNodes] = useState<Record<string, PipeWireNode>>({})
  const [links, setLinks] = useState<AudioLink[]>([])
  const [vbanNodes, setVbanNodes] = useState<VBANNodeInfo[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(async () => {
    try {
      const [nodesRes, routesRes, vbanRes, allNodesRes] = await Promise.all([
        fetch(`${API}/audio/nodes`),
        fetch(`${API}/audio/routes`),
        fetch(`${API}/audio/vban`),
        fetch(`${API}/nodes`),
      ])
      if (!nodesRes.ok || !routesRes.ok || !vbanRes.ok || !allNodesRes.ok) {
        throw new Error('Failed to fetch audio data')
      }
      const nodesData = await nodesRes.json()
      const routesData = await routesRes.json()
      const vbanData = await vbanRes.json()
      const allNodesData = await allNodesRes.json()

      setPwNodes(nodesData.nodes ?? {})
      // /audio/routes returns {routes: [{from, to, link_id}], links: [...]}
      // Map to AudioLink shape expected by RoutingMatrix
      const rawRoutes: Array<{ from: string | null; to: string | null; link_id: number | null }> =
        routesData.routes ?? []
      setLinks(rawRoutes)

      // Build VBAN list from controller nodes
      const nodeList: NodeInfo[] = allNodesData.nodes ?? []
      setVbanNodes(
        nodeList.map((n) => ({
          node_id: n.id,
          host: n.host,
          port: n.audio_vban_port ?? null,
          enabled: !!n.audio_vban_port,
        }))
      )
      setError(null)
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Unknown error')
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    refresh()
    const id = setInterval(refresh, 5000)
    return () => clearInterval(id)
  }, [refresh])

  return { pwNodes, links, vbanNodes, loading, error, refresh }
}

// ── Volume card ────────────────────────────────────────────────────────────

function VolumeCard({
  node,
  onVolumeChange,
  onMuteToggle,
}: {
  node: PipeWireNode
  onVolumeChange: (name: string, vol: number) => void
  onMuteToggle: (name: string, mute: boolean) => void
}) {
  const label = node.nick || node.name
  const isSink = node.media_class?.toLowerCase().includes('sink')
  const isSource = node.media_class?.toLowerCase().includes('source')
  const typeLabel = isSink ? 'Sink' : isSource ? 'Source' : 'Node'
  const typeColor = isSink
    ? 'bg-blue-900/40 text-blue-300'
    : isSource
    ? 'bg-purple-900/40 text-purple-300'
    : 'bg-gray-700 text-gray-300'

  return (
    <div className="bg-oz-surface rounded-xl p-4 flex flex-col gap-3 border border-white/5">
      <div className="flex items-center justify-between gap-2">
        <div className="min-w-0">
          <p className="text-sm font-medium text-oz-text truncate" title={node.name}>
            {label}
          </p>
          <span className={`text-xs px-1.5 py-0.5 rounded ${typeColor}`}>{typeLabel}</span>
        </div>
        <button
          onClick={() => onMuteToggle(node.name, !node.mute)}
          className={`shrink-0 w-8 h-8 rounded-lg flex items-center justify-center transition-colors ${
            node.mute
              ? 'bg-red-600/80 hover:bg-red-600 text-white'
              : 'bg-white/10 hover:bg-white/20 text-oz-text'
          }`}
          title={node.mute ? 'Unmute' : 'Mute'}
        >
          {node.mute ? (
            <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2}
                d="M5.586 15H4a1 1 0 01-1-1v-4a1 1 0 011-1h1.586l4.707-4.707C10.923 3.663 12 4.109 12 5v14c0 .891-1.077 1.337-1.707.707L5.586 15z" />
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2}
                d="M17 14l2-2m0 0l2-2m-2 2l-2-2m2 2l2 2" />
            </svg>
          ) : (
            <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2}
                d="M15.536 8.464a5 5 0 010 7.072M12 6v12m-3.536-9.536a5 5 0 000 7.072M5.586 15H4a1 1 0 01-1-1v-4a1 1 0 011-1h1.586l4.707-4.707C10.923 3.663 12 4.109 12 5v14c0 .891-1.077 1.337-1.707.707L5.586 15z" />
            </svg>
          )}
        </button>
      </div>

      <div className="flex items-center gap-2">
        <input
          type="range"
          min={0}
          max={1.5}
          step={0.01}
          value={node.mute ? 0 : node.volume}
          disabled={node.mute}
          onChange={(e) => onVolumeChange(node.name, parseFloat(e.target.value))}
          className="flex-1 accent-oz-accent disabled:opacity-40"
        />
        <span className="text-xs text-oz-muted w-10 text-right tabular-nums">
          {node.mute ? 'muted' : `${Math.round(node.volume * 100)}%`}
        </span>
      </div>
    </div>
  )
}

// ── VBAN row ───────────────────────────────────────────────────────────────

function VBANRow({
  info,
  onToggle,
  onPortChange,
}: {
  info: VBANNodeInfo
  onToggle: (nodeId: string, enabled: boolean, port: number) => void
  onPortChange: (nodeId: string, port: number) => void
}) {
  const [draftPort, setDraftPort] = useState(String(info.port ?? 6980))

  const shortId = info.node_id.split('.')[0]

  return (
    <tr className="border-b border-white/5 hover:bg-white/5 transition-colors">
      <td className="py-3 px-4 text-sm text-oz-text font-mono" title={info.node_id}>
        {shortId}
      </td>
      <td className="py-3 px-4 text-sm text-oz-muted">{info.host}</td>
      <td className="py-3 px-4">
        <input
          type="number"
          min={1024}
          max={65535}
          value={draftPort}
          onChange={(e) => setDraftPort(e.target.value)}
          onBlur={() => {
            const p = parseInt(draftPort, 10)
            if (!isNaN(p) && p > 1023) onPortChange(info.node_id, p)
          }}
          className="w-24 bg-oz-bg border border-white/10 rounded px-2 py-1 text-sm text-oz-text focus:outline-none focus:border-oz-accent"
        />
      </td>
      <td className="py-3 px-4">
        <button
          onClick={() => onToggle(info.node_id, !info.enabled, parseInt(draftPort, 10) || 6980)}
          className={`px-3 py-1 rounded text-xs font-medium transition-colors ${
            info.enabled
              ? 'bg-green-700/60 hover:bg-green-700 text-green-200'
              : 'bg-white/10 hover:bg-white/20 text-oz-muted'
          }`}
        >
          {info.enabled ? 'Enabled' : 'Disabled'}
        </button>
      </td>
    </tr>
  )
}

// ── Routing matrix ─────────────────────────────────────────────────────────

function RoutingMatrix({ links }: { links: AudioLink[] }) {
  const sources = Array.from(new Set(links.map((l) => l.from).filter(Boolean))) as string[]
  const sinks = Array.from(new Set(links.map((l) => l.to).filter(Boolean))) as string[]

  if (sources.length === 0 && sinks.length === 0) {
    return (
      <p className="text-oz-muted text-sm py-4 text-center">
        No active audio links detected.
      </p>
    )
  }

  const isLinked = (src: string, sink: string) =>
    links.some((l) => l.from === src && l.to === sink)

  return (
    <div className="overflow-x-auto">
      <table className="text-xs border-collapse w-full">
        <thead>
          <tr>
            <th className="text-left py-2 px-3 text-oz-muted font-normal w-40">
              Source ↓ / Sink →
            </th>
            {sinks.map((sink) => (
              <th
                key={sink}
                className="py-2 px-3 text-oz-muted font-normal text-center max-w-[120px]"
              >
                <span className="block truncate" title={sink}>
                  {sink}
                </span>
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {sources.map((src) => (
            <tr key={src} className="border-t border-white/5">
              <td className="py-2 px-3 text-oz-text truncate max-w-[160px]" title={src}>
                {src}
              </td>
              {sinks.map((sink) => (
                <td key={sink} className="py-2 px-3 text-center">
                  {isLinked(src, sink) ? (
                    <span
                      className="inline-block w-4 h-4 rounded-full bg-oz-accent"
                      title={`${src} → ${sink}`}
                    />
                  ) : (
                    <span className="inline-block w-4 h-4 rounded-full bg-white/10" />
                  )}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

// ── Main page ──────────────────────────────────────────────────────────────

export default function Audio() {
  const { pwNodes, links, vbanNodes, loading, error, refresh } = useAudioData()
  const [saving, setSaving] = useState<string | null>(null)

  const handleVolumeChange = useCallback(
    async (nodeName: string, volume: number) => {
      setSaving(nodeName)
      try {
        await fetch(`${API}/audio/volume`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ node_name: nodeName, volume }),
        })
        await refresh()
      } finally {
        setSaving(null)
      }
    },
    [refresh]
  )

  const handleMuteToggle = useCallback(async (nodeName: string, mute: boolean) => {
    setSaving(nodeName)
    try {
      await fetch(`${API}/audio/mute`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ node_name: nodeName, mute }),
      })
      await refresh()
    } finally {
      setSaving(null)
    }
  }, [refresh])

  const handleVBANToggle = useCallback(
    async (nodeId: string, enabled: boolean, port: number) => {
      await fetch(`${API}/audio/vban`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ node_id: nodeId, port, enabled }),
      })
      await refresh()
    },
    [refresh]
  )

  const handleVBANPortChange = useCallback(
    async (nodeId: string, port: number) => {
      const node = vbanNodes.find((n) => n.node_id === nodeId)
      if (!node?.enabled) return
      await fetch(`${API}/audio/vban`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ node_id: nodeId, port, enabled: true }),
      })
      await refresh()
    },
    [vbanNodes, refresh]
  )

  const nodeList = Object.values(pwNodes)
  const sinks = nodeList.filter((n) => n.media_class?.toLowerCase().includes('sink'))
  const sources = nodeList.filter((n) => n.media_class?.toLowerCase().includes('source'))
  const others = nodeList.filter(
    (n) =>
      !n.media_class?.toLowerCase().includes('sink') &&
      !n.media_class?.toLowerCase().includes('source')
  )

  return (
    <div className="p-6 space-y-8 max-w-6xl">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-semibold text-oz-text">Audio Routing</h1>
          <p className="text-oz-muted text-sm mt-1">
            PipeWire nodes, VBAN streams, and routing matrix
          </p>
        </div>
        <button
          onClick={refresh}
          className="px-3 py-1.5 rounded-lg bg-white/10 hover:bg-white/20 text-oz-text text-sm transition-colors"
        >
          Refresh
        </button>
      </div>

      {error && (
        <div className="bg-red-900/30 border border-red-700/50 rounded-lg px-4 py-3 text-red-300 text-sm">
          {error}
        </div>
      )}

      {loading ? (
        <div className="text-oz-muted text-sm animate-pulse">Loading audio data…</div>
      ) : (
        <>
          {/* ── Volume controls ── */}
          <section>
            <h2 className="text-lg font-medium text-oz-text mb-4">Volume Controls</h2>

            {nodeList.length === 0 ? (
              <p className="text-oz-muted text-sm">
                No PipeWire nodes found. Is PipeWire running on the controller?
              </p>
            ) : (
              <div className="space-y-6">
                {sinks.length > 0 && (
                  <div>
                    <p className="text-xs text-oz-muted uppercase tracking-wider mb-3">
                      Sinks (outputs)
                    </p>
                    <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-3">
                      {sinks.map((n) => (
                        <VolumeCard
                          key={n.id}
                          node={n}
                          onVolumeChange={handleVolumeChange}
                          onMuteToggle={handleMuteToggle}
                        />
                      ))}
                    </div>
                  </div>
                )}

                {sources.length > 0 && (
                  <div>
                    <p className="text-xs text-oz-muted uppercase tracking-wider mb-3">
                      Sources (inputs)
                    </p>
                    <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-3">
                      {sources.map((n) => (
                        <VolumeCard
                          key={n.id}
                          node={n}
                          onVolumeChange={handleVolumeChange}
                          onMuteToggle={handleMuteToggle}
                        />
                      ))}
                    </div>
                  </div>
                )}

                {others.length > 0 && (
                  <div>
                    <p className="text-xs text-oz-muted uppercase tracking-wider mb-3">
                      Other nodes
                    </p>
                    <div className="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-3">
                      {others.map((n) => (
                        <VolumeCard
                          key={n.id}
                          node={n}
                          onVolumeChange={handleVolumeChange}
                          onMuteToggle={handleMuteToggle}
                        />
                      ))}
                    </div>
                  </div>
                )}
              </div>
            )}
          </section>

          {/* ── VBAN streams ── */}
          <section>
            <h2 className="text-lg font-medium text-oz-text mb-1">VBAN Streams</h2>
            <p className="text-oz-muted text-sm mb-4">
              Enable VBAN audio streaming per node. Set the UDP port and toggle to activate.
            </p>

            {vbanNodes.length === 0 ? (
              <p className="text-oz-muted text-sm">No nodes registered.</p>
            ) : (
              <div className="bg-oz-surface rounded-xl border border-white/5 overflow-hidden">
                <table className="w-full">
                  <thead>
                    <tr className="border-b border-white/10 text-left">
                      <th className="py-3 px-4 text-xs text-oz-muted font-medium uppercase tracking-wider">
                        Node
                      </th>
                      <th className="py-3 px-4 text-xs text-oz-muted font-medium uppercase tracking-wider">
                        Host
                      </th>
                      <th className="py-3 px-4 text-xs text-oz-muted font-medium uppercase tracking-wider">
                        UDP Port
                      </th>
                      <th className="py-3 px-4 text-xs text-oz-muted font-medium uppercase tracking-wider">
                        Status
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {vbanNodes.map((info) => (
                      <VBANRow
                        key={info.node_id}
                        info={info}
                        onToggle={handleVBANToggle}
                        onPortChange={handleVBANPortChange}
                      />
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </section>

          {/* ── Routing matrix ── */}
          <section>
            <h2 className="text-lg font-medium text-oz-text mb-1">Routing Matrix</h2>
            <p className="text-oz-muted text-sm mb-4">
              Active PipeWire links between sources and sinks. Filled dot = connected.
            </p>
            <div className="bg-oz-surface rounded-xl border border-white/5 p-4">
              <RoutingMatrix links={links} />
            </div>
          </section>
        </>
      )}

      {saving && (
        <div className="fixed bottom-4 right-4 bg-oz-surface border border-white/10 rounded-lg px-4 py-2 text-sm text-oz-muted shadow-lg">
          Saving…
        </div>
      )}
    </div>
  )
}
