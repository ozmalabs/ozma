import { useEffect, useState } from 'react'
import { Link } from 'react-router-dom'
import { useAuth } from '../auth/AuthContext'
import { api } from '../api/client'
import type { NodeInfo } from '../types/api'

export default function NodesPage() {
  const { user, logout } = useAuth()
  const [nodes, setNodes] = useState<NodeInfo[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    api.nodes
      .list()
      .then((res) => setNodes(res.nodes))
      .catch((err) => setError(err instanceof Error ? err.message : 'Failed to load nodes'))
      .finally(() => setLoading(false))
  }, [])

  return (
    <div className="min-h-screen bg-background text-foreground">
      <header className="flex items-center justify-between border-b border-border px-6 py-4">
        <h1 className="text-lg font-semibold">Ozma Controller</h1>
        <div className="flex items-center gap-4">
          <span className="text-sm text-muted-foreground">{user?.username}</span>
          <button
            onClick={logout}
            className="rounded-md border border-border px-3 py-1.5 text-sm hover:bg-muted"
          >
            Sign out
          </button>
        </div>
      </header>

      <nav className="flex gap-2 border-b border-border px-6 py-2">
        <Link to="/" className="rounded px-3 py-1.5 text-sm text-muted-foreground hover:bg-muted">
          Dashboard
        </Link>
        <Link to="/nodes" className="rounded px-3 py-1.5 text-sm font-medium bg-muted">
          Nodes
        </Link>
      </nav>

      <main className="p-6">
        <h2 className="mb-4 text-xl font-semibold">Nodes</h2>

        {loading && <p className="text-muted-foreground">Loading…</p>}
        {error && (
          <p className="rounded-md bg-destructive/10 px-3 py-2 text-sm text-destructive">{error}</p>
        )}

        {!loading && !error && nodes.length === 0 && (
          <p className="text-muted-foreground">No nodes registered.</p>
        )}

        <ul className="space-y-2">
          {nodes.map((node) => (
            <li
              key={node.id}
              className="flex items-center justify-between rounded-lg border border-border bg-card px-4 py-3"
            >
              <div>
                <p className="font-medium">{node.name}</p>
                {node.ip && <p className="text-sm text-muted-foreground">{node.ip}</p>}
              </div>
              <span
                className={`rounded-full px-2 py-0.5 text-xs font-medium ${
                  node.status === 'online'
                    ? 'bg-green-100 text-green-700 dark:bg-green-900/30 dark:text-green-400'
                    : node.status === 'connecting'
                      ? 'bg-yellow-100 text-yellow-700 dark:bg-yellow-900/30 dark:text-yellow-400'
                      : 'bg-muted text-muted-foreground'
                }`}
              >
                {node.status}
              </span>
            </li>
          ))}
        </ul>
      </main>
    </div>
  )
}
