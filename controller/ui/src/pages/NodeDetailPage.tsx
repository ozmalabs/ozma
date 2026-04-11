import { useEffect, useState } from 'react'
import { useParams, Link, useNavigate } from 'react-router-dom'
import { api } from '../api/client'
import { useNodesStore } from '../store/useNodesStore'

export default function NodeDetailPage() {
  const { id } = useParams<{ id: string }>()
  const navigate = useNavigate()
  const { nodes, updateNode } = useNodesStore()
  const [node, setNode] = useState<any>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [activeTab, setActiveTab] = useState<'overview' | 'remote' | 'settings'>('overview')

  useEffect(() => {
    const loadNode = async () => {
      if (!id) return

      // First try to find in store
      const storedNode = nodes.find((n) => n.id === id)
      if (storedNode) {
        setNode(storedNode)
        setLoading(false)
        return
      }

      // Otherwise fetch from API
      try {
        setLoading(true)
        const response = await api.nodes.get(id)
        if (response.node) {
          setNode(response.node)
          updateNode(response.node)
        }
      } catch (err) {
        setError(err instanceof Error ? err.message : 'Failed to load node')
      } finally {
        setLoading(false)
      }
    }

    loadNode()
  }, [id, nodes, updateNode])

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin mx-auto mb-4"></div>
          <p className="text-muted-foreground">Loading node details...</p>
        </div>
      </div>
    )
  }

  if (error || !node) {
    return (
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
          <h3 className="text-xl font-semibold mb-2">Node not found</h3>
          <p className="text-muted-foreground mb-6">{error || 'The requested node could not be found.'}</p>
          <button
            onClick={() => navigate('/nodes')}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors"
          >
            Back to Nodes
          </button>
        </div>
      </div>
    )
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

  return (
    <div className="max-w-6xl mx-auto">
      {/* Header */}
      <div className="flex items-center justify-between mb-6">
        <div className="flex items-center gap-4">
          <button
            onClick={() => navigate('/nodes')}
            className="p-2 hover:bg-secondary rounded-lg transition-colors"
          >
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="24"
              height="24"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="m12 19-7-7 7-7m7 7v-4m0 4L5" />
            </svg>
          </button>
          <div>
            <h1 className="text-2xl font-bold flex items-center gap-2">
              {node.name}
              <span className={`w-3 h-3 rounded-full ${getStatusColor(node.status)} animate-pulse`}></span>
            </h1>
            <p className="text-muted-foreground">{node.hostname}</p>
          </div>
        </div>
        <div className="flex gap-2">
          <button className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors flex items-center gap-2">
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
            >
              <path d="M5 12h14" />
              <path d="M12 5v14" />
            </svg>
            Action
          </button>
        </div>
      </div>

      {/* Tabs */}
      <div className="flex gap-1 mb-6 border-b">
        <button
          onClick={() => setActiveTab('overview')}
          className={`px-4 py-2 rounded-t-lg transition-colors ${
            activeTab === 'overview'
              ? 'bg-secondary text-foreground border-b-2 border-primary'
              : 'text-muted-foreground hover:bg-secondary'
          }`}
        >
          Overview
        </button>
        <button
          onClick={() => setActiveTab('remote')}
          className={`px-4 py-2 rounded-t-lg transition-colors ${
            activeTab === 'remote'
              ? 'bg-secondary text-foreground border-b-2 border-primary'
              : 'text-muted-foreground hover:bg-secondary'
          }`}
        >
          Remote Desktop
        </button>
        <button
          onClick={() => setActiveTab('settings')}
          className={`px-4 py-2 rounded-t-lg transition-colors ${
            activeTab === 'settings'
              ? 'bg-secondary text-foreground border-b-2 border-primary'
              : 'text-muted-foreground hover:bg-secondary'
          }`}
        >
          Settings
        </button>
      </div>

      {/* Content */}
      <div className="bg-card rounded-xl border overflow-hidden">
        {activeTab === 'overview' && (
          <div className="p-6">
            <div className="grid grid-cols-1 md:grid-cols-2 gap-6">
              {/* System Info */}
              <div className="space-y-4">
                <h3 className="text-lg font-semibold">System Information</h3>
                <div className="space-y-2 text-sm">
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">ID</span>
                    <span className="font-mono text-foreground">{node.id}</span>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">Hostname</span>
                    <span className="font-mono text-foreground">{node.hostname}</span>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">Machine Class</span>
                    <span className="capitalize text-foreground">{node.machine_class}</span>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">Platform</span>
                    <span className="text-foreground">{node.platform || 'Unknown'}</span>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">Version</span>
                    <span className="text-foreground">{node.version || 'Unknown'}</span>
                  </div>
                </div>
              </div>

              {/* Network Info */}
              <div className="space-y-4">
                <h3 className="text-lg font-semibold">Network Information</h3>
                <div className="space-y-2 text-sm">
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">IP Address</span>
                    <span className="font-mono text-foreground">{node.ip_address}</span>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">MAC Address</span>
                    <span className="font-mono text-foreground">{node.mac_address || 'N/A'}</span>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">Status</span>
                    <span className={`font-medium ${getStatusColor(node.status)}`}>
                      {node.status}
                    </span>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">Last Seen</span>
                    <span className="text-foreground">{new Date(node.last_seen).toLocaleString()}</span>
                  </div>
                  <div className="flex justify-between">
                    <span className="text-muted-foreground">Active</span>
                    <span className={node.active ? 'text-emerald-500' : 'text-muted-foreground'}>
                      {node.active ? 'Yes' : 'No'}
                    </span>
                  </div>
                </div>
              </div>
            </div>

            {/* Actions */}
            <div className="mt-6 pt-6 border-t">
              <h3 className="text-lg font-semibold mb-4">Actions</h3>
              <div className="flex flex-wrap gap-2">
                <button className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors flex items-center gap-2">
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
                  >
                    <rect width="18" height="15" x="3" y="4" rx="2" ry="2" />
                  </svg>
                  Remote Desktop
                </button>
                <button className="px-4 py-2 bg-secondary text-foreground rounded-lg hover:bg-secondary/90 transition-colors flex items-center gap-2">
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
                  >
                    <path d="M12 20h9" />
                    <path d="M16.5 3.5a2.121 2.121 0 0 1 3 3L7 19l-4 1 1-4L16.5 3.5z" />
                  </svg>
                  Switch to This Node
                </button>
                <button className="px-4 py-2 bg-border text-foreground rounded-lg hover:bg-border/90 transition-colors flex items-center gap-2">
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
                  >
                    <path d="M21 15a2 2 0 0 1-2 2H7l-4 4V5a2 2 0 0 1 2-2h14a2 2 0 0 1 2 2z" />
                  </svg>
                  View Logs
                </button>
              </div>
            </div>
          </div>
        )}

        {activeTab === 'remote' && (
          <div className="p-6">
            <h3 className="text-lg font-semibold mb-4">Remote Desktop</h3>
            <div className="bg-muted/30 rounded-lg p-8 text-center">
              <div className="mb-4">
                <svg
                  xmlns="http://www.w3.org/2000/svg"
                  width="64"
                  height="64"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="1"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                  className="text-muted-foreground"
                >
                  <rect width="18" height="15" x="3" y="4" rx="2" ry="2" />
                  <line x1="2" x2="22" y1="20" y2="20" />
                  <line x1="4" x2="8" y1="20" y2="20" />
                </svg>
              </div>
              <p className="text-muted-foreground mb-6">
                Remote desktop access is available for active nodes.
              </p>
              <button className="px-6 py-3 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors flex items-center gap-2 mx-auto">
                <svg
                  xmlns="http://www.w3.org/2000/svg"
                  width="20"
                  height="20"
                  viewBox="0 0 24 24"
                  fill="none"
                  stroke="currentColor"
                  strokeWidth="2"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                >
                  <rect width="18" height="15" x="3" y="4" rx="2" ry="2" />
                </svg>
                Open Remote Desktop
              </button>
            </div>
          </div>
        )}

        {activeTab === 'settings' && (
          <div className="p-6">
            <h3 className="text-lg font-semibold mb-4">Node Settings</h3>
            <div className="space-y-4">
              <div className="p-4 bg-muted/30 rounded-lg">
                <label className="text-sm font-medium text-foreground">Node Name</label>
                <input
                  type="text"
                  defaultValue={node.name}
                  className="mt-2 w-full px-4 py-2 rounded-lg border bg-background text-foreground"
                />
              </div>
              <div className="flex gap-2">
                <button className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors">
                  Save Changes
                </button>
                <button className="px-4 py-2 bg-destructive text-destructive-foreground rounded-lg hover:bg-destructive/90 transition-colors">
                  Delete Node
                </button>
              </div>
            </div>
          </div>
        )}
      </div>
    </div>
  )
}
