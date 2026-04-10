import { ReactNode } from 'react'
import { Link, useLocation } from 'react-router-dom'

interface LayoutProps {
  children: ReactNode
}

export default function Layout({ children }: LayoutProps) {
  const location = useLocation()

  const navItems = [
    { path: '/', label: 'Nodes', icon: 'server' },
    { path: '/scenarios', label: 'Scenarios', icon: 'layers' },
    { path: '/settings', label: 'Settings', icon: 'settings' },
  ]

  return (
    <div className="flex h-screen overflow-hidden bg-[var(--background)]">
      {/* Sidebar */}
      <aside className="w-[var(--sidebar-width)] flex-shrink-0 border-r border-[var(--border-color)] bg-[var(--sidebar-background)] flex flex-col">
        <div className="p-6 border-b border-[var(--border-color)]">
          <h1 className="text-2xl font-bold text-[var(--accent-color)]">Ozma</h1>
          <p className="text-xs text-[var(--text-muted)] mt-1">KVMA Router</p>
        </div>

        <nav className="flex-1 overflow-y-auto py-4">
          <ul className="space-y-1">
            {navItems.map((item) => (
              <li key={item.path}>
                <Link
                  to={item.path}
                  className={`flex items-center gap-3 px-4 py-3 text-sm font-medium transition-colors rounded-lg mx-2 ${
                    location.pathname === item.path
                      ? 'bg-[var(--accent-color)]/10 text-[var(--accent-color)]'
                      : 'text-[var(--text-muted)] hover:bg-[var(--border-color)] hover:text-[var(--foreground)]'
                  }`}
                >
                  <Icon name={item.icon} className="w-5 h-5" />
                  {item.label}
                </Link>
              </li>
            ))}
          </ul>
        </nav>

        <div className="p-4 border-t border-[var(--border-color)]">
          <div className="flex items-center gap-3">
            <div className="w-8 h-8 rounded-full bg-[var(--border-color)] flex items-center justify-center text-xs font-bold">
              <Icon name="user" className="w-4 h-4" />
            </div>
            <div className="flex-1 min-w-0">
              <p className="text-sm font-medium truncate">Operator</p>
              <p className="text-xs text-[var(--text-muted)] truncate">admin</p>
            </div>
          </div>
        </div>
      </aside>

      {/* Main content */}
      <div className="flex-1 flex flex-col overflow-hidden">
        {/* Topbar */}
        <header className="h-[var(--topbar-height)] border-b border-[var(--border-color)] bg-[var(--sidebar-background)] flex items-center justify-between px-6">
          <h2 className="text-lg font-semibold text-[var(--foreground)]">
            {navItems.find((n) => n.path === location.pathname)?.label || 'Dashboard'}
          </h2>
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2 px-3 py-1 rounded-full bg-[var(--accent-color)]/10 text-[var(--accent-color)] text-xs">
              <span className="w-2 h-2 rounded-full bg-[var(--accent-color)] animate-pulse" />
              Connected
            </div>
          </div>
        </header>

        {/* Page content */}
        <main className="flex-1 overflow-y-auto p-6">
          {children}
        </main>
      </div>
    </div>
  )
}

