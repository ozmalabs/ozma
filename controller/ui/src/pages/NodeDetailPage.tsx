import { useEffect, useState } from 'react'
import { useParams, useNavigate, Link } from 'react-router-dom'
import Layout from '../layouts/Layout'
import { useSelectedNode, useActiveNode, useNodeActivation } from '../store/useNodesStore'

// SVG Icons
const Icons = {
  Keyboard: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect width="20" height="16" x="2" y="3" rx="2" ry="2" />
      <line x1="6" x2="6" y1="8" y2="8" />
      <line x1="10" x2="10" y1="8" y2="8" />
      <line x1="14" x2="14" y1="8" y2="8" />
      <line x1="18" x2="18" y1="8" y2="8" />
      <line x1="6" x2="6" y1="12" y2="12" />
      <line x1="10" x2="10" y1="12" y2="12" />
      <line x1="14" x2="14" y1="12" y2="12" />
      <line x1="18" x2="18" y1="12" y2="12" />
    </svg>
  ),
  Mouse: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect width="8" height="14" x="8" y="2" rx="4" ry="2" />
      <path d="M12 16v6" />
      <path d="M8 18h8" />
    </svg>
  ),
  Monitor: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect width="20" height="14" x="2" y="3" rx="2" ry="2" />
      <line x1="8" x2="16" y1="21" y2="21" />
      <line x1="12" x2="12" y1="17" y2="21" />
    </svg>
  ),
  Activity: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M22 12h-4l-3 9L9 3l-3 9H2" />
    </svg>
  ),
  Play: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <polygon points="5 3 19 12 5 21 5 3" />
    </svg>
  ),
  Pause: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect width="4" height="16" x="6" y="4" />
      <rect width="4" height="16" x="14" y="4" />
    </svg>
  ),
  Refresh: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" />
      <path d="M21 3v5h-5" />
      <path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" />
      <path d="M8 16H3v5" />
    </svg>
  ),
  Check: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <polyline points="20 6 9 17 4 12" />
    </svg>
  ),
  X: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M18 6 6 18" />
      <path d="m6 6 12 12" />
    </svg>
  ),
  ArrowLeft: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="m12 19-7-7 7-7" />
      <path d="M19 12H5" />
    </svg>
  ),
  Server: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <rect width="20" height="8" x="2" y="2" rx="2" ry="2" />
      <rect width="20" height="8" x="2" y="14" rx="2" ry="2" />
      <line x1="6" x2="6.01" y1="6" y2="6" />
      <line x1="6" x2="6.01" y1="18" y2="18" />
    </svg>
  ),
  Wifi: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <path d="M5 12.55a11 11 0 0 1 14.08 0" />
      <path d="M1.42 9a16 16 0 0 1 21.16 0" />
      <path d="M8.53 16.11a6 6 0 0 1 6.95 0" />
      <line x1="12" x2="12.01" y1="20" y2="20" />
    </svg>
  ),
  Clock: () => (
    <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="10" />
      <polyline points="12 6 12 12 16 14" />
    </svg>
  ),
}

