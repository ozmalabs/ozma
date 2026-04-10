import { useEffect } from 'react'
import { useNodes } from '../hooks/useNodes'

const NodesPage = () => {
  const { nodes, loading, error, refreshNodes } = useNodes()

  useEffect(() => {
    refreshNodes()
  }, [refreshNodes])

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'online':
        return 'bg-emerald-500'
      case 'offline':
        return 'bg-gray-500'
      case 'connecting':
        return 'bg-amber-500'
      case 'error':
        return 'bg-red-500'
      default:
        return 'bg-gray-500'
    }
  }

  const getMachineClassColor = (machineClass: string) => {
    switch (machineClass) {
      case 'workstation':
        return 'bg-blue-500/20 text-blue-300 border-blue-500/30'
      case 'server':
        return 'bg-purple-500/20 text-purple-300 border-purple-500/30'
      case 'kiosk':
        return 'bg-amber-500/20 text-amber-300 border-amber-500/30'
      default:
        return 'bg-gray-500/20 text-gray-300 border-gray-500/30'
    }
  }

  return (
    <div className="max-w-6xl mx-auto">
      <div className="flex items-center justify-between mb-6">
        <div>
          <h1 className="text-2xl font-bold text-text-primary">Nodes</h1>
          <p className="text-text-secondary">Manage your KVMA router nodes</p>
        </div>
        <button
          onClick={() => refreshNodes()}
          disabled={loading}
          className="px-4 py-2 bg-accent-emerald hover:bg-accent-emerald-dim text-white rounded-lg transition-colors disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {loading ? 'Refreshing...' : 'Refresh'}
        </button>
      </div>

      {error && (
        <div className="mb-4 p-4 bg-red-500/10 border border-red-500/30 rounded-lg text-red-400">
          Error: {error}
        </div>
      )}

      <div className="bg-bg-secondary rounded-lg border border-border-color overflow-hidden">
        <div className="overflow-x-auto">
          <table className="w-full">
            <thead className="bg-bg-tertiary text-text-secondary uppercase text-xs font-medium">
              <tr>
                <th className="px-6 py-3">Name</th>
                <th className="px-6 py-3">Status</th>
                <th className="px-6 py-3">Machine Class</th>
                <th className="px-6 py-3">IP Address</th>
                <th className="px-6 py-3">Last Seen</th>
                <th className="px-6 py-3 text-center">Active</th>
              </tr>
            </thead>
            <tbody className="divide-y divide-border-color">
              {nodes.map((node) => (
                <tr key={node.id} className="hover:bg-bg-tertiary transition-colors">
                  <td className="px-6 py-4">
                    <div className="font-medium text-text-primary">{node.name}</div>
                    <div className="text-sm text-text-muted font-mono">{node.hostname}</div>
                  </td>
                  <td className="px-6 py-4">
                    <span className={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(node.status)}`}>
                      {node.status}
                    </span>
                  </td>
                  <td className="px-6 py-4">
                    <span className={`inline-flex items-center px-2.5 py-0.5 rounded-md text-xs font-medium border ${getMachineClassColor(node.machine_class)}`}>
                      {node.machine_class}
                    </span>
                  </td>
                  <td className="px-6 py-4 text-text-secondary text-sm font-mono">
                    {node.ip_address || 'N/A'}
                  </td>
                  <td className="px-6 py-4 text-text-secondary text-sm">
                    {new Date(node.last_seen).toLocaleString()}
                  </td>
                  <td className="px-6 py-4 text-center">
                    {node.active ? (
                      <span className="text-accent-emerald font-medium">✓ Active</span>
                    ) : (
                      <span className="text-text-muted">—</span>
                    )}
                  </td>
                </tr>
              ))}
              {nodes.length === 0 && (
                <tr>
                  <td colSpan={6} className="px-6 py-12 text-center text-text-muted">
                    No nodes found. Click Refresh to scan for nodes.
                  </td>
                </tr>
              )}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  )
}

export default NodesPage
