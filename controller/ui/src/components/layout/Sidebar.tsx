import { Link, useLocation } from 'react-router-dom'
import { House, Server, Settings } from 'lucide-react'

const navItems = [
  { icon: House, label: 'Nodes', path: '/nodes' },
  { icon: Server, label: 'Scenarios', path: '/scenarios' },
  { icon: Settings, label: 'Settings', path: '/settings' },
]

export default function Sidebar() {
  const location = useLocation()

  return (
    <aside className="w-64 bg-bg-sidebar border-r border-border flex flex-col">
      <div className="p-6 border-b border-border">
        <div className="flex items-center gap-2 text-emerald-400">
          <svg className="w-8 h-8" fill="none" viewBox="0 0 24 24" stroke="currentColor">
            <path
              strokeLinecap="round"
              strokeLinejoin="round"
              strokeWidth={2}
              d="M13 10V3L4 14h7v7l9-11h-7z"
            />
          </svg>
          <span className="text-xl font-bold tracking-tight">Ozma</span>
        </div>
      </div>
      <nav className="flex-1 p-4 space-y-1">
        {navItems.map((item) => {
          const isActive = location.pathname === item.path
          return (
            <Link
              key={item.path}
              to={item.path}
              className={`flex items-center gap-3 px-4 py-3 rounded-lg transition-colors ${
                isActive
                  ? 'bg-emerald-500/10 text-emerald-400'
                  : 'text-text-muted hover:bg-bg-card hover:text-text'
              }`}
            >
              <item.icon className="w-5 h-5" />
              <span>{item.label}</span>
            </Link>
          )
        })}
      </nav>
      <div className="p-4 border-t border-border">
        <div className="px-4 py-3 bg-bg-card rounded-lg">
          <div className="text-xs text-text-muted mb-1">Controller</div>
          <div className="text-sm font-medium text-text">Localhost:7380</div>
          <div className="flex items-center gap-2 mt-2 text-xs text-emerald-400">
            <span className="relative flex h-2 w-2">
              <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-emerald-400 opacity-75"></span>
              <span className="relative inline-flex rounded-full h-2 w-2 bg-emerald-500"></span>
            </span>
            <span>Connected</span>
          </div>
        </div>
      </div>
    </aside>
  )
}