// Helper to format uptime
function formatUptime(seconds: number): string {
  if (seconds < 60) return `${Math.round(seconds)}s`
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ${Math.round((seconds % 3600) / 60)}m`
  return `${Math.round(seconds / 86400)}d ${Math.round((seconds % 86400) / 3600)}h`
}

// Helper to format datetime
function formatDate(dateString: string): string {
  const date = new Date(dateString)
  return new Intl.DateTimeFormat('en-US', {
    dateStyle: 'medium',
    timeStyle: 'medium',
  }).format(date)
}

// Status badge component
function StatusBadge({ status }: { status: string }) {
  const colors = {
    online: 'bg-emerald-500',
    offline: 'bg-rose-500',
    connecting: 'bg-amber-500',
  }
  const color = colors[status as keyof typeof colors] || 'bg-slate-500'

  return (
    <span className="flex items-center gap-2">
      <span className={`w-2 h-2 rounded-full ${color} animate-pulse`} />
      <span className="capitalize">{status}</span>
    </span>
  )
}

// Node info section
function NodeInfoSection({ node }: { node: any }) {
  return (
    <div className="space-y-4">
      <div className="flex items-center gap-4 mb-4">
        <div className="w-16 h-16 rounded-xl bg-primary/10 flex items-center justify-center text-primary">
          <Icons.Server />
        </div>
        <div>
          <h2 className="text-2xl font-bold text-foreground">{node.name || node.id}</h2>
          <div className="flex items-center gap-3 mt-1">
            <span className="text-sm text-muted-foreground">ID: {node.id}</span>
            <span className="text-sm text-muted-foreground">•</span>
            <StatusBadge status={node.status || (node.last_seen ? 'online' : 'offline')} />
          </div>
        </div>
      </div>

      <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
        <div className="p-4 rounded-xl bg-card border">
          <div className="flex items-center gap-2 text-muted-foreground mb-2">
            <Icons.Wifi />
            <span className="text-sm font-medium">IP Address</span>
          </div>
          <div className="text-lg font-semibold">{node.host}</div>
          <div className="text-xs text-muted-foreground">Port {node.port}</div>
        </div>

        <div className="p-4 rounded-xl bg-card border">
          <div className="flex items-center gap-2 text-muted-foreground mb-2">
            <Icons.Refresh />
            <span className="text-sm font-medium">Uptime</span>
          </div>
          <div className="text-lg font-semibold">
            {node.uptime_seconds ? formatUptime(node.uptime_seconds) : 'N/A'}
          </div>
          <div className="text-xs text-muted-foreground">Last seen: {node.last_seen ? formatDate(node.last_seen) : 'Unknown'}</div>
        </div>

        <div className="p-4 rounded-xl bg-card border">
          <div className="flex items-center gap-2 text-muted-foreground mb-2">
            <Icons.Activity />
            <span className="text-sm font-medium">Machine Class</span>
          </div>
          <div className="text-lg font-semibold capitalize">{node.machine_class}</div>
          <div className="text-xs text-muted-foreground">Role: {node.role}</div>
        </div>

        <div className="p-4 rounded-xl bg-card border">
          <div className="flex items-center gap-2 text-muted-foreground mb-2">
            <Icons.Server />
            <span className="text-sm font-medium">Hardware</span>
          </div>
          <div className="text-lg font-semibold">{node.hw}</div>
          <div className="text-xs text-muted-foreground">FW: {node.fw_version}</div>
        </div>
      </div>
    </div>
  )
}

// KVM Focus Control section
function KVMFocusControl({ node, isActive, onActivate }: { node: any; isActive: boolean; onActivate: () => void }) {
  const [isActivating, setIsActivating] = useState(false)

  const handleActivate = async () => {
    setIsActivating(true)
    try {
      await onActivate()
    } finally {
      setIsActivating(false)
    }
  }

  return (
    <div className="p-6 rounded-xl bg-card border">
      <div className="flex items-center justify-between mb-4">
        <h3 className="text-lg font-semibold flex items-center gap-2">
          <Icons.Keyboard />
          KVM Focus Control
        </h3>
        {isActive && (
          <span className="px-3 py-1 bg-emerald-500/10 text-emerald-500 rounded-full text-sm font-medium">
            Active Node
          </span>
        )}
      </div>

      <div className="flex items-center justify-between py-4 border-t">
        <div>
          <p className="text-sm text-muted-foreground">
            {isActive ? 'This node currently receives all HID input' : 'Click to route all input to this node'}
          </p>
          <p className="text-xs text-muted-foreground mt-1">
            Switching will immediately redirect keyboard, mouse, and audio to this machine
          </p>
        </div>
        <button
          onClick={handleActivate}
          disabled={isActivating || isActive}
          className={`flex items-center gap-2 px-6 py-3 rounded-lg font-medium transition-all ${
            isActive
              ? 'bg-emerald-500/10 text-emerald-500 cursor-default'
              : isActivating
              ? 'bg-slate-500/20 text-slate-400 cursor-wait'
              : 'bg-primary hover:bg-primary/90 text-primary-foreground shadow-lg shadow-primary/20'
          }`}
        >
          {isActivating ? (
            <>
              <Icons.Refresh className="animate-spin" />
              Switching...
            </>
          ) : isActive ? (
            <>
              <Icons.Check />
              Focus Active
            </>
          ) : (
            <>
              <Icons.Play />
              Activate Node
            </>
          )}
        </button>
      </div>
    </div>
  )
}

// Stream Preview section
function StreamPreview({ node }: { node: any }) {
  const streamUrl = node.stream_port && node.stream_path
    ? `http://${node.host}:${node.stream_port}${node.stream_path}`
    : null

  const vncUrl = node.vnc_host && node.vnc_port
    ? `http://${node.vnc_host}:${node.vnc_port}/vnc.html`
    : null

  return (
    <div className="p-6 rounded-xl bg-card border">
      <h3 className="text-lg font-semibold flex items-center gap-2 mb-4">
        <Icons.Monitor />
        Stream Preview
      </h3>

      <div className="space-y-4">
        {streamUrl ? (
          <div className="aspect-video bg-black rounded-lg overflow-hidden relative group">
            <iframe
              src={streamUrl}
              className="w-full h-full"
              title="Node Stream"
              sandbox="allow-same-origin allow-scripts allow-forms allow-presentation"
              loading="lazy"
            />
            <div className="absolute top-2 right-2 bg-black/50 text-white px-2 py-1 rounded text-xs backdrop-blur-sm">
              HLS Stream
            </div>
          </div>
        ) : vncUrl ? (
          <div className="aspect-video bg-black rounded-lg overflow-hidden relative">
            <a
              href={vncUrl}
              target="_blank"
              rel="noopener noreferrer"
              className="w-full h-full flex items-center justify-center bg-slate-800 hover:bg-slate-700 transition-colors"
            >
              <div className="text-center">
                <Icons.Monitor className="w-12 h-12 text-slate-500 mx-auto mb-2" />
                <p className="text-slate-400">VNC Preview</p>
                <p className="text-sm text-slate-500 mt-1">Click to open</p>
              </div>
            </a>
          </div>
        ) : (
          <div className="aspect-video bg-slate-900/50 rounded-lg flex items-center justify-center border-2 border-dashed border-slate-700/50">
            <div className="text-center">
              <Icons.Monitor className="w-12 h-12 text-slate-600 mx-auto mb-2" />
              <p className="text-slate-500">No stream available</p>
            </div>
          </div>
        )}

        <div className="grid grid-cols-2 gap-4">
          <div className="p-3 rounded-lg bg-slate-900/30 border border-slate-800">
            <span className="text-xs text-muted-foreground block mb-1">Stream Port</span>
            <span className="text-sm font-mono">{streamUrl ? `:${node.stream_port}` : 'N/A'}</span>
          </div>
          <div className="p-3 rounded-lg bg-slate-900/30 border border-slate-800">
            <span className="text-xs text-muted-foreground block mb-1">VNC Port</span>
            <span className="text-sm font-mono">{vncUrl ? `:${node.vnc_port}` : 'N/A'}</span>
          </div>
        </div>
      </div>
    </div>
  )
}

