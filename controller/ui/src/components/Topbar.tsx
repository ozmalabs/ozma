import React, { ReactNode } from 'react'
import { Link, useLocation, useParams } from 'react-router-dom'
import { ROUTES } from '../router'
import { useAuth } from '../store/useAuthStore'

interface TopbarProps {
  title?: string
  subtitle?: string
  actions?: ReactNode
  showBack?: boolean
  className?: string
}

export function Topbar({ title, subtitle, actions, showBack = false, className = '' }: TopbarProps) {
  const location = useLocation()
  const params = useParams()
  const { user } = useAuth()

  const breadcrumbItems = getBreadcrumbItems(location.pathname, params)

  return (
    <header className={`h-16 border-b bg-card flex items-center px-6 justify-between ${className}`}>
      <div className="flex items-center gap-4 flex-1 min-w-0">
        {showBack && (
          <button
            onClick={() => window.history.back()}
            className="p-2 hover:bg-secondary rounded-lg transition-colors text-muted-foreground"
          >
            <svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
              <path d="m12 19-7-7 7-7m7 7v-4m0 4L5" />
            </svg>
          </button>
        )}
        <div className="flex flex-col min-w-0">
          <div className="flex items-center gap-2">
            {breadcrumbItems.length > 0 && (
              <div className="flex items-center text-sm text-muted-foreground">
                {breadcrumbItems.map((item, index) => (
                  <React.Fragment key={index}>
                    <Link
                      to={item.path}
                      className={`hover:text-foreground transition-colors ${index === breadcrumbItems.length - 1 ? 'font-medium text-foreground' : ''}`}
                    >
                      {item.label}
                    </Link>
                    {index < breadcrumbItems.length - 1 && (
                      <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" className="mx-2">
                        <path d="m9 18 6-6-6-6" />
                      </svg>
                    )}
                  </React.Fragment>
                ))}
              </div>
            )}
          </div>
          <div className="flex flex-col">
            <h1 className="text-lg font-semibold text-foreground">{title || breadcrumbItems[breadcrumbItems.length - 1]?.label}</h1>
            {subtitle && <p className="text-xs text-muted-foreground">{subtitle}</p>}
          </div>
        </div>
      </div>
      <div className="flex items-center gap-4 flex-shrink-0">
        {actions}
        {user && (
          <div className="flex items-center gap-3 pl-4 border-l">
            <div className="text-right hidden md:block">
              <p className="text-sm font-medium text-foreground">{user.username}</p>
              <p className="text-xs text-muted-foreground">{user.email}</p>
            </div>
            <div className="w-10 h-10 bg-primary/20 rounded-full flex items-center justify-center">
              <span className="text-primary font-medium">{user.username.charAt(0).toUpperCase()}</span>
            </div>
          </div>
        )}
      </div>
    </header>
  )
}

function getBreadcrumbItems(pathname: string, params: Record<string, string | undefined>) {
  const items: { label: string; path: string }[] = []

  if (pathname === ROUTES.nodes || pathname === ROUTES.root) {
    items.push({ label: 'Dashboard', path: ROUTES.nodes })
  } else if (pathname.startsWith(ROUTES.node)) {
    items.push({ label: 'Nodes', path: ROUTES.nodes })
    const nodeId = params.id
    if (nodeId) {
      items.push({ label: nodeId, path: pathname })
    }
  } else if (pathname === ROUTES.settings) {
    items.push({ label: 'Dashboard', path: ROUTES.nodes })
    items.push({ label: 'Settings', path: ROUTES.settings })
  } else if (pathname.startsWith('/routing')) {
    items.push({ label: 'Dashboard', path: ROUTES.nodes })
    items.push({ label: 'Routing', path: '/routing' })
  } else if (pathname.startsWith('/audio')) {
    items.push({ label: 'Dashboard', path: ROUTES.nodes })
    items.push({ label: 'Audio', path: '/audio' })
  } else if (pathname.startsWith('/devices')) {
    items.push({ label: 'Dashboard', path: ROUTES.nodes })
    items.push({ label: 'Devices', path: '/devices' })
  }

  return items
}
