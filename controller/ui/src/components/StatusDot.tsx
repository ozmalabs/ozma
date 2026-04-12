type Status = 'online' | 'offline' | 'error' | 'warning';

interface StatusDotProps {
  status: Status;
  className?: string;
}

const colorClasses: Record<Status, string> = {
  online:  'bg-emerald-400',
  offline: 'bg-zinc-500',
  error:   'bg-red-400',
  warning: 'bg-amber-400',
};

export default function StatusDot({ status, className = '' }: StatusDotProps) {
  return (
    <span
      className={`inline-block h-2.5 w-2.5 rounded-full ${colorClasses[status]} ${className}`}
      aria-label={status}
    />
  );
}