function Icon({ name, className = 'w-6 h-6' }: { name: string; className?: string }) {
  const icons: Record<string, React.JSX.Element> = {
    server: (
      <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth={1.5} stroke="currentColor" className={className}>
        <path strokeLinecap="round" strokeLinejoin="round" d="M21.75 17.25v-.228a4.5 4.5 0 00-.12-1.03l-2.268-9.67A3.375 3.375 0 0015.838 6h-3.76a3.375 3.375 0 00-3.353 2.778l-2.268 9.67a4.5 4.5 0 00-.12 1.03v.228m19.5 0a3 3 0 01-3 3H5.25a3 3 0 01-3-3m19.5 0a3 3 0 00-3-3H5.25a3 3 0 00-3 3m16.5 0h.01v.01h-.01v-.01a3 3 0 00-3 3h-12a3 3 0 00-3-3h.01v-.01h.01v.01a3 3 0 003 3h12a3 3 0 003-3z" />
      </svg>
    ),
    layers: (
      <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth={1.5} stroke="currentColor" className={className}>
        <path strokeLinecap="round" strokeLinejoin="round" d="M6.429 9.75L2.25 12l4.179 2.25m0-4.5l5.571 3 5.571-3m0 4.5L5.571 15l5.571 3 5.571-3M12 3v7.5" />
      </svg>
    ),
    settings: (
      <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth={1.5} stroke="currentColor" className={className}>
        <path strokeLinecap="round" strokeLinejoin="round" d="M9.594 3.94c.09-.542.56-.94 1.11-.94h2.593c.55 0 1.02.398 1.11.94l.213 1.281c.063.374.313.729.742.966l1.026.604c.436.256.683.76.513 1.236l-.737 2.016c-.12.327.108.69.462.789l1.882.686c.525.192.88.687.84 1.216l-.46 3.156c-.068.463.46.824.906.619l2.763-1.302c.426-.201.806.043.92.48l.534 2.016c.12.455-.19.87-.655.96l-2.322.619a1.147 1.147 0 00-.608.866l-.533 2.016c-.06.227.06.47.28.594l2.016.737c.47.172.716.676.513 1.202l-1.302 2.764c-.205.436-.62.746-1.075.618l-3.156-.46c-.529-.077-.984.387-.984.916v.01c0 .529.46.993.984.916l3.156-.46c.455-.128.87.182.916.608l.533 2.016c.114.438-.03.906-.48.92l-2.016.534c-.22.06-.46-.06-.594-.28l-2.016-.737a1.147 1.147 0 00-.789.462l-1.882 2.016c-.237.505-.74.752-1.236.513l-1.026-.604a2.23 2.23 0 00-.966-.742l-1.281-.213a1.147 1.147 0 00-.92-.12l-2.593.213a1.147 1.147 0 00-.94.594l-1.302 2.764c-.192.426.043.906.48.92l2.016.534c.22.06.47.03.594-.28l.737-2.016a1.147 1.147 0 00.462-.789l.686-1.882c.172-.47.676-.716 1.202-.513l2.016.737c.227.082.47-.03.594-.28l.604-1.026a2.23 2.23 0 00.742-.966l1.281-.213c.542-.09.94-.56.94-1.11v-2.593c0-.55-.398-1.02-.94-1.11l-1.281-.213a2.23 2.23 0 00-.966-.742l-2.016-.737a1.147 1.147 0 00-.594-.28l-2.016-.534a1.147 1.147 0 00-.92-.48l-2.764 1.302c-.436.205-.906-.043-.92-.48l-.533-2.016a1.147 1.147 0 00.462-.789l.737-2.016c.082-.227-.03-.47-.28-.594l-2.016-.737c-.47-.172-.716-.676-.513-1.202l1.302-2.764c.205-.436-.043-.906-.48-.92l-2.016-.534c-.22-.06-.47-.03-.594.28l-.737 2.016a1.147 1.147 0 00-.462.789l-.686 1.882a2.23 2.23 0 00-.742.966l-.213 1.281c-.09.542.398 1.02.94 1.11h2.593c.55 0 1.02-.398 1.11-.94l.213-1.281c.063-.374.313-.729.742-.966l1.026-.604c.436-.256.683-.76.513-1.236l-.737-2.016a1.147 1.147 0 00-.462-.789l-1.882-.686c-.525-.192-.88-.687-.84-1.216l.46-3.156c.068-.463-.46-.824-.906-.619l-2.763 1.302c-.426.201-.806-.043-.92-.48l-.534-2.016c-.12-.455.19-.87.655-.96l2.322-.619a1.147 1.147 0 00.608-.866l.533-2.016c.06-.227-.06-.47-.28-.594l-2.016-.737c-.47-.172-.716-.676-.513-1.202l1.302-2.764c.205-.436.62-.746 1.075-.618l3.156.46c.529.077.984-.387.984-.916v-.01c0-.529-.46-.993-.984-.916z" />
      </svg>
    ),
    user: (
      <svg xmlns="http://www.w3.org/2000/svg" fill="none" viewBox="0 0 24 24" strokeWidth={1.5} stroke="currentColor" className={className}>
        <path strokeLinecap="round" strokeLinejoin="round" d="M15.75 6a3.75 3.75 0 11-7.5 0 3.75 3.75 0 017.5 0zM4.501 20.118a7.5 7.5 0 0114.998 0A17.933 17.933 0 0112 21.75c-2.676 0-5.216-.584-7.499-1.632zM9.75 11.25a2.25 2.25 0 114.5 0 2.25 2.25 0 01-4.5 0z" />
      </svg>
    ),
  }

  return icons[name] || icons.server
}
