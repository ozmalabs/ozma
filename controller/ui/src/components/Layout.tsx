import { ReactNode } from 'react'
import { Link, useLocation } from 'react-router-dom'
import { useNodes } from '../hooks/useNodes'

interface LayoutProps {
  children: ReactNode
}

export default function Layout({ children }: LayoutProps) {
  const location = useLocation()
  const { nodes, wsConnected } = useNodes()

  const navItems = [
    { path: '/nodes', label: 'Nodes', icon: 'grid' },
    { path: '/settings', label: 'Settings', icon: 'settings' },
  ]

  return (
    <div className="flex h-screen bg-background text-foreground">
      {/* Sidebar */}
      <div className="w-64 flex-shrink-0 border-r border-border bg-card">
        <div className="p-6">
          <div className="flex items-center gap-2">
            <div className="h-8 w-8 rounded bg-emerald-400" />
            <span className="text-xl font-bold">Ozma</span>
          </div>
          <p className="mt-2 text-sm text-muted-foreground">
            {wsConnected ? 'Connected to server' : 'Disconnected'}
          </p>
          <p className="text-sm text-muted-foreground">
            {nodes.length} node{nodes.length !== 1 ? 's' : ''}
          </p>
        </div>

        <nav className="px-2">
          {navItems.map((item) => (
            <Link
              key={item.path}
              to={item.path}
              className={`flex items-center gap-3 rounded-md px-4 py-3 text-sm font-medium transition-colors ${
                location.pathname === item.path
                  ? 'bg-emerald-400 text-white'
                  : 'text-foreground hover:bg-accent'
              }`}
            >
              <Icon name={item.icon} className="h-5 w-5" />
              {item.label}
            </Link>
          ))}
        </nav>
      </div>

      {/* Main Content */}
      <div className="flex flex-1 flex-col overflow-hidden">
        {/* Topbar */}
        <header className="flex h-16 items-center justify-between border-b border-border bg-card px-6">
          <h1 className="text-lg font-semibold">
            {location.pathname === '/nodes' ? 'Nodes' : 'Settings'}
          </h1>
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2 rounded-md border border-border px-3 py-1 text-sm">
              <div className="h-2 w-2 rounded-full bg-emerald-400" />
              <span>Online</span>
            </div>
            <button className="rounded-md px-4 py-2 text-sm font-medium hover:bg-accent">
              Logout
            </button>
          </div>
        </header>

        {/* Content */}
        <main className="flex-1 overflow-y-auto bg-background p-6">
          {children}
        </main>
      </div>
    </div>
  )
}

interface IconProps {
  name: string
  className?: string
}

function Icon({ name, className = 'h-6 w-6' }: IconProps) {
  const icons: Record<string, React.JSX.Element> = {
    grid: (
      <svg
        xmlns="http://www.w3.org/2000/svg"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
        className={className}
      >
        <rect x="3" y="3" width="7" height="7" />
        <rect x="14" y="3" width="7" height="7" />
        <rect x="14" y="14" width="7" height="7" />
        <rect x="3" y="14" width="7" height="7" />
      </svg>
    ),
    settings: (
      <svg
        xmlns="http://www.w3.org/2000/svg"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
        className={className}
      >
        <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.5a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.1a2 2 0 0 1-1-1.72v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
        <circle cx="12" cy="12" r="3" />
      </svg>
    ),
  }

  return icons[name] || null
}
