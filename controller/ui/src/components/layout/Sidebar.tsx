import React from 'react';
import { Link, useLocation } from 'react-router-dom';
import { LayoutDashboard, Server, Settings, Bell, HelpCircle } from 'lucide-react';

interface SidebarProps {
  isOpen: boolean;
}

const Sidebar: React.FC<SidebarProps> = ({ isOpen }) => {
  const location = useLocation();

  const menuItems = [
    { icon: LayoutDashboard, label: 'Nodes', path: '/' },
    { icon: Server, label: 'Devices', path: '/devices' },
    { icon: Settings, label: 'Settings', path: '/settings' },
    { icon: Bell, label: 'Notifications', path: '/notifications' },
    { icon: HelpCircle, label: 'Help', path: '/help' },
  ];

  return (
    <aside
      className={`${
        isOpen ? 'w-64' : 'w-20'
      } bg-gray-900 border-r border-gray-800 transition-all duration-300 flex flex-col z-20`}
    >
      <div className="p-4 border-b border-gray-800 flex items-center">
        <div className="flex items-center gap-3">
          <div className="w-8 h-8 bg-emerald-500 rounded-lg flex items-center justify-center">
            <Server className="w-5 h-5 text-white" />
          </div>
          {isOpen && (
            <div className="flex flex-col">
              <h1 className="text-lg font-bold text-white">Ozma</h1>
              <span className="text-xs text-gray-400">Controller UI</span>
            </div>
          )}
        </div>
      </div>

      <nav className="flex-1 py-4">
        <ul className="space-y-2 px-2">
          {menuItems.map((item) => {
            const isActive = location.pathname === item.path;
            return (
              <li key={item.path}>
                <Link
                  to={item.path}
                  className={`flex items-center gap-3 px-3 py-3 rounded-lg transition-colors ${
                    isActive
                      ? 'bg-emerald-500/10 text-emerald-400'
                      : 'text-gray-400 hover:text-gray-200 hover:bg-gray-800'
                  }`}
                  title={!isOpen ? item.label : undefined}
                  aria-label={!isOpen ? item.label : undefined}
                >
                  <item.icon className="w-5 h-5" />
                  {isOpen && <span>{item.label}</span>}
                </Link>
              </li>
            );
          })}
        </ul>
      </nav>

      <div className="p-4 border-t border-gray-800">
        <div className="flex items-center gap-3">
          <div className="w-8 h-8 bg-gray-700 rounded-full flex items-center justify-center">
            <span className="text-sm font-medium text-white">AD</span>
          </div>
          {isOpen && (
            <div className="flex flex-col overflow-hidden">
              <span className="text-sm font-medium text-white truncate">Admin User</span>
              <span className="text-xs text-gray-400 truncate">admin@ozma.local</span>
            </div>
          )}
        </div>
      </div>
    </aside>
  );
};

export default Sidebar;
