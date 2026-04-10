import { useEffect, useMemo } from 'react'
import { Link } from 'react-router-dom'
import { useNodes } from '../hooks/useNodes'
import { ServerIcon, CpuIcon, ActivityIcon, RefreshCwIcon } from 'lucide-react'
import { NodeInfo } from '../types'

const NodesPage = () => {
  const { nodes, activeNodeId, isLoading, error, fetchNodes, connectWebSocket } = useNodes()

  useEffect(() => {
    fetchNodes()
    connectWebSocket()
  }, [fetchNodes, connectWebSocket])

  const getStatusColor = (status: NodeInfo['status']) => {
    switch (status) {
      case 'online':
        return 'bg-success'
      case 'offline':
        return 'bg-error'
      case 'connecting':
        return 'bg-warning animate-pulse'
      case 'error':
        return 'bg-error'
      default:
        return 'bg-text-tertiary'
    }
  }

  const getStatusText = (status: NodeInfo['status']) => {
    switch (status) {
      case 'online':
        return 'Online'
      case 'offline':
        return 'Offline'
      case 'connecting':
        return 'Connecting'
      case 'error':
        return 'Error'
      default:
        return status
    }
  }

  const activeNode = useMemo(() => nodes.find(n => n.id === activeNodeId), [nodes, activeNodeId])

  return (
    <div className="max-w-7xl mx-auto space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div>
          <h2 className="text-2xl font-bold text-text-primary">Nodes</h2>
          <p className="text-text-tertiary mt-1">
            {nodes.length} node{nodes.length !== 1 ? 's' : ''} registered
          </p>
        </div>
        <button
          onClick={fetchNodes}
          disabled={isLoading}
          className="flex items-center gap-2 px-4 py-2 bg-secondary hover:bg-tertiary rounded-md border border-border text-sm font-medium text-text-primary transition-colors disabled:opacity-50"
        >
          <RefreshCwIcon className={`w-4 h-4 ${isLoading ? 'animate-spin' : ''}`} />
          Refresh
        </button>
      </div>

      {/* Active Node Banner */}
      {activeNode && (
        <div className="bg-brand-accent-dim rounded-lg p-4 border border-brand-accent/30 flex items-center justify-between">
          <div className="flex items-center gap-4">
            <div className="w-12 h-12 bg-brand-accent rounded-lg flex items-center justify-center flex-shrink-0">
              <NodeIcon className="w-6 h-6 text-black" />
            </div>
            <div>
              <p className="text-sm font-medium text-brand-accent/80">Active Node</p>
              <h3 className="text-xl font-bold text-text-primary">{activeNode.name}</h3>
            </div>
          </div>
          <div className="flex items-center gap-6">
            <div className="text-right hidden md:block">
              <p className="text-xs text-text-tertiary">Hostname</p>
              <p className="text-sm font-mono text-text-primary">{activeNode.hostname}</p>
            </div>
            <div className="text-right">
              <p className="text-xs text-text-tertiary">Status</p>
              <div className="flex items-center gap-2">
                <span className={`w-2.5 h-2.5 rounded-full ${getStatusColor(activeNode.status)}`}></span>
                <span className="text-sm font-medium text-success">{getStatusText(activeNode.status)}</span>
              </div>
            </div>
          </div>
        </div>
      )}

      {/* Error State */}
      {error && (
        <div className="bg-error/10 border border-error/30 rounded-lg p-4 flex items-start gap-3">
          <div className="w-5 h-5 bg-error rounded-full flex items-center justify-center flex-shrink-0 mt-0.5">
            <span className="text-white text-xs font-bold">!</span>
          </div>
          <div>
            <p className="text-sm font-medium text-error">Failed to load nodes</p>
            <p className="text-sm text-error/80 mt-1">{error}</p>
          </div>
        </div>
      )}

      {/* Nodes Grid */}
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
        {nodes.map((node) => {
          const isActive = node.id === activeNodeId
          return (
            <Link
              key={node.id}
              to={`/nodes/${node.id}`}
              className={`
                group relative bg-secondary hover:bg-tertiary rounded-lg border transition-all duration-200
                ${isActive 
                  ? 'border-brand-accent ring-1 ring-brand-accent/50' 
                  : 'border-border hover:border-brand-accent/50 hover:shadow-lg hover:shadow-brand-accent/10'}
              `}
            >
              {/* Active Indicator */}
              {isActive && (
                <div className="absolute -top-2 left-1/2 -translate-x-1/2 bg-brand-accent text-black text-xs font-bold px-3 py-0.5 rounded-full shadow-lg">
                  Active
                </div>
              )}

              <div className="p-5">
                <div className="flex items-start justify-between mb-4">
                  <div className="flex items-center gap-3">
                    <div className={`
                      w-10 h-10 rounded-lg flex items-center justify-center transition-colors
                      ${isActive ? 'bg-brand-accent text-black' : 'bg-secondary text-text-primary group-hover:bg-brand-accent group-hover:text-black'}
                    `}>
                      <NodeIcon className="w-5 h-5" />
                    </div>
                    <div>
                      <h3 className="font-semibold text-text-primary group-hover:text-brand-accent transition-colors">
                        {node.name}
                      </h3>
                      <p className="text-xs text-text-tertiary font-mono">{node.id.substring(0, 8)}</p>
                    </div>
                  </div>
                  <span className={`
                    text-xs font-medium px-2 py-1 rounded-full
                    ${isActive 
                      ? 'bg-brand-accent/20 text-brand-accent' 
                      : 'bg-text-tertiary/10 text-text-tertiary'}
                  `}>
                    {node.machine_class}
                  </span>
                </div>

                {/* Node Info */}
                <div className="space-y-2 text-sm">
                  <div className="flex items-center justify-between">
                    <div className="flex items-center gap-2 text-text-tertiary">
                      <CpuIcon className="w-4 h-4" />
                      <span>Hostname</span>
                    </div>
                    <span className="font-mono text-text-primary">{node.hostname}</span>
                  </div>
                  {node.ip_address && (
                    <div className="flex items-center justify-between">
                      <div className="flex items-center gap-2 text-text-tertiary">
                        <ActivityIcon className="w-4 h-4" />
                        <span>IP Address</span>
                      </div>
                      <span className="font-mono text-text-primary">{node.ip_address}</span>
                    </div>
                  )}
                </div>

                {/* Status Footer */}
                <div className="mt-4 pt-4 border-t border-border flex items-center justify-between">
                  <div className="flex items-center gap-2">
                    <span className={`w-2 h-2 rounded-full ${getStatusColor(node.status)}`}></span>
                    <span className="text-xs font-medium text-text-secondary">
                      {getStatusText(node.status)}
                    </span>
                  </div>
                  <span className="text-xs text-text-tertiary">
                    {node.last_seen ? new Date(node.last_seen).toLocaleDateString() : 'Never'}
                  </span>
                </div>
              </div>
            </Link>
          )
        })}
      </div>

      {/* Empty State */}
      {nodes.length === 0 && !isLoading && (
        <div className="text-center py-16">
          <div className="w-16 h-16 bg-secondary rounded-full flex items-center justify-center mx-auto mb-4">
            <NodeIcon className="w-8 h-8 text-text-tertiary" />
          </div>
          <h3 className="text-lg font-medium text-text-primary">No nodes yet</h3>
          <p className="text-text-tertiary mt-1">Nodes will appear here once they register with the controller</p>
        </div>
      )}

      {/* Loading State */}
      {isLoading && nodes.length === 0 && (
        <div className="flex items-center justify-center py-16">
          <div className="flex items-center gap-3">
            <div className="w-8 h-8 border-2 border-brand-accent border-t-transparent rounded-full animate-spin"></div>
            <span className="text-text-primary">Loading nodes...</span>
          </div>
        </div>
      )}
    </div>
  )
}

export default NodesPage
