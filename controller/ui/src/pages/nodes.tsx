import { useEffect } from 'react'
import { useNodesStore } from '../hooks/useNodesStore'
import { NodeInfo } from '../types/node'

export default function NodesPage() {
  const { nodes, loading, error, refreshNodes } = useNodesStore()

  useEffect(() => {
    refreshNodes()
  }, [refreshNodes])

  const getStatusColor = (status: NodeInfo['status']) => {
    switch (status) {
      case 'online':
        return 'bg-emerald-600'
      case 'offline':
        return 'bg-gray-500'
      case 'connecting':
        return 'bg-yellow-500'
      case 'error':
        return 'bg-red-500'
      default:
        return 'bg-gray-500'
    }
  }

  const getMachineClassColor = (machineClass: NodeInfo['machine_class']) => {
    switch (machineClass) {
      case 'workstation':
        return 'bg-blue-600'
      case 'server':
        return 'bg-purple-600'
      case 'kiosk':
        return 'bg-indigo-600'
      default:
        return 'bg-gray-600'
    }
  }

  if (loading && nodes.length === 0) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="inline-block animate-spin rounded-full h-8 w-8 border-b-2 border-emerald-600 mb-4"></div>
          <p className="text-text-secondary">Loading nodes...</p>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="bg-bg-surface p-6 rounded-lg border border-border max-w-md">
          <h3 className="text-lg font-semibold text-text mb-2">Error Loading Nodes</h3>
          <p className="text-text-secondary mb-4">{error}</p>
          <button
            onClick={() => refreshNodes()}
            className="px-4 py-2 bg-emerald-600 text-white rounded-lg hover:bg-emerald-700 transition-colors"
          >
            Try Again
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="space-y-6">
      {/* Header with actions */}
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-2xl font-bold text-text">Nodes</h2>
          <p className="text-text-secondary text-sm mt-1">
            {nodes.length} node{nodes.length !== 1 ? 's' : ''} registered
          </p>
        </div>
        <div className="flex gap-2">
          <button
            onClick={() => refreshNodes()}
            disabled={loading}
            className="px-4 py-2 bg-bg-surface text-text border border-border rounded-lg hover:bg-bg-surface-light transition-colors disabled:opacity-50 flex items-center gap-2"
          >
            <span className={loading ? 'animate-spin' : ''}>↻</span>
            Refresh
          </button>
          <button className="px-4 py-2 bg-emerald-600 text-white rounded-lg hover:bg-emerald-700 transition-colors flex items-center gap-2">
            <span>＋</span>
            Add Node
          </button>
        </div>
      </div>

      {/* Nodes Grid */}
      {nodes.length === 0 ? (
        <div className="flex flex-col items-center justify-center py-16 text-center">
          <div className="w-24 h-24 bg-bg-surface rounded-full flex items-center justify-center mb-6">
            <span className="text-4xl">🖥️</span>
          </div>
          <h3 className="text-xl font-semibold text-text mb-2">No Nodes Found</h3>
          <p className="text-text-secondary max-w-md mb-6">
            No nodes have been registered yet. Nodes automatically discover themselves via mDNS or
            can be manually added.
          </p>
          <button className="px-6 py-3 bg-emerald-600 text-white rounded-lg hover:bg-emerald-700 transition-colors">
            Learn More
          </button>
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-6">
          {nodes.map((node) => (
            <div
              key={node.id}
              className="bg-bg-surface rounded-xl border border-border p-6 hover:border-emerald-600/50 transition-all group"
            >
              <div className="flex items-start justify-between mb-4">
                <div className="flex items-center gap-3">
                  <div className="w-12 h-12 bg-bg-surface-light rounded-lg flex items-center justify-center">
                    <span className="text-2xl">
                      {node.machine_class === 'workstation' ? '💻' : node.machine_class === 'server' ? '🖥️' : '🖥️'}
                    </span>
                  </div>
                  <div>
                    <h3 className="font-semibold text-text flex items-center gap-2">
                      {node.name}
                      {node.active && <span className="text-xs bg-emerald-600 px-2 py-0.5 rounded-full">Active</span>}
                    </h3>
                    <p className="text-sm text-text-secondary">{node.hostname}</p>
                  </div>
                </div>
                <div className={`w-3 h-3 rounded-full ${getStatusColor(node.status)}`}></div>
              </div>

              <div className="space-y-3">
                <div className="flex items-center justify-between text-sm">
                  <span className="text-text-secondary">IP Address</span>
                  <span className="text-text font-mono">{node.ip_address}</span>
                </div>
                <div className="flex items-center justify-between text-sm">
                  <span className="text-text-secondary">Machine Class</span>
                  <span className={`px-2 py-1 rounded text-xs font-medium ${getMachineClassColor(node.machine_class)} text-white`}>
                    {node.machine_class}
                  </span>
                </div>
                <div className="flex items-center justify-between text-sm">
                  <span className="text-text-secondary">Last Seen</span>
                  <span className="text-text">{new Date(node.last_seen).toLocaleString()}</span>
                </div>
              </div>

              <div className="mt-6 pt-4 border-t border-border flex items-center justify-between">
                <div className="flex gap-2">
                  <button className="px-3 py-1.5 text-sm bg-bg-surface-light text-text rounded hover:bg-bg-surface transition-colors">
                    Details
                  </button>
                  <button className="px-3 py-1.5 text-sm bg-emerald-600/10 text-emerald-600 rounded hover:bg-emerald-600/20 transition-colors">
                    Focus
                  </button>
                </div>
                {node.uptime !== undefined && (
                  <span className="text-xs text-text-secondary" title={`Uptime: ${node.uptime}s`}>
                    {node.uptime !== undefined && node.uptime > 0 && `${Math.floor(node.uptime / 60)}m`}
                  </span>
                )}
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
