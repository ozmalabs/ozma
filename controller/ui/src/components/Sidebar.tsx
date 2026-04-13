import { NavLink } from 'react-router-dom';

interface NavItem {
  to: string;
  label: string;
  icon?: string;
}

const NAV_ITEMS: NavItem[] = [
  { to: '/',          label: 'Dashboard' },
  { to: '/nodes',     label: 'Nodes' },
  { to: '/scenarios', label: 'Scenarios' },
  { to: '/streaming', label: 'Streaming' },
  { to: '/streams',   label: 'Streams & Cameras' },
  { to: '/audio',     label: 'Audio' },
  { to: '/rgb',       label: 'RGB' },
  { to: '/controls',       label: 'Controls' },
  { to: '/routing-graph',  label: 'Routing Graph' },
  { to: '/settings',       label: 'Settings' },
];

interface SidebarProps {
  className?: string;
}

export default function Sidebar({ className = '' }: SidebarProps) {
  return (
    <nav
      className={`flex flex-col w-56 bg-zinc-900 border-r border-zinc-800 py-6 px-3 gap-1 ${className}`}
    >
      <div className="px-3 mb-6">
        <span className="text-emerald-400 font-bold text-lg tracking-tight">ozma</span>
      </div>
      {NAV_ITEMS.map((item) => (
        <NavLink
          key={item.to}
          to={item.to}
          end={item.to === '/'}
          className={({ isActive }) =>
            [
              'rounded-md px-3 py-2 text-sm transition-colors',
              isActive
                ? 'bg-emerald-400/10 text-emerald-400 font-medium'
                : 'text-zinc-400 hover:text-zinc-100 hover:bg-zinc-800',
            ].join(' ')
          }
        >
          {item.label}
        </NavLink>
      ))}
    </nav>
  );
}
