import { useEffect, useState } from 'react'
import { useNodes } from '../hooks/useNodes'

export default function NodesPage() {
  const { nodes, loading, error, wsConnected, fetchNodes, clearError } = useNodes()
  const [filter, setFilter] = useState('')

  useEffect(() => {
    if (!wsConnected) {
      const interval = setInterval(() => {
        fetchNodes()
      }, 30000)
      return () => clearInterval(interval)
    }
  }, [wsConnected, fetchNodes])

  const filteredNodes = nodes.filter((node) =>
    node.name.toLowerCase().includes(filter.toLowerCase()) ||
    node.hostname.toLowerCase().includes(filter.toLowerCase()),
  )

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'online':
        return 'bg-emerald-500'
      case 'offline':
        return 'bg-red-500'
      case 'connecting':
        return 'bg-amber-500'
      default:
        return 'bg-gray-500'
    }
  }

  if (loading && nodes.length === 0) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="inline-block animate-spin rounded-full h-8 w-8 border-2 border-emerald-500 border-t-transparent mb-2" />
          <p className="text-muted">Loading nodes...</p>
        </div>
      </div>
    )
  }

  return (
    <div className="max-w-6xl mx-auto">
      <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-4 mb-6">
        <div>
          <h1 className="text-2xl font-bold text-foreground">Nodes</h1>
          <p className="text-muted mt-1">Manage and monitor your KVMA nodes</p>
        </div>
        <div className="flex items-center gap-2">
          <div className="relative">
            <input
              type="text"
              placeholder="Search nodes..."
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              className="w-full sm:w-64 px-4 py-2 rounded-lg border bg-background text-foreground placeholder-muted focus:outline-none focus:ring-2 focus:ring-emerald-500"
            />
            <span className="absolute right-3 top-2.5 text-muted">
              <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth={1.5} stroke="currentColor" className="w-4 h-4">
                <path strokeLinecap="round" strokeLinejoin="round" d="m21 21-5.197-5.197m0 0A7.5 7.5 0 1 0 5.196 5.196a7.5 7.5 0 0 0 10.607 10.607Z" />
              </svg>
            </span>
          </div>
          <button
            onClick={fetchNodes}
            disabled={loading}
            className="px-4 py-2 rounded-lg bg-emerald-600 text-white hover:bg-emerald-700 disabled:opacity-50 disabled:cursor-not-allowed transition-colors"
          >
            Refresh
          </button>
        </div>
      </div>

      {error && (
        <div className="mb-6 p-4 rounded-lg bg-red-500/10 border border-red-500/20 flex items-center justify-between">
          <div className="flex items-center gap-2 text-red-400">
            <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth={1.5} stroke="currentColor" className="w-5 h-5">
              <path strokeLinecap="round" strokeLinejoin="round" d="M12 9v2m0 4h.008v.008h-.008v-.008zm0-4h.008v.008h-.008v-.008z" />
            </svg>
            {error}
          </div>
          <button
            onClick={clearError}
            className="text-red-400 hover:text-red-300"
          >
            <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth={1.5} stroke="currentColor" className="w-4 h-4">
              <path strokeLinecap="round" strokeLinejoin="round" d="M6 18L18 6M6 6l12 12" />
            </svg>
          </button>
        </div>
      )}

      <div className="bg-card rounded-lg border overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full text-left text-sm">
            <thead className="bg-muted/50 text-muted">
              <tr>
                <th className="px-6 py-3 font-medium">Name</th>
                <th className="px-6 py-3 font-medium">Hostname</th>
                <th className="px-6 py-3 font-medium">IP Address</th>
                <th className="px-6 py-3 font-medium">Status</th>
                <th className="px-6 py-3 font-medium">Class</th>
                <th className="px-6 py-3 font-medium">Active</th>
                <th className="px-6 py-3 font-medium">Last Seen</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-muted/20">
              {filteredNodes.length === 0 ? (
                <tr>
                  <td colSpan={7} className="px-6 py-8 text-center text-muted">
                    {loading ? 'Loading...' : 'No nodes found'}
                  </td>
                </tr>
              ) : (
                filteredNodes.map((node) => (
                  <tr key={node.id} className="hover:bg-muted/5 transition-colors">
                    <td className="px-6 py-4">
                      <div className="flex items-center gap-3">
                        <div
                          className={`w-2 h-2 rounded-full ${getStatusColor(node.status)}`}
                          title={node.status}
                        />
                        <span className="font-medium text-foreground">{node.name}</span>
                      </div>
                    </td>
                    <td className="px-6 py-4 text-muted">{node.hostname}</td>
                    <td className="px-6 py-4 text-muted font-mono">{node.ip}</td>
                    <td className="px-6 py-4">
                      <span
                        className={`inline-flex items-center px-2 py-1 rounded-full text-xs font-medium ${
                          node.status === 'online'
                            ? 'bg-emerald-500/10 text-emerald-400'
                            : node.status === 'offline'
                              ? 'bg-red-500/10 text-red-400'
                              : 'bg-amber-500/10 text-amber-400'
                        }`}
                      >
                        {node.status}
                      </span>
                    </td>
                    <td className="px-6 py-4 text-muted capitalize">{node.machine_class}</td>
                    <td className="px-6 py-4">
                      {node.active ? (
                        <span className="text-emerald-400 flex items-center gap-1">
                          <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth={2} stroke="currentColor" className="w-4 h-4">
                            <path strokeLinecap="round" strokeLinejoin="round" d="M4.5 12.75l6 6 9-13.5" />
                          </svg>
                          Yes
                        </span>
                      ) : (
                        <span className="text-muted">No</span>
                      )}
                    </td>
                    <td className="px-6 py-4 text-muted text-right">
                      {new Date(node.last_seen).toLocaleString()}
                    </td>
                  </tr>
                ))
              )}
            </tbody>
          </table>
        </div>
      </div>

      <div className="mt-4 text-sm text-muted flex items-center gap-4">
        <span>Total: {nodes.length}</span>
        <span className="flex items-center gap-1">
          <span className="w-2 h-2 rounded-full bg-emerald-500" />
          {nodes.filter((n) => n.status === 'online').length} online
        </span>
        <span className="flex items-center gap-1">
          <span className="w-2 h-2 rounded-full bg-red-500" />
          {nodes.filter((n) => n.status === 'offline').length} offline
        </span>
      </div>
    </div>
  )
}
