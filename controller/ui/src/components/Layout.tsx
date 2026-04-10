import { ReactNode } from 'react'
import { Link } from 'react-router-dom'

interface LayoutProps {
  children: React.ReactNode
}

function Layout({ children }: LayoutProps) {
  return (
    <div className="flex h-screen bg-gray-900 text-gray-100">
      {/* Sidebar */}
      <aside className="w-64 bg-gray-800 flex-shrink-0 flex flex-col">
        <div className="p-4 border-b border-gray-700">
          <h1 className="text-2xl font-bold text-emerald-400">Ozma</h1>
          <p className="text-xs text-gray-500 mt-1">KVMA Router</p>
        </div>
        
        <nav className="flex-1 overflow-y-auto py-4">
          <Link
            to="/"
            className="block px-4 py-3 text-sm font-medium text-emerald-400 border-l-4 border-emerald-400 bg-gray-700/50"
          >
            Nodes
          </Link>
          <Link
            to="/scenarios"
            className="block px-4 py-3 text-sm font-medium text-gray-400 hover:text-gray-200 hover:bg-gray-700/50 transition-colors"
          >
            Scenarios
          </Link>
          <Link
            to="/settings"
            className="block px-4 py-3 text-sm font-medium text-gray-400 hover:text-gray-200 hover:bg-gray-700/50 transition-colors"
          >
            Settings
          </Link>
        </nav>
        
        <div className="p-4 border-t border-gray-700">
          <div className="text-xs text-gray-500">
            <p>Controller v0.1.0</p>
          </div>
        </div>
      </aside>
      
      {/* Main Content */}
      <main className="flex-1 flex flex-col overflow-hidden">
        {/* Topbar */}
        <header className="h-16 bg-gray-800 border-b border-gray-700 flex items-center px-6 justify-between">
          <div className="flex items-center space-x-4">
            <h2 className="text-xl font-semibold text-white">Dashboard</h2>
          </div>
          <div className="flex items-center space-x-4">
            <span className="text-sm text-gray-400">Status:</span>
            <span className="px-2 py-1 bg-emerald-900/50 text-emerald-300 border border-emerald-700 rounded text-xs font-medium">
              Online
            </span>
          </div>
        </header>
        
        {/* Content */}
        <div className="flex-1 overflow-auto">
          {children}
        </div>
      </main>
    </div>
  )
}
Layout.displayName = 'Layout'

export default Layout
