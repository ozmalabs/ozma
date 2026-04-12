import { Link } from 'react-router-dom'
import { useAuth } from '../auth/AuthContext'

export default function DashboardPage() {
  const { user, logout } = useAuth()

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
        <Link to="/" className="rounded px-3 py-1.5 text-sm font-medium bg-muted">
          Dashboard
        </Link>
        <Link to="/nodes" className="rounded px-3 py-1.5 text-sm text-muted-foreground hover:bg-muted">
          Nodes
        </Link>
      </nav>

      <main className="p-6">
        <h2 className="mb-4 text-xl font-semibold">Dashboard</h2>
        <p className="text-muted-foreground">Welcome back, {user?.username}.</p>
      </main>
    </div>
  )
}
