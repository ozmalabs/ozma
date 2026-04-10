import { useEffect } from 'react'
import { useNavigate } from 'react-router-dom'
import { useNodeStore } from '../store/nodeStore'
import { useWebSocketStore } from '../store/websocketStore'
import { Server, Plus, RefreshCw, AlertCircle } from 'lucide-react'

export default function NodesPage() {
  const { nodes, loading, error, fetchNodes } = useNodeStore()
  const { connect } = useWebSocketStore()
  const navigate = useNavigate()

  useEffect(() => {
    fetchNodes()
    connect()
  }, [fetchNodes, connect])

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'online':
        return 'bg-emerald-500'
      case 'offline':
        return 'bg-slate-500'
      case 'connecting':
        return 'bg-amber-500'
      case 'error':
        return 'bg-red-500'
      default:
        return 'bg-slate-500'
    }
  }

  const getStatusLabel = (status: string) => {
    return status.charAt(0).toUpperCase() + status.slice(1)
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-text">Nodes</h1>
          <p className="text-text-muted text-sm mt-1">
            {nodes.length} node{nodes.length !== 1 && 's'} registered
          </p>
        </div>
        <div className="flex items-center gap-2">
          <button
            onClick={() => fetchNodes()}
            disabled={loading}
            className="flex items-center gap-2 px-4 py-2 bg-bg-card border border-border rounded-lg text-text hover:bg-bg-card/80 disabled:opacity-50 transition-colors"
          >
            <RefreshCw className={`w-4 h-4 ${loading ? 'animate-spin' : ''}`} />
            <span>Refresh</span>
          </button>
          <button className="flex items-center gap-2 px-4 py-2 bg-emerald-500 hover:bg-emerald-400 text-black font-medium rounded-lg transition-colors">
            <Plus className="w-4 h-4" />
            <span>Add Node</span>
          </button>
        </div>
      </div>

      {error && (
        <div className="p-4 bg-red-500/10 border border-red-500/20 rounded-lg flex items-center gap-3">
          <AlertCircle className="w-5 h-5 text-red-500" />
          <span className="text-red-500">{error}</span>
        </div>
      )}

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
        {nodes.map((node) => (
          <div
            key={node.id}
            onClick={() => navigate(`/nodes/${node.id}`)}
            className="group bg-bg-card border border-border rounded-xl p-5 hover:border-emerald-400/50 transition-all cursor-pointer hover:shadow-lg hover:shadow-emerald-400/5"
          >
            <div className="flex items-start justify-between mb-4">
              <div className="flex items-center gap-3">
                <div className="p-2 bg-bg-sidebar rounded-lg">
                  <Server className="w-6 h-6 text-emerald-400" />
                </div>
                <div>
                  <h3 className="font-semibold text-text group-hover:text-emerald-400 transition-colors">
                    {node.name}
                  </h3>
                  <p className="text-xs text-text-muted font-mono">{node.hostname}</p>
                </div>
              </div>
              <div className="flex flex-col items-end gap-1">
                <span
                  className={`px-2 py-1 rounded-full text-xs font-medium ${
                    node.active
                      ? 'bg-emerald-500/20 text-emerald-400'
                      : 'bg-slate-500/20 text-slate-400'
                  }`}
                >
                  {node.active ? 'Active' : 'Standby'}
                </span>
                <span
                  className={`px-2 py-1 rounded-full text-xs font-medium flex items-center gap-1.5 ${
                    node.status === 'online'
                      ? 'bg-emerald-500/20 text-emerald-400'
                      : node.status === 'offline'
                        ? 'bg-slate-500/20 text-slate-400'
                        : node.status === 'connecting'
                          ? 'bg-amber-500/20 text-amber-400'
                          : 'bg-red-500/20 text-red-400'
                  }`}
                >
                  <span className={`w-1.5 h-1.5 rounded-full ${getStatusColor(node.status)}`}></span>
                  {getStatusLabel(node.status)}
                </span>
              </div>
            </div>

            <div className="space-y-2 text-sm text-text-muted">
              <div className="flex items-center gap-2">
                <span className="w-20 text-xs">Machine Class:</span>
                <span className="font-medium text-text capitalize">{node.machine_class}</span>
              </div>
              <div className="flex items-center gap-2">
                <span className="w-20 text-xs">IP Address:</span>
                <span className="font-mono text-xs">{node.ip_address || 'N/A'}</span>
              </div>
              <div className="flex items-center gap-2">
                <span className="w-20 text-xs">Last Seen:</span>
                <span className="text-xs">
                  {new Date(node.last_seen).toLocaleString()}
                </span>
              </div>
            </div>
          </div>
        ))}
        {nodes.length === 0 && !loading && (
          <div className="col-span-full py-12 text-center border border-dashed border-border rounded-xl">
            <Server className="w-12 h-12 text-text-muted mx-auto mb-3" />
            <h3 className="text-lg font-medium text-text">No nodes yet</h3>
            <p className="text-text-muted text-sm mt-1">
              Add a new node to start managing your KVMA infrastructure
            </p>
          </div>
        )}
      </div>

      <div className="flex items-center justify-center text-xs text-text-muted gap-4">
        <span className="flex items-center gap-2">
          <span className="w-2 h-2 bg-emerald-500 rounded-full"></span> Online
        </span>
        <span className="flex items-center gap-2">
          <span className="w-2 h-2 bg-slate-500 rounded-full"></span> Offline
        </span>
        <span className="flex items-center gap-2">
          <span className="w-2 h-2 bg-amber-500 rounded-full"></span> Connecting
        </span>
        <span className="flex items-center gap-2">
          <span className="w-2 h-2 bg-red-500 rounded-full"></span> Error
        </span>
      </div>
    </div>
  )
}
