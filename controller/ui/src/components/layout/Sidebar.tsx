import { Link } from 'react-router-dom'
import { useNodesStore } from '../../store/nodesStore'

interface SidebarProps {
  activePath: string
}

export default function Sidebar({ activePath }: SidebarProps) {
  const { state } = useNodesStore()

  const navItems = [
    { path: '/', label: 'Nodes', icon: 'server' },
    { path: '/settings', label: 'Settings', icon: 'settings' },
  ]

  return (
    <aside className="w-64 flex-shrink-0 border-r bg-card hidden md:flex flex-col">
      <div className="p-6 border-b">
        <div className="flex items-center gap-2">
          <div className="w-8 h-8 rounded-lg bg-emerald-500 flex items-center justify-center">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              className="w-5 h-5 text-white"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
            >
              <rect x="2" y="2" width="20" height="8" rx="2" />
              <rect x="2" y="14" width="20" height="8" rx="2" />
              <circle cx="7" cy="6" r="1" fill="currentColor" />
              <circle cx="7" cy="18" r="1" fill="currentColor" />
            </svg>
          </div>
          <h1 className="text-xl font-bold">Ozma</h1>
        </div>
      </div>

      <nav className="flex-1 p-4 space-y-1">
        <div className="px-4 py-2 text-xs font-semibold text-muted-foreground uppercase tracking-wider">
          Navigation
        </div>

        {navItems.map((item) => {
          const isActive = activePath === item.path || (item.path === '/' && activePath === '/nodes')
          return (
            <Link
              key={item.path}
              to={item.path}
              className={`flex items-center gap-3 px-4 py-3 rounded-lg transition-colors ${
                isActive
                  ? 'bg-emerald-500/10 text-emerald-400'
                  : 'text-muted-foreground hover:bg-accent hover:text-accent-foreground'
              }`}
            >
              <NavItemIcon name={item.icon} />
              <span>{item.label}</span>
            </Link>
          )
        })}

        <div className="px-4 py-2 text-xs font-semibold text-muted-foreground uppercase tracking-wider mt-6">
          Resources
        </div>

        <div className="px-4 py-3 rounded-lg bg-secondary/50">
          <div className="text-sm font-medium">Total Nodes</div>
          <div className="text-2xl font-bold text-emerald-400">{state.nodes.length}</div>
        </div>
      </nav>

      <div className="p-4 border-t">
        <div className="px-4 py-2 text-xs font-semibold text-muted-foreground uppercase tracking-wider">
          Status
        </div>
        <div className="mt-2 flex items-center gap-2 px-4 py-2 rounded-lg bg-secondary/50">
          <div className="w-2 h-2 rounded-full bg-emerald-500 animate-pulse" />
          <span className="text-sm text-muted-foreground">Connected</span>
        </div>
      </div>
    </aside>
  )
}

function NavItemIcon({ name }: { name: string }) {
  switch (name) {
    case 'server':
      return (
        <svg
          xmlns="http://www.w3.org/2000/svg"
          className="w-5 h-5"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
        >
          <rect x="2" y="2" width="20" height="8" rx="2" />
          <rect x="2" y="14" width="20" height="8" rx="2" />
          <circle cx="7" cy="6" r="1" fill="currentColor" />
          <circle cx="7" cy="18" r="1" fill="currentColor" />
        </svg>
      )
    case 'settings':
      return (
        <svg
          xmlns="http://www.w3.org/2000/svg"
          className="w-5 h-5"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
        >
          <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.1a2 2 0 0 1-1-1.72v-.51a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
          <circle cx="12" cy="12" r="3" />
        </svg>
      )
    default:
      return null
  }
}
