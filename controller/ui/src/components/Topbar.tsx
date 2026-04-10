import { BellIcon, UserIcon, SearchIcon } from 'lucide-react'

const Topbar = () => {
  return (
    <header className="h-16 bg-bg-primary border-b border-border flex items-center justify-between px-6 sticky top-0 z-10">
      <div className="flex items-center gap-4 flex-1">
        <div className="relative w-full max-w-md hidden md:block">
          <SearchIcon className="absolute left-3 top-1/2 -translate-y-1/2 w-4 h-4 text-text-tertiary" />
          <input
            type="text"
            placeholder="Search nodes..."
            className="w-full pl-10 pr-4 py-2 bg-secondary rounded-md border border-border text-sm text-text-primary placeholder-text-tertiary focus:outline-none focus:ring-2 focus:ring-brand-accent focus:border-transparent transition-all"
          />
        </div>
      </div>

      <div className="flex items-center gap-4">
        <button className="relative p-2 text-text-secondary hover:text-text-primary transition-colors">
          <BellIcon className="w-5 h-5" />
          <span className="absolute top-1.5 right-1.5 w-2 h-2 bg-brand-accent rounded-full"></span>
        </button>

        <div className="flex items-center gap-3 pl-4 border-l border-border">
          <div className="text-right hidden md:block">
            <p className="text-sm font-medium text-text-primary">Admin User</p>
            <p className="text-xs text-text-tertiary">Administrator</p>
          </div>
          <div className="w-10 h-10 bg-secondary rounded-full flex items-center justify-center border border-border">
            <UserIcon className="w-5 h-5 text-text-secondary" />
          </div>
        </div>
      </div>
    </header>
  )
}

export default Topbar
