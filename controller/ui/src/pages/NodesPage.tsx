import { useMemo } from 'react'
import { useNodes } from '../hooks/useNodes'
import { NodeInfo } from '../types/node'

function statusColor(status: NodeInfo['status']) {
  switch (status) {
    case 'online':
      return 'bg-emerald-400'
    case 'offline':
      return 'bg-red-500'
    case 'connecting':
      return 'bg-yellow-400'
    default:
      return 'bg-gray-400'
  }
}

function machineClassColor(machineClass: NodeInfo['machine_class']) {
  switch (machineClass) {
    case 'workstation':
      return 'bg-blue-500'
    case 'server':
      return 'bg-purple-500'
    case 'kiosk':
      return 'bg-orange-500'
    default:
      return 'bg-gray-500'
  }
}

export default function NodesPage() {
  const { nodes, loading, error, wsConnected } = useNodes()

  const sortedNodes = useMemo(() => {
    return [...nodes].sort((a, b) => {
      if (a.active && !b.active) return -1
      if (!a.active && b.active) return 1
      return a.name.localeCompare(b.name)
    })
  }, [nodes])

  if (loading) {
    return (
      <div className="flex h-full items-center justify-center">
        <div className="animate-spin rounded-full border-4 border-emerald-400 border-t-transparent h-12 w-12" />
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex h-full flex-col items-center justify-center text-center">
        <div className="rounded-lg bg-red-500/10 px-6 py-4">
          <p className="text-red-500">Error: {error}</p>
          <button
            onClick={() => window.location.reload()}
            className="mt-4 rounded bg-red-500 px-4 py-2 text-white hover:bg-red-600"
          >
            Retry
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="max-w-6xl mx-auto">
      <div className="mb-6 flex items-center justify-between">
        <div>
          <h2 className="text-2xl font-bold">Nodes</h2>
          <p className="text-muted-foreground">
            {wsConnected ? 'Live updates enabled' : 'Disconnected from server'}
          </p>
        </div>
        <div className="flex gap-2">
          <button className="rounded-md bg-emerald-400 px-4 py-2 font-medium text-white hover:bg-emerald-500">
            Add Node
          </button>
          <button className="rounded-md bg-accent px-4 py-2 font-medium hover:bg-accent/90">
            Refresh
          </button>
        </div>
      </div>

      <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
        {sortedNodes.map((node) => (
          <div
            key={node.id}
            className="rounded-lg border border-border bg-card p-6 transition-shadow hover:shadow-lg"
          >
            <div className="flex items-start justify-between">
              <div className="flex items-center gap-3">
                <div className="h-12 w-12 rounded-full bg-emerald-400/20 flex items-center justify-center">
                  <span className="text-lg font-bold text-emerald-400">
                    {node.name.charAt(0).toUpperCase()}
                  </span>
                </div>
                <div>
                  <h3 className="font-semibold">{node.name}</h3>
                  <div className="flex items-center gap-2 text-sm text-muted-foreground">
                    <span className={statusColor(node.status) + ' h-2 w-2 rounded-full'} />
                    {node.status}
                    {node.active && (
                      <>
                        <span className="text-muted-foreground">•</span>
                        <span className="text-emerald-400">Active</span>
                      </>
                    )}
                  </div>
                </div>
              </div>
              <span className={`rounded-full px-2 py-1 text-xs font-medium text-white ${machineClassColor(node.machine_class)}`}>
                {node.machine_class}
              </span>
            </div>

            <div className="mt-4 space-y-2 text-sm text-muted-foreground">
              <div className="flex justify-between">
                <span>ID</span>
                <code className="text-xs bg-accent px-1 py-0.5 rounded">{node.id.slice(0, 8)}...</code>
              </div>
              <div className="flex justify-between">
                <span>Address</span>
                <span className="font-mono">{node.address}</span>
              </div>
              {node.hostname && (
                <div className="flex justify-between">
                  <span>Hostname</span>
                  <span className="font-mono">{node.hostname}</span>
                </div>
              )}
            </div>

            <div className="mt-4 flex gap-2">
              <button className="flex-1 rounded-md bg-emerald-400 px-3 py-2 text-sm font-medium text-white hover:bg-emerald-500">
                Connect
              </button>
              <button className="rounded-md bg-accent px-3 py-2 text-sm font-medium hover:bg-accent/90">
                Details
              </button>
            </div>
          </div>
        ))}
      </div>

      {sortedNodes.length === 0 && (
        <div className="flex flex-col items-center justify-center py-16 text-center">
          <div className="mb-4 rounded-full bg-accent/20 p-8">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="1"
              strokeLinecap="round"
              strokeLinejoin="round"
              className="h-16 w-16 text-muted-foreground"
            >
              <rect x="3" y="3" width="7" height="7" />
              <rect x="14" y="3" width="7" height="7" />
              <rect x="14" y="14" width="7" height="7" />
              <rect x="3" y="14" width="7" height="7" />
            </svg>
          </div>
          <h3 className="text-lg font-semibold">No nodes found</h3>
          <p className="mt-1 text-muted-foreground">
            {wsConnected ? 'Add a new node to get started.' : 'Waiting for server connection...'}
          </p>
        </div>
      )}
    </div>
  )
}
