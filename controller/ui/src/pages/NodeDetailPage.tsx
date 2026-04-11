import { useEffect, useState } from 'react'
import { useParams, useNavigate } from 'react-router-dom'
import Layout from '../layouts/Layout'
import { useQuery, useMutation, useSubscription } from 'urql'
import { GET_NODE_BY_ID, ACTIVATE_NODE, SUBSCRIBE_NODE_STATE } from '../graphql/queries'
import { useNodesStore } from '../store/useNodesStore'
import { formatUptime } from '../utils/time'
import { Button } from '../components/ui/Button'

export default function NodeDetailPage() {
  const { id } = useParams<{ id: string }>()
  const navigate = useNavigate()
  const [node, setNode] = useState<any>(null)
  const [loading, setLoading] = useState(true)
  const [activeNode, setActiveNode] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [streamPort, setStreamPort] = useState<number | null>(null)

  // GraphQL query for node details
  const [{ data: nodeData, error: nodeError, fetching: nodeLoading }] = useQuery({
    query: GET_NODE_BY_ID,
    variables: { id: id || '' },
    pause: !id,
  })

  // GraphQL mutation for activating node
  const [, activateNode] = useMutation(ACTIVATE_NODE)

  // GraphQL subscription for real-time updates
  const [, subscriptionResult] = useSubscription({
    query: SUBSCRIBE_NODE_STATE,
    variables: { id: id || '' },
  })

  // Zustand store for WebSocket events
  const { nodes, updateNode } = useNodesStore()

  useEffect(() => {
    if (nodeError) {
      setError(nodeError.message)
      setLoading(false)
    }
  }, [nodeError])

  useEffect(() => {
    if (nodeData?.node) {
      setNode(nodeData.node)
      setLoading(false)
      
      // Set stream port for preview thumbnail
      if (nodeData.node.stream_port) {
        setStreamPort(nodeData.node.stream_port)
      }
    }
  }, [nodeData])

  useEffect(() => {
    if (subscriptionResult.data?.nodeStateChanged) {
      const updatedNode = subscriptionResult.data.nodeStateChanged
      if (updatedNode.id === id) {
        setNode(updatedNode)
        updateNode({
          id: updatedNode.id,
          name: updatedNode.name || '',
          hostname: updatedNode.host || '',
          machine_class: updatedNode.machine_class as any,
          status: updatedNode.active ? 'online' : 'offline',
          active: updatedNode.active || false,
          last_seen: new Date().toISOString(),
          ip_address: updatedNode.host || '',
          mac_address: null,
          uptime_seconds: updatedNode.uptime_seconds || 0,
          hid_stats: updatedNode.hid_stats || null,
          scenario: updatedNode.scenario || null,
        })
      }
    }
  }, [subscriptionResult.data, id, updateNode])

  useEffect(() => {
    // Check Zustand store for node updates
    const storeNode = nodes.find((n) => n.id === id)
    if (storeNode && !node) {
      setNode({
        name: storeNode.name,
        host: storeNode.hostname,
        machine_class: storeNode.machine_class,
        active: storeNode.active,
        last_seen: storeNode.last_seen,
        ip_address: storeNode.ip_address,
        mac_address: storeNode.mac_address || null,
      })
      setLoading(false)
    }
  }, [nodes, id, node])

  const handleActivate = async () => {
    if (!id) return
    
    try {
      const result = await activateNode({ nodeId: id })
      if (result.data?.activate_node) {
        setActiveNode(id)
      }
    } catch (err) {
      console.error('Failed to activate node:', err)
      setError('Failed to activate node')
    }
  }

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'online':
        return 'text-emerald-500'
      case 'offline':
        return 'text-destructive'
      case 'connecting':
        return 'text-amber-500'
      default:
        return 'text-muted-foreground'
    }
  }

  if (loading) {
    return (
      <Layout>
        <div className="flex items-center justify-center h-full">
          <div className="text-center">
            <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin mx-auto mb-4"></div>
            <p className="text-muted-foreground">Loading node...</p>
          </div>
        </div>
      </Layout>
    )
  }

  if (error || !node) {
    return (
      <Layout>
        <div className="flex items-center justify-center h-full">
          <div className="text-center max-w-md">
            <div className="text-destructive mb-4">
              <svg
                xmlns="http://www.w3.org/2000/svg"
                width="48"
                height="48"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              >
                <circle cx="12" cy="12" r="10" />
                <line x1="15" x2="9" y1="9" y2="15" />
                <line x1="9" x2="15" y1="9" y2="15" />
              </svg>
            </div>
            <h3 className="text-xl font-semibold mb-2">Failed to load node</h3>
            <p className="text-muted-foreground mb-6">{error || 'Node not found'}</p>
            <button
              onClick={() => navigate('/nodes')}
              className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors"
            >
              Back to Nodes
            </button>
          </div>
        </div>
      </Layout>
    )
  }

  const isActive = node.active || node.id === activeNode
  const uptime = (node as any)?.uptime_seconds
  const uptimeString = uptime !== undefined ? formatUptime(uptime) : 'Unknown'

  // HID stats display
  const hidStats = (node as any)?.hid_stats
  const scenario = (node as any)?.scenario

  return (
    <Layout>
      <div className="max-w-6xl mx-auto">
        {/* Header */}
        <div className="flex justify-between items-center mb-6">
          <div>
            <div className="flex items-center gap-3 mb-2">
              <h2 className="text-2xl font-bold">{node.name}</h2>
              {isActive && (
                <span className="px-2 py-1 text-xs font-medium bg-emerald-500 text-white rounded-full">
                  Active
                </span>
              )}
            </div>
            <p className="text-muted-foreground">Node Details</p>
          </div>
          <div className="flex gap-2">
            <button
              onClick={() => navigate('/nodes')}
              className="px-4 py-2 text-sm font-medium bg-secondary text-foreground rounded-lg hover:bg-secondary/80 transition-colors"
            >
              Back to Nodes
            </button>
            {isActive ? (
              <button
                disabled
                className="px-4 py-2 text-sm font-medium bg-emerald-500 text-white rounded-lg opacity-50 cursor-not-allowed"
              >
                Currently Active
              </button>
            ) : (
              <Button onClick={handleActivate} className="px-4 py-2 text-sm font-medium">
                Activate Node
              </Button>
            )}
          </div>
        </div>

        {/* Error banner for activation errors */}
        {error && (
          <div className="mb-4 p-3 bg-destructive/10 border border-destructive/20 rounded-lg flex items-center gap-2">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="16"
              height="16"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              className="text-destructive"
            >
              <circle cx="12" cy="12" r="10" />
              <line x1="12" x2="12" y1="8" y2="12" />
              <line x1="12" x2="13" y1="17" y2="17" />
            </svg>
            <p className="text-sm text-destructive">{error}</p>
          </div>
        )}

        <div className="grid grid-cols-1 lg:grid-cols-3 gap-6">
          {/* Main Info Column */}
          <div className="lg:col-span-2 space-y-6">
            {/* Node Info Card */}
            <div className="bg-card rounded-xl border p-6">
              <h3 className="text-lg font-semibold mb-4">Node Information</h3>
              <div className="grid grid-cols-2 gap-4">
                <div>
                  <span className="text-sm text-muted-foreground">Node ID</span>
                  <p className="font-mono text-sm break-all">{node.id}</p>
                </div>
                <div>
                  <span className="text-sm text-muted-foreground">Hostname</span>
                  <p className="font-mono text-sm">{node.host}</p>
                </div>
                <div>
                  <span className="text-sm text-muted-foreground">IP Address</span>
                  <p className="text-sm">{node.ip_address || node.host}</p>
                </div>
                <div>
                  <span className="text-sm text-muted-foreground">Machine Class</span>
                  <p className="text-sm capitalize">{node.machine_class}</p>
                </div>
                <div>
                  <span className="text-sm text-muted-foreground">Status</span>
                  <div className="flex items-center gap-2">
                    <span className={`w-2 h-2 rounded-full ${getStatusColor(node.active ? 'online' : 'offline')}`}></span>
                    <span className="text-sm">{node.active ? 'Online' : 'Offline'}</span>
                  </div>
                </div>
                <div>
                  <span className="text-sm text-muted-foreground">Uptime</span>
                  <p className="text-sm">{uptimeString}</p>
                </div>
              </div>
            </div>

            {/* Stream Preview Card */}
            <div className="bg-card rounded-xl border p-6">
              <h3 className="text-lg font-semibold mb-4">Stream Preview</h3>
              <div className="aspect-video bg-black rounded-lg overflow-hidden flex items-center justify-center">
                {streamPort ? (
                  <div className="text-center p-4">
                    <div className="mb-4 flex justify-center">
                      <div className="w-16 h-16 border-4 border-primary/50 border-t-primary rounded-full animate-spin">
                        <div className="w-full h-full rounded-full border-t-4 border-primary"></div>
                      </div>
                    </div>
                    <p className="text-sm text-muted-foreground">
                      Stream available at port {streamPort}
                    </p>
                    <p className="text-xs text-muted-foreground mt-2">
                      (HLS stream at http://{node.host}:{streamPort}/stream/stream.m3u8)
                    </p>
                  </div>
                ) : (
                  <div className="text-center p-4">
                    <svg
                      xmlns="http://www.w3.org/2000/svg"
                      width="48"
                      height="48"
                      viewBox="0 0 24 24"
                      fill="none"
                      stroke="currentColor"
                      strokeWidth="2"
                      strokeLinecap="round"
                      strokeLinejoin="round"
                      className="text-muted-foreground mb-2"
                    >
                      <polygon points="23 7 16 12 23 17 23 7" />
                      <rect x="1" y="5" width="15" height="14" rx="2" ry="2" />
                    </svg>
                    <p className="text-sm text-muted-foreground">No stream available</p>
                    <p className="text-xs text-muted-foreground mt-2">
                      This node does not have a video stream configured
                    </p>
                  </div>
                )}
              </div>
            </div>

            {/* HID Stats Card */}
            {hidStats && (
              <div className="bg-card rounded-xl border p-6">
                <h3 className="text-lg font-semibold mb-4">HID Statistics</h3>
                <div className="grid grid-cols-2 md:grid-cols-4 gap-4">
                  <div className="bg-secondary rounded-lg p-3">
                    <span className="text-xs text-muted-foreground block mb-1">Total Keys</span>
                    <span className="text-2xl font-bold">{hidStats.total_keys || 0}</span>
                  </div>
                  <div className="bg-secondary rounded-lg p-3">
                    <span className="text-xs text-muted-foreground block mb-1">Total Clicks</span>
                    <span className="text-2xl font-bold">{hidStats.total_clicks || 0}</span>
                  </div>
                  <div className="bg-secondary rounded-lg p-3">
                    <span className="text-xs text-muted-foreground block mb-1">Total Scrolls</span>
                    <span className="text-2xl font-bold">{hidStats.total_scrolls || 0}</span>
                  </div>
                  <div className="bg-secondary rounded-lg p-3">
                    <span className="text-xs text-muted-foreground block mb-1">Last Activity</span>
                    <span className="text-sm font-medium">
                      {hidStats.last_activity ? formatUptime(Date.now() / 1000 - hidStats.last_activity) : 'N/A'}
                    </span>
                  </div>
                </div>
              </div>
            )}

            {/* Current Scenario Card */}
            {scenario && (
              <div className="bg-card rounded-xl border p-6">
                <h3 className="text-lg font-semibold mb-4">Current Scenario</h3>
                <div className="flex items-center gap-4">
                  <div
                    className="w-12 h-12 rounded-lg flex items-center justify-center"
                    style={{ backgroundColor: `${scenario.color}20` }}
                  >
                    <div
                      className="w-8 h-8 rounded-md"
                      style={{ backgroundColor: scenario.color }}
                    />
                  </div>
                  <div>
                    <p className="text-sm text-muted-foreground mb-1">Scenario Name</p>
                    <p className="text-lg font-semibold">{scenario.name}</p>
                  </div>
                  <div className="ml-auto">
                    <span
                      className="px-2 py-1 text-xs font-medium rounded-full"
                      style={{ backgroundColor: `${scenario.color}20`, color: scenario.color }}
                    >
                      ID: {scenario.id}
                    </span>
                  </div>
                </div>
              </div>
            )}
          </div>

          {/* Sidebar Column */}
          <div className="space-y-6">
            {/* Hardware Info Card */}
            <div className="bg-card rounded-xl border p-6">
              <h3 className="text-lg font-semibold mb-4">Hardware</h3>
              <div className="space-y-3">
                <div>
                  <span className="text-xs text-muted-foreground uppercase tracking-wider">Hardware Type</span>
                  <p className="text-sm font-medium mt-1">{node.hw || 'Unknown'}</p>
                </div>
                <div>
                  <span className="text-xs text-muted-foreground uppercase tracking-wider">Firmware Version</span>
                  <p className="text-sm font-medium mt-1">{node.fw_version || 'Unknown'}</p>
                </div>
                <div>
                  <span className="text-xs text-muted-foreground uppercase tracking-wider">Protocol Version</span>
                  <p className="text-sm font-medium mt-1">v{node.proto_version}</p>
                </div>
                <div>
                  <span className="text-xs text-muted-foreground uppercase tracking-wider">Role</span>
                  <p className="text-sm font-medium mt-1">{node.role}</p>
                </div>
              </div>
            </div>

            {/* Capabilities Card */}
            {node.capabilities && node.capabilities.length > 0 && (
              <div className="bg-card rounded-xl border p-6">
                <h3 className="text-lg font-semibold mb-4">Capabilities</h3>
                <div className="flex flex-wrap gap-2">
                  {node.capabilities.map((cap: string, idx: number) => (
                    <span
                      key={idx}
                      className="px-2 py-1 text-xs font-medium bg-secondary text-foreground rounded-md"
                    >
                      {cap}
                    </span>
                  ))}
                </div>
              </div>
            )}

            {/* Node Stats Card */}
            <div className="bg-card rounded-xl border p-6">
              <h3 className="text-lg font-semibold mb-4">Node Stats</h3>
              <div className="space-y-3">
                <div className="flex justify-between text-sm">
                  <span className="text-muted-foreground">Last Seen</span>
                  <span className="font-mono">
                    {new Date((node as any).last_seen * 1000).toLocaleString()}
                  </span>
                </div>
                {node.owner_user_id && (
                  <div className="flex justify-between text-sm">
                    <span className="text-muted-foreground">Owner</span>
                    <span className="font-mono">{node.owner_user_id}</span>
                  </div>
                )}
              </div>
            </div>
          </div>
        </div>
      </div>
    </Layout>
  )
}

// Format uptime from seconds to human-readable string
function formatUptime(seconds: number): string {
  if (seconds === undefined || seconds === null || isNaN(seconds)) {
    return 'Unknown'
  }
  
  const days = Math.floor(seconds / 86400)
  const hours = Math.floor((seconds % 86400) / 3600)
  const minutes = Math.floor((seconds % 3600) / 60)
  const secs = Math.floor(seconds % 60)
  
  if (days > 0) {
    return `${days}d ${hours}h ${minutes}m ${secs}s`
  }
  if (hours > 0) {
    return `${hours}h ${minutes}m ${secs}s`
  }
  return `${minutes}m ${secs}s`
}
