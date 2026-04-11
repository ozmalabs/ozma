import { ReactNode } from 'react'
import { Link, useLocation } from 'react-router-dom'
import { ROUTES } from '../router'

interface SidebarProps {
  children?: ReactNode
  className?: string
}

export function Sidebar({ children, className = '' }: SidebarProps) {
  const location = useLocation()

  const navItems = [
    { name: 'Dashboard', path: ROUTES.nodes, icon: 'grid' },
    { name: 'Nodes', path: ROUTES.nodes, icon: 'computer' },
    { name: 'Routing', path: '/routing', icon: 'git-branch' },
    { name: 'Audio', path: '/audio', icon: 'volume-2' },
    { name: 'Devices', path: '/devices', icon: 'usb' },
    { name: 'Settings', path: ROUTES.settings, icon: 'settings' },
  ]

  return (
    <aside className={`w-64 border-r bg-card hidden md:flex flex-col ${className}`}>
      <div className="p-6 border-b">
        <Link to={ROUTES.nodes} className="flex items-center gap-2">
          <div className="w-8 h-8 bg-primary rounded-lg flex items-center justify-center">
            <span className="text-primary-foreground font-bold text-lg">O</span>
          </div>
          <div>
            <h1 className="text-xl font-bold text-foreground">Ozma</h1>
            <p className="text-xs text-muted-foreground">Controller UI</p>
          </div>
        </Link>
      </div>
      <nav className="flex-1 p-4 space-y-1 overflow-y-auto">
        {navItems.map((item) => (
          <Link
            key={item.path}
            to={item.path}
            className={`flex items-center gap-3 px-4 py-3 rounded-lg transition-all ${
              location.pathname === item.path
                ? 'bg-primary text-primary-foreground shadow-md'
                : 'text-foreground hover:bg-secondary hover:text-foreground'
            }`}
          >
            {getIcon(item.icon)}
            <span className="font-medium">{item.name}</span>
            {location.pathname === item.path && (
              <div className="ml-auto w-1.5 h-1.5 rounded-full bg-primary-foreground" />
            )}
          </Link>
        ))}
        {children}
      </nav>
      <div className="p-4 border-t">
        <div className="bg-muted/30 rounded-lg p-4">
          <div className="flex items-center gap-3 mb-2">
            <div className="w-2 h-2 rounded-full bg-emerald-500 animate-pulse" />
            <span className="text-sm font-medium text-foreground">Controller</span>
          </div>
          <p className="text-xs text-muted-foreground">Status: Online</p>
          <p className="text-xs text-muted-foreground mt-1">Port: 7380</p>
        </div>
      </div>
    </aside>
  )
}

function getIcon(name: string) {
  const icons: Record<string, ReactNode> = {
    grid: (
      <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <rect width="7" height="7" x="3" y="3" rx="1" />
        <rect width="7" height="7" x="14" y="3" rx="1" />
        <rect width="7" height="7" x="14" y="14" rx="1" />
        <rect width="7" height="7" x="3" y="14" rx="1" />
      </svg>
    ),
    computer: (
      <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <rect width="18" height="15" x="3" y="4" rx="2" ry="2" />
        <line x1="2" x2="22" y1="20" y2="20" />
        <line x1="4" x2="8" y1="20" y2="20" />
      </svg>
    ),
    'git-branch': (
      <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <line x1="6" x2="6" y1="3" y2="15" />
        <circle cx="18" cy="6" r="3" />
        <circle cx="6" cy="15" r="3" />
        <path d="M18 9a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z" />
        <path d="M6 18h12" />
      </svg>
    ),
    'volume-2': (
      <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <polygon points="11 5 6 9 2 9 2 15 6 15 11 19 11 5" />
        <path d="M19.07 4.93a10 10 0 0 1 0 14.14" />
        <path d="M15.54 8.46a5 5 0 0 1 0 7.07" />
      </svg>
    ),
    usb: (
      <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <path d="M10.6 10a2 2 0 1 1-4 0" />
        <path d="M13.4 14a2 2 0 1 1 4 0" />
        <path d="M14 14v4" />
        <path d="M10 10v4" />
        <path d="M10 14h4" />
      </svg>
    ),
    settings: (
      <svg xmlns="http://www.w3.org/2000/svg" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.5a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.1a2 2 0 0 1-1-1.72v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
        <circle cx="12" cy="12" r="3" />
      </svg>
    ),
  }
  return icons[name] || null
}
