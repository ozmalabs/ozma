import { ReactNode } from 'react'
import { Link, useLocation } from 'react-router-dom'

interface LayoutProps {
  children: ReactNode
}

export function Layout({ children }: LayoutProps) {
  const location = useLocation()

  const navItems = [
    { path: '/nodes', label: 'Nodes', icon: 'servers' },
    { path: '/scenarios', label: 'Scenarios', icon: 'layers' },
    { path: '/stream', label: 'Stream', icon: 'video' },
    { path: '/settings', label: 'Settings', icon: 'settings' },
  ]

  return (
    <div className="flex h-screen bg-bg">
      {/* Sidebar */}
      <aside className="w-sidebar-width flex-shrink-0 bg-bg-secondary flex flex-col border-r border-bg-tertiary">
        <div className="p-6 border-b border-bg-tertiary">
          <h1 className="text-2xl font-bold text-emerald">
            <svg className="w-8 h-8 inline-block mr-2" fill="none" viewBox="0 0 24 24" stroke="currentColor">
              <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M14.752 11.168l-3.197-2.132A1.998 1.998 0 0010.5 13.5h-3.25a2 2 0 00-1.733 2.998l3.2 3.2A1.998 1.998 0 0010.5 21.5h3.25a2 2 0 001.733-2.998l-3.197-2.132A1.998 1.998 0 0010.5 15.5h3.25a2 2 0 001.733-2.998l-3.2-3.2A1.998 1.998 0 0013.75 9.5h-3.25a2 2 0 00-1.733 2.998l3.2 3.2z" />
            </svg>
            Ozma
          </h1>
          <p className="text-xs text-text-secondary mt-1">KVMA Router</p>
        </div>
        <nav className="flex-1 p-3 overflow-y-auto">
          <ul className="space-y-1">
            {navItems.map((item) => (
              <li key={item.path}>
                <Link
                  to={item.path}
                  className={`flex items-center gap-3 px-4 py-3 rounded-lg transition-colors ${
                    location.pathname === item.path
                      ? 'bg-emerald/10 text-emerald'
                      : 'text-text-secondary hover:bg-bg-tertiary hover:text-text'
                  }`}
                >
                  <svg className="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                    {item.icon === 'servers' && (
                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M5 12h14M5 12a2 2 0 01-2-2 2 2 0 00-2-2 2 2 0 00-2 2 2 2 0 012 2m14 0a2 2 0 002-2 2 2 0 012-2 2 2 0 01-2 2 2 2 0 00-2 2m-2 0a2 2 0 012-2 2 2 0 002-2 2 2 0 00-2 2 2 2 0 01-2 2" />
                    )}
                    {item.icon === 'layers' && (
                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M19 11H5m14 0a2 2 0 012 2v6a2 2 0 01-2 2H5a2 2 0 01-2-2v-6a2 2 0 012-2m14 0V9a2 2 0 00-2-2M5 11V9a2 2 0 012-2m0 0V5a2 2 0 012-2h6a2 2 0 012 2v2M7 7h10" />
                    )}
                    {item.icon === 'video' && (
                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M15 10l4.553-2.276A1 1 0 0121 8.618v6.764a1 1 0 01-1.447.894L15 14M5 18h8a2 2 0 002-2V8a2 2 0 00-2-2H5a2 2 0 00-2 2v8a2 2 0 002 2z" />
                    )}
                    {item.icon === 'settings' && (
                      <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M10.325 4.317c.423-1.081.879-2.065 1.357-2.944.573-1.052 1.293-1.889 2.182-2.484.495-.333.863-.568 1.23-.672 1.265-.288 2.535-.288 3.8 0 .368.103.736.338 1.23.672.889.595 1.609 1.432 2.182 2.484.478.879.934 1.863 1.357 2.944.331.856.556 1.757.665 2.668l.016.094a2.22 2.22 0 01-.016-.094c-.109-.911-.334-1.812-.665-2.668a9.926 9.926 0 00-1.357-2.944c-.573-1.051-1.293-1.888-2.182-2.484-.495-.333-.863-.568-1.23-.672-1.265-.288-2.535-.288-3.8 0-.368.103-.736.338-1.23.672-.889.595-1.609 1.432-2.182 2.484-.478.879-.934 1.863-1.357 2.944-.331.856-.556 1.757-.665 2.668l-.016.094a2.22 2.22 0 00.016-.094c.109-.911.334-1.812.665-2.668.478-1.081.934-2.065 1.357-2.944.573-1.052 1.293-1.889 2.182-2.484.495-.333.863-.568 1.23-.672 1.265-.288 2.535-.288 3.8 0 .368.103.736.338 1.23.672.889.595 1.609 1.432 2.182 2.484.478.879.934 1.863 1.357 2.944.331.856.556 1.757.665 2.668l.016.094a2.22 2.22 0 01-.016-.094c-.109-.911-.334-1.812-.665-2.668a9.926 9.926 0 00-1.357-2.944c-.573-1.051-1.293-1.888-2.182-2.484-.495-.333-.863-.568-1.23-.672-1.265-.288-2.535-.288-3.8 0-.368.103-.736.338-1.23.672-.889.595-1.609 1.432-2.182 2.484-.478.879-.934 1.863-1.357 2.944-.331.856-.556 1.757-.665 2.668l-.016.094a2.22 2.22 0 00.016-.094c.109-.911.334-1.812.665-2.668z" />
                    )}
                  </svg>
                  <span>{item.label}</span>
                  {item.path === '/nodes' && (
                    <span className="ml-auto text-xs bg-emerald/20 text-emerald px-2 py-0.5 rounded-full">Live</span>
                  )}
                </Link>
              </li>
            ))}
          </ul>
        </nav>
        <div className="p-4 border-t border-bg-tertiary">
          <div className="flex items-center gap-3">
            <div className="w-8 h-8 rounded-full bg-emerald flex items-center justify-center text-bg">
              <svg className="w-5 h-5" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path strokeLinecap="round" strokeLinejoin="round" strokeWidth={2} d="M16 7a4 4 0 11-8 0 4 4 0 018 0zM12 14a7 7 0 00-7 7h14a7 7 0 00-7-7z" />
              </svg>
            </div>
            <div className="flex-1 min-w-0">
              <p className="text-sm font-medium text-text truncate">Operator</p>
              <p className="text-xs text-text-secondary truncate">Admin</p>
            </div>
          </div>
        </div>
      </aside>

      {/* Main Content */}
      <div className="flex-1 flex flex-col overflow-hidden">
        {/* Topbar */}
        <header className="h-header-height bg-bg-secondary flex items-center justify-between px-6 border-b border-bg-tertiary">
          <h2 className="text-lg font-semibold text-text">
            {navItems.find((item) => item.path === location.pathname)?.label ?? 'Dashboard'}
          </h2>
          <div className="flex items-center gap-4">
            <div className="flex items-center gap-2 px-3 py-1.5 bg-bg-tertiary rounded-full text-sm">
              <span className="w-2 h-2 rounded-full bg-emerald animate-pulse"></span>
              <span className="text-text-secondary">Controller Online</span>
            </div>
          </div>
        </header>

        {/* Page Content */}
        <main className="flex-1 overflow-y-auto p-6">
          {children}
        </main>
      </div>
    </div>
  )
}
