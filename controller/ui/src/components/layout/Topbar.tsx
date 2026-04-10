import { Bell, User, Search, Menu } from 'lucide-react'
import { useState } from 'react'

export default function Topbar() {
  const [sidebarOpen, setSidebarOpen] = useState(false)

  return (
    <header className="bg-bg-sidebar border-b border-border px-6 py-3 flex items-center justify-between">
      <div className="flex items-center gap-4">
        <button
          onClick={() => setSidebarOpen(!sidebarOpen)}
          className="text-text-muted hover:text-text"
        >
          <Menu className="w-6 h-6" />
        </button>
        <div className="hidden md:flex items-center gap-2 text-text-muted">
          <Search className="w-4 h-4" />
          <input
            type="text"
            placeholder="Search..."
            className="bg-bg border border-border rounded-md px-3 py-1 text-sm text-text focus:outline-none focus:border-emerald-400 transition-colors"
          />
        </div>
      </div>
      <div className="flex items-center gap-4">
        <button className="relative text-text-muted hover:text-text">
          <Bell className="w-6 h-6" />
          <span className="absolute top-0 right-0 h-2 w-2 bg-emerald-400 rounded-full"></span>
        </button>
        <div className="flex items-center gap-2 text-text">
          <User className="w-5 h-5 text-emerald-400" />
          <span className="text-sm font-medium">Admin</span>
        </div>
      </div>
    </header>
  )
}
