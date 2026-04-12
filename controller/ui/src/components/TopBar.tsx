import Button from './Button';

interface TopBarProps {
  activeNodeName?: string;
  onLogout?: () => void;
  className?: string;
}

export default function TopBar({ activeNodeName, onLogout, className = '' }: TopBarProps) {
  return (
    <header
      className={`flex items-center justify-between h-14 px-6 bg-zinc-900 border-b border-zinc-800 ${className}`}
    >
      <div className="flex items-center gap-3">
        <span className="text-zinc-100 font-semibold text-sm">Controller</span>
        {activeNodeName && (
          <>
            <span className="text-zinc-600">/</span>
            <span className="text-emerald-400 text-sm">{activeNodeName}</span>
          </>
        )}
      </div>
      {onLogout && (
        <Button variant="secondary" size="sm" onClick={onLogout}>
          Logout
        </Button>
      )}
    </header>
  );
}
