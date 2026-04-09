import { Bell, Search, Moon, Sun } from 'lucide-react'
import { useState } from 'react'

interface TopbarProps {
  toggleTheme: () => void
  isDark: boolean
}

const Topbar = ({ toggleTheme, isDark }: TopbarProps) => {
  const [searchQuery, setSearchQuery] = useState('')
  const [showNotifications, setShowNotifications] = useState(false)

  return (
    <header className="h-16 px-6 bg-slate-900 border-b border-slate-800 flex items-center justify-between">
      {/* Search */}
      <div className="hidden md:flex items-center flex-1 max-w-md">
        <div className="relative w-full">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 text-slate-500" size={18} />
          <input
            type="text"
            placeholder="Search nodes..."
            value={searchQuery}
            onChange={(e) => setSearchQuery(e.target.value)}
            className="w-full pl-10 pr-4 py-2 bg-slate-800 border border-slate-700 rounded-lg text-sm text-white placeholder-slate-500 focus:outline-none focus:border-emerald-500 focus:ring-1 focus:ring-emerald-500 transition-colors"
          />
        </div>
      </div>

      {/* Right side */}
      <div className="flex items-center gap-4 ml-4">
        {/* Theme toggle */}
        <button
          onClick={toggleTheme}
          className="p-2 text-slate-400 hover:text-white hover:bg-slate-800 rounded-lg transition-colors"
          title={isDark ? 'Switch to light mode' : 'Switch to dark mode'}
        >
          {isDark ? <Sun size={20} /> : <Moon size={20} />}
        </button>

        {/* Notifications */}
        <div className="relative">
          <button
            onClick={() => setShowNotifications(!showNotifications)}
            className="p-2 text-slate-400 hover:text-white hover:bg-slate-800 rounded-lg transition-colors relative"
          >
            <Bell size={20} />
            <span className="absolute top-1 right-1 w-2 h-2 bg-emerald-500 rounded-full" />
          </button>

          {/* Notifications dropdown */}
          {showNotifications && (
            <div className="absolute right-0 mt-2 w-64 bg-slate-900 border border-slate-800 rounded-lg shadow-lg py-2 z-50">
              <div className="px-4 py-2 border-b border-slate-800">
                <h3 className="font-semibold text-sm text-white">Notifications</h3>
              </div>
              <div className="px-2 py-2">
                <div className="px-3 py-2 hover:bg-slate-800 rounded cursor-pointer">
                  <p className="text-sm text-slate-300">Node "vm1" is online</p>
                  <p className="text-xs text-slate-500">2 minutes ago</p>
                </div>
                <div className="px-3 py-2 hover:bg-slate-800 rounded cursor-pointer">
                  <p className="text-sm text-slate-300">High CPU usage detected</p>
                  <p className="text-xs text-slate-500">15 minutes ago</p>
                </div>
              </div>
            </div>
          )}
        </div>

        {/* User avatar */}
        <div className="w-8 h-8 rounded-full bg-emerald-500 flex items-center justify-center text-white font-medium cursor-pointer">
          AU
        </div>
      </div>
    </header>
  )
}

export default Topbar
