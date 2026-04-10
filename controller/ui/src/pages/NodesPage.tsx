import { useEffect } from 'react'
import { Link } from 'react-router-dom'
import { Layout } from '../components/layout'
import { useNodesStore } from '../store/useNodesStore'

export function NodesPage() {
  const { nodes, loading, error, fetchNodes, lastUpdated } = useNodesStore()

  useEffect(() => {
    fetchNodes()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'online':
        return 'bg-emerald/20 text-emerald border-emerald/30'
      case 'offline':
        return 'bg-text-secondary/20 text-text-secondary border-text-secondary/30'
      case 'connecting':
        return 'bg-accent/20 text-accent border-accent/30 animate-pulse'
      case 'error':
        return 'bg-danger/20 text-danger border-danger/30'
      default:
        return 'bg-bg-tertiary text-text-secondary border-bg-tertiary'
    }
  }

  const getStatusIcon = (status: string) => {
    switch (status) {
      case 'online':
        return (
          <svg className="w-2 h-2" fill="currentColor" viewBox="0 0 24 24">
            <circle cx="12" cy="12" r="10" />
          </svg>
        )
      case 'offline':
        return (
          <svg className="w-2 h-2" fill="currentColor" viewBox="0 0 24 24">
            <circle cx="12" cy="12" r="4" />
          </svg>
        )
      case 'connecting':
        return (
          <svg className="w-2 h-2" fill="currentColor" viewBox="0 0 24 24">
            <circle cx="12" cy="12" r="8" />
          </svg>
        )
      case 'error':
        return (
          <svg className="w-2 h-2" fill="currentColor" viewBox="0 0 24 24">
            <path d="M12 2C6.48 2 2 6.48 2 12s4.48 10 10 10 10-4.48 10-10S17.52 2 12 2zm1 15h-2v-2h2v2zm0-4h-2V7h2v6z" />
          </svg>
        )
      default:
        return null
    }
  }

  if (loading && nodes.length === 0) {
    return (
      <Layout>
        <div className="flex items-center justify-center h-full">
          <div className="flex flex-col items-center gap-4">
            <div className="w-12 h-12 border-4 border-emerald/20 border-t-emerald rounded-full animate-spin"></div>
            <p className="text-text-secondary">Loading nodes...</p>
          </div>
        </div>
      </Layout>
    )
  }

  return (
    <Layout>
      <div className="max-w-6xl mx-auto">
        <div className="flex items-center justify-between mb-6">
          <div>
            <h1 className="text-2xl font-bold text-text">Nodes</h1>
            <p className="text-text-secondary">Manage and monitor your KVMA nodes</p>
          </div>
          <div className="flex items-center gap-3">
            <button className="px-4 py-2 bg-emerald hover:bg-emerald-dim text-bg rounded-lg transition-colors font-medium flex items-center gap-2">
              <svg className="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 4v16m8-8H4" />
              </svg>
              Add Node
            </button>
          </div>
        </div>

        {error && (
          <div className="mb-6 p-4 bg-danger/10 border border-danger/30 rounded-lg text-danger flex items-center gap-3">
            <svg className="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 8v4m0 4h.01M21 12a9 9 0 11-18 0 9 9 0 0118 0z" />
            </svg>
            {error}
          </div>
        )}

        <div className="bg-bg-secondary rounded-xl border border-bg-tertiary overflow-hidden">
          <div className="overflow-x-auto">
            <table className="w-full">
              <thead className="bg-bg-tertiary text-xs uppercase text-text-secondary font-medium">
                <tr>
                  <th className="px-6 py-4 text-left">Name</th>
                  <th className="px-6 py-4 text-left">Status</th>
                  <th className="px-6 py-4 text-left">Class</th>
                  <th className="px-6 py-4 text-left">Machine ID</th>
                  <th className="px-6 py-4 text-left">Last Seen</th>
                  <th className="px-6 py-4 text-left">Active</th>
                  <th className="px-6 py-4 text-right">Actions</th>
                </tr>
              </thead>
              <tbody className="divide-y divide-bg-tertiary">
                {nodes.map((node) => (
                  <tr key={node.id} className="hover:bg-bg-tertiary/50 transition-colors">
                    <td className="px-6 py-4">
                      <div className="flex items-center gap-3">
                        <div className="w-10 h-10 bg-bg rounded-lg flex items-center justify-center">
                          {node.machine_class === 'workstation' && (
                            <svg className="w-6 h-6 text-emerald" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9.75 17L9 20l-1 1h8l-1-1-.75-3M3 13h18M5 17h14a2 2 0 002-2V5a2 2 0 00-2-2H5a2 2 0 00-2 2v10a2 2 0 002 2z" />
                            </svg>
                          )}
                          {node.machine_class === 'server' && (
                            <svg className="w-6 h-6 text-emerald" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 12h14M5 12a2 2 0 01-2-2 2 2 0 00-2-2 2 2 0 00-2 2 2 2 0 012 2m14 0a2 2 0 002-2 2 2 0 012-2 2 2 0 01-2 2 2 2 0 00-2 2m-2 0a2 2 0 012-2 2 2 0 002-2 2 2 0 00-2 2 2 2 0 01-2 2" />
                            </svg>
                          )}
                          {node.machine_class === 'kiosk' && (
                            <svg className="w-6 h-6 text-emerald" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M9.75 17L9 20l-1 1h8l-1-1-.75-3M3 13h18M5 17h14a2 2 0 002-2V5a2 2 0 00-2-2H5a2 2 0 00-2 2v10a2 2 0 002 2z" />
                            </svg>
                          )}
                        </div>
                        <div>
                          <div className="font-medium text-text">{node.name}</div>
                          {node.description && (
                            <div className="text-xs text-text-secondary truncate w-48">{node.description}</div>
                          )}
                        </div>
                      </div>
                    </td>
                    <td className="px-6 py-4">
                      <span
                        className={`inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full text-xs font-medium border ${
                          getStatusColor(node.status)
                        }`}
                      >
                        {getStatusIcon(node.status)}
                        {node.status.charAt(0).toUpperCase() + node.status.slice(1)}
                      </span>
                    </td>
                    <td className="px-6 py-4">
                      <span className="px-2 py-1 bg-bg-tertiary text-text-secondary rounded text-xs font-medium capitalize">
                        {node.machine_class}
                      </span>
                    </td>
                    <td className="px-6 py-4 text-sm text-text-secondary font-mono truncate max-w-[120px]" title={node.machine_id}>
                      {node.machine_id?.substring(0, 8)}
                    </td>
                    <td className="px-6 py-4 text-sm text-text-secondary">
                      {node.last_seen ? new Date(node.last_seen).toLocaleString() : 'Never'}
                    </td>
                    <td className="px-6 py-4">
                      {node.active ? (
                        <div className="flex items-center gap-2 text-emerald">
                          <svg className="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 13l4 4L19 7" />
                          </svg>
                          <span className="text-sm font-medium">Active</span>
                        </div>
                      ) : (
                        <span className="text-sm text-text-secondary">Inactive</span>
                      )}
                    </td>
                    <td className="px-6 py-4 text-right">
                      <div className="flex items-center justify-end gap-2">
                        <Link
                          to={`/nodes/${node.id}`}
                          className="px-3 py-1.5 bg-bg-tertiary hover:bg-bg-secondary text-text-secondary rounded-lg text-sm font-medium transition-colors"
                        >
                          View
                        </Link>
                        <button className="p-1.5 hover:bg-bg-secondary text-text-secondary rounded-lg transition-colors">
                          <svg className="w-4 h-4" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15.232 5.232l3.536 3.536m-2.036-5.036a2.5 2.5 0 113.536 3.536L6.5 21.036H3v-3.572L16.732 3.732z" />
                          </svg>
                        </button>
                      </div>
                    </td>
                  </tr>
                ))}
                {nodes.length === 0 && (
                  <tr>
                    <td colSpan={7} className="px-6 py-12 text-center text-text-secondary">
                      <div className="flex flex-col items-center gap-3">
                        <div className="w-16 h-16 bg-bg-tertiary/50 rounded-full flex items-center justify-center">
                          <svg className="w-8 h-8 text-text-secondary/50" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 12h14M5 12a2 2 0 01-2-2 2 2 0 00-2-2 2 2 0 00-2 2 2 2 0 012 2m14 0a2 2 0 002-2 2 2 0 012-2 2 2 0 01-2 2 2 2 0 00-2 2m-2 0a2 2 0 012-2 2 2 0 002-2 2 2 0 00-2 2 2 2 0 01-2 2" />
                          </svg>
                        </div>
                        <p>No nodes registered yet</p>
                        <Link
                          to="/nodes/new"
                          className="text-emerald hover:text-emerald-light text-sm font-medium"
                        >
                          Register a new node
                        </Link>
                      </div>
                    </td>
                  </tr>
                )}
              </tbody>
            </table>
          </div>
          {nodes.length > 0 && (
            <div className="px-6 py-4 border-t border-bg-tertiary flex items-center justify-between text-xs text-text-secondary">
              <span>{nodes.length} node{nodes.length !== 1 && 's'} registered</span>
              {lastUpdated && (
                <span>Last updated: {new Date(lastUpdated).toLocaleTimeString()}</span>
              )}
            </div>
          )}
        </div>
      </div>
    </Layout>
  )
}