// HID Stats section
function HIDStatsSection({ stats }: { stats?: any }) {
  if (!stats) {
    return (
      <div className="p-6 rounded-xl bg-card border">
        <h3 className="text-lg font-semibold flex items-center gap-2 mb-4">
          <Icons.Keyboard />
          HID Statistics
        </h3>
        <div className="text-center py-8 text-slate-500">
          <Icons.Keyboard className="w-12 h-12 mx-auto mb-2 opacity-50" />
          <p>HID statistics not available</p>
        </div>
      </div>
    )
  }

  return (
    <div className="p-6 rounded-xl bg-card border">
      <h3 className="text-lg font-semibold flex items-center gap-2 mb-4">
        <Icons.Keyboard />
        HID Statistics
      </h3>

      <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
        <StatCard label="Total Keys" value={stats.total_keys} icon={<Icons.Keyboard />} color="bg-blue-500" />
        <StatCard label="Total Clicks" value={stats.total_clicks} icon={<Icons.Mouse />} color="bg-purple-500" />
        <StatCard label="Total Scrolls" value={stats.total_scrolls} icon={<Icons.Activity />} color="bg-pink-500" />
        <StatCard
          label="Last Activity"
          value={stats.last_activity ? formatDate(stats.last_activity) : 'N/A'}
          icon={<Icons.Clock />}
          color="bg-cyan-500"
          isTime
        />
      </div>
    </div>
  )
}

