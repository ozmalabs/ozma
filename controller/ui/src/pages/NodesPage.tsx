import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { api } from '../api/client'
import { NodeInfo } from '../types/api'

function useNodes() {
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    api.nodes
      .list()
      .then((res) => {
        if (!cancelled) setNodes(res.nodes)
      })
      .catch((err) => {
        if (!cancelled) setError(err instanceof Error ? err.message : 'Failed to load nodes')
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  return { nodes, loading, error }
}

export default function NodesPage() {
  const { nodes, loading, error } = useNodes()

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin" />
      </div>
    )
  }

  if (error) {
    return (
      <div className="p-6">
        <div className="rounded-lg bg-destructive/10 border border-destructive/20 p-4 text-destructive">
          {error}
        </div>
      </div>
    )
  }

  return (
    <div className="p-6">
      <h1 className="text-2xl font-bold mb-6">Nodes</h1>
      {nodes.length === 0 ? (
        <p className="text-muted-foreground">No nodes found.</p>
      ) : (
        <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {nodes.map((node) => (
            <NodeCard key={node.id} node={node} />
          ))}
        </div>
      )}
    </div>
  )
}

function NodeCard({ node }: { node: NodeInfo }) {
  return (
    <div className="rounded-xl border bg-card p-4 shadow-sm flex flex-col gap-2 hover:border-primary/50 transition-colors">
      <div className="flex items-center justify-between">
        <span className="font-semibold">{node.name}</span>
        <span
          className={`text-xs px-2 py-0.5 rounded-full font-medium ${
            node.online ? 'bg-green-500/10 text-green-400' : 'bg-muted text-muted-foreground'
          }`}
        >
          {node.online ? 'Online' : 'Offline'}
        </span>
      </div>
      <p className="text-sm text-muted-foreground font-mono">{node.host}</p>
      <p className="text-xs text-muted-foreground capitalize">{node.machine_class}</p>
      <div className="mt-2 pt-2 border-t">
        <Link
          to={`/nodes/${node.id}`}
          className="text-sm text-primary hover:underline"
        >
          View Details →
        </Link>
      </div>
    </div>
  )
}
