import { Link, useLocation } from 'react-router-dom'
import { ServerIcon, ActivityIcon, SettingsIcon } from 'lucide-react'

const Sidebar = () => {
  const location = useLocation()

  const navItems = [
    { path: '/', label: 'Nodes', icon: ServerIcon },
    { path: '/activity', label: 'Activity', icon: ActivityIcon },
    { path: '/settings', label: 'Settings', icon: SettingsIcon },
  ]

  return (
    <aside className="w-64 h-screen bg-sidebar border-r border-border hidden md:flex flex-col">
      <div className="p-6 border-b border-border">
        <div className="flex items-center gap-3">
          <div className="w-8 h-8 bg-brand-accent rounded-lg flex items-center justify-center">
            <span className="font-bold text-black">O</span>
          </div>
          <h1 className="text-xl font-bold text-text-primary">Ozma</h1>
        </div>
      </div>

      <nav className="flex-1 py-6">
        <ul className="space-y-1">
          {navItems.map((item) => {
            const isActive = location.pathname === item.path
            return (
              <li key={item.path}>
                <Link
                  to={item.path}
                  className={`
                    flex items-center gap-3 px-6 py-3 text-sm font-medium transition-colors
                    ${isActive 
                      ? 'bg-brand-accent-dim text-brand-accent' 
                      : 'text-text-secondary hover:bg-tertiary hover:text-text-primary'}
                  `}
                >
                  <item.icon className="w-5 h-5" />
                  {item.label}
                </Link>
              </li>
            )
          })}
        </ul>
      </nav>

      <div className="p-4 border-t border-border">
        <div className="px-6 py-3 bg-secondary rounded-lg">
          <p className="text-xs font-mono text-tertiary mb-1">
            Controller Status
          </p>
          <div className="flex items-center gap-2">
            <span className="relative flex h-2 w-2">
              <span className="animate-ping absolute inline-flex h-full w-full rounded-full bg-brand-accent opacity-75"></span>
              <span className="relative inline-flex rounded-full h-2 w-2 bg-brand-accent"></span>
            </span>
            <span className="text-sm font-medium text-text-primary">Online</span>
          </div>
        </div>
      </div>
    </aside>
  )
}

export default Sidebar
