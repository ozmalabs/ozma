import { useEffect, useState } from 'react'
import { useNodesStore } from '../hooks/useNodes'
import { Node } from '../types/api'
import { useWebSocket } from '../hooks/useNodes'

export default function NodesPage() {
  const { nodes, loading, error, fetchNodes } = useNodesStore()
  const [searchTerm, setSearchTerm] = useState('')
  const [filterStatus, setFilterStatus] = useState<string>('all')

  useWebSocket()

  useEffect(() => {
    fetchNodes()
  }, [fetchNodes])

  const filteredNodes = nodes.filter((node) => {
    const matchesSearch =
      node.name.toLowerCase().includes(searchTerm.toLowerCase()) ||
      node.hostname.toLowerCase().includes(searchTerm.toLowerCase()) ||
      node.ip_address?.toLowerCase().includes(searchTerm.toLowerCase())
    const matchesStatus =
      filterStatus === 'all' || node.status === filterStatus
    return matchesSearch && matchesStatus
  })

  const getStatusColor = (status: Node['status']) => {
    const colors = {
      online: 'bg-emerald-500',
      offline: 'bg-slate-500',
      connecting: 'bg-amber-500',
      error: 'bg-red-500',
    }
    return colors[status]
  }

  const getStatusText = (status: Node['status']) => {
    const texts = {
      online: 'Online',
      offline: 'Offline',
      connecting: 'Connecting',
      error: 'Error',
    }
    return texts[status]
  }

  if (loading && nodes.length === 0) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="flex flex-col items-center gap-4">
          <div className="w-12 h-12 border-4 border-primary border-t-transparent rounded-full animate-spin" />
          <p className="text-muted-foreground">Loading nodes...</p>
        </div>
      </div>
    )
  }

  if (error && nodes.length === 0) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center space-y-4">
          <div className="text-red-500">
            <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="w-16 h-16 mx-auto">
              <circle cx="12" cy="12" r="10" />
              <line x1="12" y1="8" x2="12" y2="12" />
              <line x1="12" y1="16" x2="12.01" y2="16" />
            </svg>
          </div>
          <div className="space-y-2">
            <h3 className="text-lg font-medium">Failed to load nodes</h3>
            <p className="text-sm text-muted-foreground">{error}</p>
          </div>
          <button
            onClick={() => fetchNodes()}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors"
          >
            Retry
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <div className="flex flex-col md:flex-row md:items-center justify-between gap-4">
        <div>
          <h2 className="text-2xl font-bold">Nodes</h2>
          <p className="text-muted-foreground text-sm">
            {nodes.length} node{nodes.length !== 1 ? 's' : ''} connected
          </p>
        </div>
        <div className="flex flex-col sm:flex-row gap-3">
          <div className="relative">
            <input
              type="text"
              placeholder="Search nodes..."
              value={searchTerm}
              onChange={(e) => setSearchTerm(e.target.value)}
              className="w-full sm:w-64 px-4 py-2 bg-card border border-border rounded-lg focus:outline-none focus:ring-2 focus:ring-primary text-foreground placeholder:text-muted-foreground"
            />
            <svg
              xmlns="http://www.w3.org/2000/svg"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              className="absolute right-3 top-2.5 w-4 h-4 text-muted-foreground"
            >
              <circle cx="11" cy="11" r="8" />
              <line x1="21" y1="21" x2="16.65" y2="16.65" />
            </svg>
          </div>
          <select
            value={filterStatus}
            onChange={(e) => setFilterStatus(e.target.value)}
            className="px-4 py-2 bg-card border border-border rounded-lg focus:outline-none focus:ring-2 focus:ring-primary text-foreground"
          >
            <option value="all">All Status</option>
            <option value="online">Online</option>
            <option value="offline">Offline</option>
            <option value="connecting">Connecting</option>
            <option value="error">Error</option>
          </select>
        </div>
      </div>

      {filteredNodes.length === 0 ? (
        <div className="text-center py-12">
          <div className="text-muted-foreground mb-2">No nodes found</div>
          {nodes.length === 0 ? (
            <p className="text-sm text-muted-foreground">Try adjusting your filters or wait for nodes to connect</p>
          ) : (
            <p className="text-sm text-muted-foreground">
              No nodes match your search criteria
            </p>
          )}
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
          {filteredNodes.map((node) => (
            <div
              key={node.id}
              className="bg-card rounded-lg border border-border p-5 hover:border-primary/50 transition-colors group"
            >
              <div className="flex items-start justify-between mb-4">
                <div className="flex items-center gap-3">
                  <div className={`w-2 h-2 ${getStatusColor(node.status)} rounded-full`} />
                  <div className="flex flex-col">
                    <h3 className="font-medium text-foreground">{node.name}</h3>
                    <span className="text-xs text-muted-foreground">{node.hostname}</span>
                  </div>
                </div>
                {node.active && (
                  <span className="px-2 py-1 bg-primary/10 text-primary text-xs font-medium rounded-full">
                    Active
                  </span>
                )}
              </div>

              <div className="space-y-2 text-sm text-muted-foreground">
                <div className="flex items-center gap-2">
                  <svg
                    xmlns="http://www.w3.org/2000/svg"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    className="w-4 h-4"
                  >
                    <path d="M22 16.2V6a2 2 0 0 0-2-2H6.3l-.9 1.8a2 2 0 0 1-2.3.7l-.7.2a2 2 0 0 0-1.4 2v8a2 2 0 0 0 2 2h15.5" />
                    <path d="M15.5 2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-7l-4.1 2a2 2 0 0 1-2.6-.2l-.4-.6a2 2 0 0 0-2.6-.2l-1.1.2a2 2 0 0 1-2-2V8a2 2 0 0 0 0 .2l.5.5a2 2 0 0 1 .7 2.3l.9 1.8H6.3" />
                  </svg>
                  <span>{node.ip_address || 'Unknown IP'}</span>
                </div>
                <div className="flex items-center gap-2">
                  <svg
                    xmlns="http://www.w3.org/2000/svg"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    className="w-4 h-4"
                  >
                    <circle cx="12" cy="12" r="10" />
                    <polyline points="12 6 12 12 16 14" />
                  </svg>
                  <span>Version {node.version}</span>
                </div>
                <div className="flex items-center gap-2">
                  <svg
                    xmlns="http://www.w3.org/2000/svg"
                    viewBox="0 0 24 24"
                    fill="none"
                    stroke="currentColor"
                    strokeWidth="2"
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    className="w-4 h-4"
                  >
                    <rect x="2" y="5" width="20" height="14" rx="2" />
                    <line x1="2" y1="10" x2="22" y2="10" />
                  </svg>
                  <span className="capitalize">{node.machine_class}</span>
                </div>
              </div>

              <div className="mt-4 pt-4 border-t border-border flex items-center justify-between">
                <span className={`text-xs font-medium ${getStatusColor(node.status)} text-white px-2 py-1 rounded`}>
                  {getStatusText(node.status)}
                </span>
                <span className="text-xs text-muted-foreground">
                  Last seen: {node.last_seen ? new Date(node.last_seen).toLocaleDateString() : 'Never'}
                </span>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}
