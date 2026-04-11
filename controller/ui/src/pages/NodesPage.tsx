import { useNodes } from '../store/useNodesStore'
import { Link } from 'react-router-dom'
import { StatusDot } from '../components/StatusDot'

export default function NodesPage() {
  const { nodes, loading, error } = useNodes()

  if (loading) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center">
          <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin mx-auto mb-4"></div>
          <p className="text-muted-foreground">Loading nodes...</p>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="flex items-center justify-center h-full">
        <div className="text-center max-w-md">
          <div className="text-destructive mb-4">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="48"
              height="48"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <circle cx="12" cy="12" r="10" />
              <line x1="15" x2="9" y1="9" y2="15" />
              <line x1="9" x2="15" y1="9" y2="15" />
            </svg>
          </div>
          <h3 className="text-xl font-semibold mb-2">Failed to load nodes</h3>
          <p className="text-muted-foreground mb-6">{error}</p>
          <button
            onClick={() => window.location.reload()}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors"
          >
            Retry
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="max-w-6xl mx-auto">
      <div className="flex justify-between items-center mb-6">
        <div>
          <h2 className="text-2xl font-bold">Nodes</h2>
          <p className="text-muted-foreground">Manage your connected nodes</p>
        </div>
        <div className="flex gap-2">
          <button className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors flex items-center gap-2">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="16"
              height="16"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <path d="M5 12h14" />
              <path d="M12 5v14" />
            </svg>
            Add Node
          </button>
        </div>
      </div>

      {nodes.length === 0 ? (
        <div className="text-center py-12 border-2 border-dashed border-border rounded-xl">
          <div className="text-muted-foreground mb-4">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="48"
              height="48"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            >
              <rect width="18" height="15" x="3" y="4" rx="2" ry="2" />
              <line x1="2" x2="22" y1="20" y2="20" />
              <line x1="4" x2="8" y1="20" y2="20" />
            </svg>
          </div>
          <h3 className="text-lg font-semibold mb-2">No nodes found</h3>
          <p className="text-muted-foreground mb-6">
            Get started by adding a new node to your controller.
          </p>
          <button className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors">
            Add First Node
          </button>
        </div>
      ) : (
        <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
          {nodes.map((node) => (
            <div key={node.id} className="bg-card rounded-xl border p-5 hover:border-primary/50 transition-all group">
              <div className="flex justify-between items-start mb-4">
                <div className="flex items-center gap-3">
                  <StatusDot status={node.status === 'online' ? 'online' : node.status === 'offline' ? 'offline' : 'connecting'} />
                  <h3 className="font-semibold text-lg">{node.name}</h3>
                </div>
                {node.active && (
                  <span className="px-2 py-1 text-xs font-medium bg-primary text-primary-foreground rounded-full">
                    Active
                  </span>
                )}
              </div>

              <div className="space-y-2 text-sm">
                <div className="flex justify-between">
                  <span className="text-muted-foreground">Hostname</span>
                  <span className="font-mono">{node.hostname || 'N/A'}</span>
                </div>
                <div className="flex justify-between">
                  <span className="text-muted-foreground">IP Address</span>
                  <span className="font-mono">{node.ip_address || 'N/A'}</span>
                </div>
                <div className="flex justify-between">
                  <span className="text-muted-foreground">Machine Class</span>
                  <span className="capitalize">{node.machine_class || 'workstation'}</span>
                </div>
                <div className="flex justify-between">
                  <span className="text-muted-foreground">Last Seen</span>
                  <span className="text-muted-foreground">{formatDate(node.last_seen)}</span>
                </div>
              </div>

              <div className="mt-4 pt-4 border-t flex gap-2">
                <Link
                  to={`/nodes/${node.id}`}
                  className="flex-1 px-3 py-2 text-sm font-medium bg-secondary text-foreground rounded-lg hover:bg-secondary/80 transition-colors"
                >
                  View Details
                </Link>
                <button className="px-3 py-2 text-sm font-medium bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors">
                  Remote Desktop
                </button>
              </div>
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

function formatDate(dateString: string): string {
  const date = new Date(dateString)
  return date.toLocaleString()
}
