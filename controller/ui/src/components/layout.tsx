import { ReactNode, useState } from 'react'
import { Link, useLocation } from 'react-router-dom'

interface LayoutProps {
  children: ReactNode
}

export function Layout({ children }: LayoutProps) {
  const location = useLocation()
  const [isSidebarOpen, setIsSidebarOpen] = useState(true)

  const navItems = [
    { name: 'Nodes', path: '/nodes', icon: '💻' },
  ]

  return (
    <div className="flex h-screen overflow-hidden bg-bg">
      {/* Sidebar */}
      <aside
        className={`${
          isSidebarOpen ? 'w-64' : 'w-20'
        } transition-all duration-300 ease-in-out flex-shrink-0 bg-bg-surface border-r border-border flex flex-col`}
      >
        <div className="h-16 flex items-center justify-center border-b border-border">
          {isSidebarOpen ? (
            <div className="flex items-center gap-2 text-emerald-600 font-bold text-xl">
              <span className="text-2xl">⚡</span>
              <span>Ozma</span>
            </div>
          ) : (
            <span className="text-emerald-600 text-xl">⚡</span>
          )}
        </div>

        <nav className="flex-1 py-4 overflow-y-auto">
          <ul className="space-y-1">
            {navItems.map((item) => (
              <li key={item.path}>
                <Link
                  to={item.path}
                  className={`flex items-center px-4 py-3 text-sm font-medium transition-colors rounded-lg mx-2 ${
                    location.pathname === item.path
                      ? 'bg-emerald-600/10 text-emerald-600'
                      : 'text-text-secondary hover:bg-bg-surface-light hover:text-text'
                  }`}
                  title={!isSidebarOpen ? item.name : ''}
                >
                  <span className="text-lg">{item.icon}</span>
                  {isSidebarOpen && <span className="ml-3">{item.name}</span>}
                </Link>
              </li>
            ))}
          </ul>
        </nav>

        <div className="p-4 border-t border-border">
          <button
            onClick={() => setIsSidebarOpen(!isSidebarOpen)}
            className="flex items-center justify-center w-full p-2 rounded-lg text-text-secondary hover:bg-bg-surface-light hover:text-text transition-colors"
          >
            <span className="text-xl">«</span>
          </button>
        </div>
      </aside>

      {/* Main Content */}
      <div className="flex-1 flex flex-col overflow-hidden">
        {/* Topbar */}
        <header className="h-16 flex items-center justify-between px-6 border-b border-border bg-bg">
          <h1 className="text-xl font-semibold text-text">
            {location.pathname === '/nodes' ? 'Nodes' : 'Dashboard'}
          </h1>

          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2 px-3 py-1.5 rounded-full bg-bg-surface-light border border-border">
              <span className="w-2 h-2 rounded-full bg-emerald-600 animate-pulse"></span>
              <span className="text-xs text-text-secondary">Controller</span>
            </div>
            <div className="flex items-center gap-2 px-3 py-1.5 rounded-full bg-bg-surface-light border border-border">
              <span className="text-sm text-text">admin</span>
            </div>
          </div>
        </header>

        {/* Content Area */}
        <main className="flex-1 overflow-auto p-6">{children}</main>
      </div>
    </div>
  )
}
