import React from 'react'
import { Link, useLocation } from 'react-router-dom'

interface LayoutProps {
  children: React.ReactNode
}

const Layout: React.FC<LayoutProps> = ({ children }) => {
  const location = useLocation()

  const navItems = [
    { path: '/nodes', label: 'Nodes', icon: '🖥️' },
    { path: '/scenarios', label: 'Scenarios', icon: '🎬' },
    { path: '/streams', label: 'Streams', icon: '📡' },
    { path: '/settings', label: 'Settings', icon: '⚙️' },
  ]

  return (
    <div className="min-h-screen bg-bg-primary flex flex-col">
      {/* Topbar */}
      <header className="bg-bg-secondary border-b border-border-color px-6 py-3 flex items-center justify-between">
        <div className="flex items-center gap-3">
          <div className="text-accent-emerald text-2xl font-bold">ozma</div>
          <span className="text-text-secondary text-sm">KVMA Router</span>
        </div>
        <div className="flex items-center gap-4">
          <div className="flex items-center gap-2 text-text-secondary text-sm">
            <span className="w-2 h-2 rounded-full bg-accent-emerald animate-pulse"></span>
            <span>Connected</span>
          </div>
        </div>
      </header>

      <div className="flex flex-1 overflow-hidden">
        {/* Sidebar */}
        <aside className="w-64 bg-bg-secondary border-r border-border-color flex flex-col">
          <nav className="flex-1 p-4 space-y-1 overflow-y-auto">
            {navItems.map((item) => (
              <Link
                key={item.path}
                to={item.path}
                className={`flex items-center gap-3 px-4 py-3 rounded-lg transition-colors ${
                  location.pathname === item.path
                    ? 'bg-bg-tertiary text-accent-emerald'
                    : 'text-text-secondary hover:bg-bg-tertiary hover:text-text-primary'
                }`}
              >
                <span className="text-lg">{item.icon}</span>
                <span>{item.label}</span>
              </Link>
            ))}
          </nav>
          <div className="p-4 border-t border-border-color">
            <div className="text-xs text-text-muted">
              <p className="mb-1">Controller</p>
              <p className="font-mono text-xs">v0.1.0</p>
            </div>
          </div>
        </aside>

        {/* Main Content */}
        <main className="flex-1 overflow-auto bg-bg-primary p-6">
          {children}
        </main>
      </div>
    </div>
  )
}

export default Layout
