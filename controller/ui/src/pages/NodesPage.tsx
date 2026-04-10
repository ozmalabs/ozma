import { useEffect } from 'react'
import { useNodes } from '../hooks/useNodes'
import { Link } from 'react-router-dom'

const NodesPage = () => {
  const { nodes, loading, error, refresh } = useNodes()
  
  useEffect(() => {
    refresh()
  }, [refresh])
  
  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-emerald-400"></div>
      </div>
    )
  }
  
  if (error) {
    return (
      <div className="p-6">
        <div className="bg-red-900/50 border border-red-500 rounded-lg p-4">
          <h2 className="text-red-300 font-semibold">Error loading nodes</h2>
          <p className="text-red-200 mt-2">{error}</p>
          <button
            onClick={refresh}
            className="mt-4 px-4 py-2 bg-red-700 hover:bg-red-600 rounded-lg text-white"
          >
            Retry
          </button>
        </div>
      </div>
    )
  }
  
  return (
    <div className="p-6">
      <div className="flex items-center justify-between mb-6">
        <h1 className="text-3xl font-bold text-emerald-400">Nodes</h1>
        <button
          onClick={refresh}
          disabled={loading}
          className="px-4 py-2 bg-emerald-600 hover:bg-emerald-500 rounded-lg text-white disabled:opacity-50 disabled:cursor-not-allowed"
        >
          {loading ? 'Refreshing...' : 'Refresh'}
        </button>
      </div>
      
      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
        {nodes.map((node) => (
          <Link
            key={node.id}
            to={`/nodes/${node.id}`}
            className="block"
          >
            <div className="bg-gray-800 hover:bg-gray-750 border border-gray-700 rounded-lg p-4 transition-colors">
              <div className="flex items-center justify-between mb-2">
                <h3 className="text-lg font-semibold text-white">{node.name}</h3>
                <span
                  className={`px-2 py-1 rounded-full text-xs font-medium ${
                    node.status === 'online'
                      ? 'bg-emerald-900/50 text-emerald-300 border border-emerald-700'
                      : node.status === 'offline'
                        ? 'bg-gray-900/50 text-gray-400 border border-gray-700'
                        : 'bg-amber-900/50 text-amber-300 border border-amber-700'
                  }`}
                >
                  {node.status}
                </span>
              </div>
              <div className="space-y-1 text-sm text-gray-400">
                <p>Hostname: {node.hostname}</p>
                <p>IP: {node.ip}</p>
                <p>Machine Class: <span className="text-gray-300">{node.machine_class}</span></p>
                {node.active && (
                  <span className="inline-flex items-center px-2 py-0.5 rounded text-xs font-medium bg-emerald-600 text-white">
                    Active
                  </span>
                )}
              </div>
            </div>
          </Link>
        ))}
        {nodes.length === 0 && (
          <div className="col-span-full text-center py-12 text-gray-500">
            No nodes found. Click Refresh to scan for nodes.
          </div>
        )}
      </div>
    </div>
  )
}

export default NodesPage
