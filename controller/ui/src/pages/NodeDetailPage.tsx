import { useEffect, useState } from 'react'
import { useParams, Link, useNavigate } from 'react-router-dom'
import { api } from '../api/client'
import { NodeInfo } from '../types/api'

export default function NodeDetailPage() {
  const { id } = useParams<{ id: string }>()
  const navigate = useNavigate()
  const [node, setNode] = useState<NodeInfo | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!id) return
    let cancelled = false
    setLoading(true)
    api.nodes
      .get(id)
      .then((res) => {
        if (!cancelled && res.node) setNode(res.node as NodeInfo)
      })
      .catch((err) => {
        if (!cancelled) setError(err instanceof Error ? err.message : 'Failed to load node')
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [id])

  if (loading) {
    return (
      <div className="flex items-center justify-center h-64">
        <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin" />
      </div>
    )
  }

  if (error || !node) {
    return (
      <div className="p-6">
        <Link to="/nodes" className="text-sm text-primary hover:underline mb-4 inline-block">
          ← Back to Nodes
        </Link>
        <div className="rounded-lg bg-destructive/10 border border-destructive/20 p-4 text-destructive">
          {error ?? 'Node not found.'}
        </div>
      </div>
    )
  }

  return (
    <div className="p-6 max-w-3xl">
      <button
        onClick={() => navigate('/nodes')}
        className="text-sm text-primary hover:underline mb-6 inline-block"
      >
        ← Back to Nodes
      </button>

      <h1 className="text-2xl font-bold mb-1">{node.name}</h1>
      <p className="text-muted-foreground font-mono mb-6">{node.host}</p>

      <div className="rounded-xl border bg-card p-6 space-y-3 text-sm">
        <Row label="ID" value={node.id} mono />
        <Row label="Host" value={node.host} mono />
        <Row label="Machine Class" value={node.machine_class} capitalize />
        <Row label="Status" value={node.online ? 'Online' : 'Offline'} />
        <Row label="Last Seen" value={node.last_seen ? new Date(node.last_seen).toLocaleString() : 'Never'} />
        {node.tags.length > 0 && (
          <div className="flex justify-between">
            <span className="text-muted-foreground">Tags</span>
            <span className="flex gap-1 flex-wrap justify-end">
              {node.tags.map((tag) => (
                <span key={tag} className="px-2 py-0.5 bg-secondary rounded text-xs">{tag}</span>
              ))}
            </span>
          </div>
        )}
      </div>
    </div>
  )
}

function Row({
  label,
  value,
  mono,
  capitalize,
}: {
  label: string
  value: string
  mono?: boolean
  capitalize?: boolean
}) {
  return (
    <div className="flex justify-between">
      <span className="text-muted-foreground">{label}</span>
      <span className={`${mono ? 'font-mono' : ''} ${capitalize ? 'capitalize' : ''}`}>{value}</span>
    </div>
  )
}
