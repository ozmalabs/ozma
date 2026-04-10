import React from 'react';
import { Menu, Bell, Search, Moon, Sun } from 'lucide-react';
import { useNodesStore } from '../../hooks/useNodes';

interface TopbarProps {
  isOpen: boolean;
  setIsOpen: (isOpen: boolean) => void;
  toggleTheme: () => void;
  isDark: boolean;
}

const Topbar: React.FC<TopbarProps> = ({ isOpen, setIsOpen, toggleTheme, isDark }) => {
  const { webSocketStatus } = useNodesStore();

  return (
    <header className="h-16 bg-gray-900 border-b border-gray-800 flex items-center justify-between px-4 z-10">
      <div className="flex items-center gap-4">
        <button
          onClick={() => setIsOpen(!isOpen)}
          className="p-2 text-gray-400 hover:text-white rounded-lg hover:bg-gray-800 transition-colors"
          aria-label="Toggle sidebar"
        >
          <Menu className="w-6 h-6" />
        </button>
        
        <div className="relative hidden md:block">
          <Search className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-gray-400" />
          <input
            type="text"
            placeholder="Search nodes..."
            className="pl-10 pr-4 py-2 bg-gray-800 border border-gray-700 rounded-lg text-sm text-gray-200 placeholder-gray-500 focus:outline-none focus:border-emerald-500 focus:ring-1 focus:ring-emerald-500 w-64"
          />
        </div>
      </div>

      <div className="flex items-center gap-3">
        {/* WebSocket connection status indicator */}
        <div className="flex items-center gap-2 px-3 py-1.5 rounded-full bg-gray-800 border border-gray-700">
          <div
            className={`w-2 h-2 rounded-full ${
              webSocketStatus === 'connected'
                ? 'bg-emerald-500 animate-pulse'
                : webSocketStatus === 'disconnected'
                  ? 'bg-gray-500'
                  : 'bg-red-500'
            }`}
          />
          <span className="text-xs text-gray-400">
            {webSocketStatus === 'connected' ? 'Live' : webSocketStatus === 'disconnected' ? 'Idle' : 'Error'}
          </span>
        </div>

        <button
          onClick={toggleTheme}
          className="p-2 text-gray-400 hover:text-white rounded-lg hover:bg-gray-800 transition-colors"
          aria-label="Toggle theme"
        >
          {isDark ? <Sun className="w-5 h-5" /> : <Moon className="w-5 h-5" />}
        </button>

        <button className="p-2 text-gray-400 hover:text-white rounded-lg hover:bg-gray-800 transition-colors relative">
          <Bell className="w-5 h-5" />
          <span className="absolute top-2 right-2 w-2 h-2 bg-red-500 rounded-full" />
        </button>
      </div>
    </header>
  );
};

export default Topbar;