// Individual stat card
function StatCard({ label, value, icon, color, isTime = false }: { label: string; value: string | number; icon: React.ReactNode; color: string; isTime?: boolean }) {
  return (
    <div className="p-4 rounded-lg bg-slate-900/30 border border-slate-800">
      <div className="flex items-center gap-3 mb-2">
        <div className={`p-2 rounded-lg ${color} text-white`}>{icon}</div>
        <span className="text-xs text-muted-foreground uppercase tracking-wide">{label}</span>
      </div>
      <div className="text-2xl font-bold text-foreground">{isTime ? value : value.toLocaleString()}</div>
    </div>
  )
}

// Current Scenario section
function CurrentScenario({ scenario }: { scenario?: any }) {
  if (!scenario) {
    return (
      <div className="p-6 rounded-xl bg-card border">
        <h3 className="text-lg font-semibold mb-4">Current Scenario</h3>
        <div className="text-center py-8 text-slate-500">
          <p>No scenario currently assigned</p>
        </div>
      </div>
    )
  }

  return (
    <div className="p-6 rounded-xl bg-card border">
      <h3 className="text-lg font-semibold mb-4">Current Scenario</h3>

      <div className={`p-6 rounded-lg ${scenario.color ? `bg-gradient-to-br ${scenario.color.replace('#', '')}10` : 'bg-slate-900/30'} border`}>
        <div className="flex items-center gap-4">
          <div className={`w-16 h-16 rounded-lg flex items-center justify-center ${scenario.color ? `bg-${scenario.color}` : 'bg-primary'}`}>
            {scenario.name ? (
              <span className="text-3xl font-bold text-white">{scenario.name.charAt(0).toUpperCase()}</span>
            ) : (
              <Icons.Activity className="text-white w-8 h-8" />
            )}
          </div>
          <div>
            <h4 className="text-xl font-bold">{scenario.name}</h4>
            <div className="flex flex-wrap gap-2 mt-2">
              <span className="px-2 py-1 bg-slate-900/20 rounded text-sm">ID: {scenario.id}</span>
              <span className="px-2 py-1 bg-slate-900/20 rounded text-sm">Scenario</span>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}

// Node details view
function NodeDetails({ node, isActive, onActivate }: { node: any; isActive: boolean; onActivate: () => void }) {
  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-4">
          <Link to="/nodes" className="p-2 hover:bg-slate-800 rounded-lg transition-colors">
            <Icons.ArrowLeft />
          </Link>
          <div>
            <h1 className="text-3xl font-bold">Node Details</h1>
            <p className="text-muted-foreground">Manage {node.name || node.id}</p>
          </div>
        </div>
      </div>

      {/* Quick Actions */}
      <div className="flex gap-4">
        <button className="flex-1 px-4 py-3 bg-card hover:bg-secondary rounded-lg border flex items-center justify-center gap-2 transition-colors">
          <Icons.Server />
          Remote Desktop
        </button>
        <button className="flex-1 px-4 py-3 bg-card hover:bg-secondary rounded-lg border flex items-center justify-center gap-2 transition-colors">
          <Icons.Play />
          Start Recording
        </button>
        <button className="flex-1 px-4 py-3 bg-card hover:bg-secondary rounded-lg border flex items-center justify-center gap-2 transition-colors">
          <Icons.Activity />
          View Logs
        </button>
      </div>

      {/* Node Info */}
      <NodeInfoSection node={node} />

      {/* KVM Focus Control */}
      <KVMFocusControl node={node} isActive={isActive} onActivate={onActivate} />

      {/* Stream Preview */}
      <StreamPreview node={node} />

      {/* HID Stats */}
      <HIDStatsSection stats={node.hid_stats} />

      {/* Current Scenario */}
      <CurrentScenario scenario={node.scenario} />

      {/* Additional Info Grid */}
      <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
        {/* Audio Configuration */}
        <div className="p-6 rounded-xl bg-card border">
          <h3 className="text-lg font-semibold mb-4">Audio Configuration</h3>
          <div className="space-y-3 text-sm">
            <div className="flex justify-between">
              <span className="text-muted-foreground">Audio Type</span>
              <span className="font-medium capitalize">{node.audio_type || 'None'}</span>
            </div>
            <div className="flex justify-between">
              <span className="text-muted-foreground">Audio Sink</span>
              <span className="font-mono">{node.audio_sink || 'N/A'}</span>
            </div>
            {node.audio_vban_port && (
              <div className="flex justify-between">
                <span className="text-muted-foreground">VBAN Port</span>
                <span className="font-mono">{node.audio_vban_port}</span>
              </div>
            )}
            {node.mic_vban_port && (
              <div className="flex justify-between">
                <span className="text-muted-foreground">Mic VBAN</span>
                <span className="font-mono">{node.mic_vban_port}</span>
              </div>
            )}
          </div>
        </div>

        {/* Display Outputs */}
        <div className="p-6 rounded-xl bg-card border">
          <h3 className="text-lg font-semibold mb-4">Display Outputs</h3>
          {node.display_outputs && node.display_outputs.length > 0 ? (
            <div className="space-y-3">
              {node.display_outputs.map((output: any, idx: number) => (
                <div key={idx} className="p-3 rounded-lg bg-slate-900/30 border border-slate-800">
                  <div className="flex justify-between text-sm">
                    <span className="text-muted-foreground">Output {idx}</span>
                    <span className="font-medium">{output.width}×{output.height}</span>
                  </div>
                  <div className="text-xs text-muted-foreground mt-1">
                    Source: {output.source_type} ({output.capture_source_id})
                  </div>
                </div>
              ))}
            </div>
          ) : (
            <div className="text-center py-4 text-slate-500 text-sm">
              No display outputs configured
            </div>
          )}
        </div>
      </div>

      {/* Camera Streams */}
      {node.camera_streams && node.camera_streams.length > 0 && (
        <div className="p-6 rounded-xl bg-card border">
          <h3 className="text-lg font-semibold mb-4">Camera Streams</h3>
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            {node.camera_streams.map((stream: any, idx: number) => (
              <div key={idx} className="p-4 rounded-lg bg-slate-900/30 border border-slate-800">
                <div className="flex items-center justify-between mb-2">
                  <span className="font-medium">{stream.name}</span>
                  <span className="text-xs bg-emerald-500/10 text-emerald-500 px-2 py-1 rounded">RTSP</span>
                </div>
                <div className="text-xs font-mono text-muted-foreground break-all mb-2">{stream.rtsp_inbound}</div>
                <a href={stream.hls} target="_blank" rel="noopener noreferrer" className="text-primary text-sm hover:underline">
                  View Stream →
                </a>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  )
}

// Loading and Error States
function NodeDetailLoading() {
  return (
    <div className="flex items-center justify-center h-full">
      <div className="text-center">
        <div className="w-12 h-12 border-4 border-primary border-t-transparent rounded-full animate-spin mx-auto mb-4"></div>
        <p className="text-muted-foreground">Loading node details...</p>
      </div>
    </div>
  )
}

function NodeDetailError({ message }: { message: string }) {
  return (
    <div className="flex items-center justify-center h-full">
      <div className="text-center max-w-md">
        <div className="text-destructive mb-4">
          <Icons.X className="w-16 h-16 mx-auto" />
        </div>
        <h3 className="text-xl font-semibold mb-2">Failed to load node</h3>
        <p className="text-muted-foreground mb-6">{message}</p>
        <button
          onClick={() => window.location.reload()}
          className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors"
        >
          Retry
        </button>
      </div>
    </div>
  )
}

export default function NodeDetailPage() {
  const { id } = useParams<{ id: string }>()
  const navigate = useNavigate()
  const { selectedNode, fetchNodeById, selectNode } = useSelectedNode()
  const { activeNode } = useActiveNode()
  const { activateNode } = useNodeActivation()

  useEffect(() => {
    if (id) {
      selectNode(id)
      fetchNodeById(id)
    }
  }, [id, selectNode, fetchNodeById])

  if (!id) {
    return (
      <Layout>
        <NodeDetailError message="No node ID provided" />
      </Layout>
    )
  }

  if (!selectedNode) {
    return (
      <Layout>
        <NodeDetailLoading />
      </Layout>
    )
  }

  const isActive = activeNode?.id === id

  const handleActivate = async () => {
    try {
      await activateNode(id)
    } catch (error) {
      console.error('Failed to activate node:', error)
    }
  }

  return (
    <Layout>
      <NodeDetails node={selectedNode} isActive={isActive} onActivate={handleActivate} />
    </Layout>
  )
}
