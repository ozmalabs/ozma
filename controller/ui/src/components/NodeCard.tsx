import { Link } from 'react-router-dom'
import { Node } from '../types'

interface NodeCardProps {
  node: Node
}

export default function NodeCard({ node }: NodeCardProps) {
  const statusColors = {
    connected: 'bg-emerald-500',
    disconnected: 'bg-slate-500',
    error: 'bg-red-500',
  }

  const statusLabels = {
    connected: 'Connected',
    disconnected: 'Disconnected',
    error: 'Error',
  }

  return (
    <Link
      to={`/nodes/${node.id}`}
      className="block bg-card rounded-lg border p-6 hover:border-emerald-500/50 transition-all hover:shadow-lg hover:shadow-emerald-500/10"
    >
      <div className="flex items-start justify-between mb-4">
        <div>
          <h3 className="text-lg font-semibold flex items-center gap-2">
            {node.name}
            {node.active && (
              <span className="px-2 py-0.5 text-xs font-medium bg-emerald-500/10 text-emerald-400 rounded-full">
                Active
              </span>
            )}
          </h3>
          <p className="text-sm text-muted-foreground mt-1">{node.address}</p>
        </div>
        <div
          className={`w-3 h-3 rounded-full ${statusColors[node.status]} ${
            node.status === 'connected' ? 'animate-pulse' : ''
          }`}
          title={statusLabels[node.status]}
        />
      </div>

      <div className="grid grid-cols-2 gap-3 text-sm">
        <div>
          <span className="text-muted-foreground">Machine Class</span>
          <div className="font-medium">{node.machine_class}</div>
        </div>
        <div>
          <span className="text-muted-foreground">Last Seen</span>
          <div className="font-medium">
            {new Date(node.last_seen).toLocaleString()}
          </div>
        </div>
        <div className="col-span-2">
          <span className="text-muted-foreground">Metadata</span>
          <div className="font-medium truncate">{node.metadata.hostname}</div>
        </div>
      </div>
    </Link>
  )
}
