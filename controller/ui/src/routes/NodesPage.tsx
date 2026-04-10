import { useMemo } from 'react'
import { useNodes } from '../hooks/useNodes'
import NodeCard from '../components/NodeCard'

export default function NodesPage() {
  const { nodes, loading, error, wsConnected, refresh } = useNodes()

  const filteredNodes = useMemo(() => {
    if (!nodes) return []
    return [...nodes].sort((a, b) => {
      if (a.active && !b.active) return -1
      if (!a.active && b.active) return 1
      return a.name.localeCompare(b.name)
    })
  }, [nodes])

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="flex flex-col items-center gap-3">
          <div className="w-8 h-8 border-4 border-emerald-500 border-t-transparent rounded-full animate-spin" />
          <p className="text-sm text-slate-500">Loading nodes...</p>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-64 bg-red-500/10 rounded-xl border border-red-500/20">
        <div className="flex flex-col items-center gap-2 p-6">
          <svg className="w-8 h-8 text-red-500" fill="none" stroke="currentColor" viewBox="0 0 24 24">
            <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M12 9v2m0 4h.01m-6.938 4h13.856c1.54 0 2.502-1.667 1.732-3L13.732 9c-.77-1.333-2.694-1.333-3.464 0L3.34 16c-.77 1.333.192 3 1.732 3z" />
          </svg>
          <p className="text-red-500 font-medium">Failed to load nodes</p>
          <p className="text-sm text-red-500/70">{error}</p>
          <button
            onClick={refresh}
            className="mt-2 px-4 py-2 bg-emerald-600 hover:bg-emerald-500 text-white rounded-lg transition-colors text-sm font-medium"
          >
            Retry
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-slate-100">Nodes</h1>
          <p className="text-slate-400 text-sm mt-1">
            {nodes.length} node{nodes.length !== 1 && 's'} connected
          </p>
        </div>
        <div className="flex items-center gap-3">
          <div className="flex items-center gap-2 px-3 py-1.5 rounded-full bg-slate-900 border border-slate-800">
            <div
              className={`w-2 h-2 rounded-full ${
                wsConnected ? 'bg-emerald-500 animate-pulse' : 'bg-amber-500 animate-pulse'
              }`}
            />
            <span className="text-xs font-medium text-slate-300">
              {wsConnected ? 'Live Updates' : 'Reconnecting...'}
            </span>
          </div>
          <button
            onClick={refresh}
            className="p-2 rounded-lg bg-slate-900 border border-slate-800 hover:bg-slate-800 hover:text-emerald-400 text-slate-400 transition-colors"
          >
            <svg className="w-5 h-5" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M4 4v5h.582m15.356 2A8.001 8.001 0 004.582 9m0 0H9m11 11v-5h-.581m0 0a8.003 8.003 0 01-15.357-2m15.357 2H15" />
            </svg>
          </button>
        </div>
      </div>

      {filteredNodes.length === 0 ? (
        <div className="flex flex-col items-center justify-center py-20 text-center">
          <div className="w-16 h-16 rounded-2xl bg-slate-900 border border-slate-800 flex items-center justify-center mb-4">
            <svg className="w-8 h-8 text-slate-600" fill="none" stroke="currentColor" viewBox="0 0 24 24">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 12h14M5 12a2 2 0 01-2-2 2 2 0 00-2 2 2 2 0 002 2 2 2 0 012 2v2a2 2 0 01-2 2 2 2 0 00-2-2 2 2 0 002 2 2 2 0 012-2h14a2 2 0 012 2 2 2 0 002-2 2 2 0 00-2-2 2 2 0 01-2-2v-2a2 2 0 012-2 2 2 0 002 2 2 2 0 00-2-2 2 2 0 01-2 2H5z" />
            </svg>
          </div>
          <h3 className="text-lg font-medium text-slate-300">No nodes found</h3>
          <p className="text-slate-500 mt-1 max-w-sm">
            Nodes will appear here once they register with the controller.
          </p>
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 xl:grid-cols-4 gap-4">
          {filteredNodes.map((node) => (
            <NodeCard key={node.id} node={node} />
          ))}
        </div>
      )}
    </div>
  )
}
