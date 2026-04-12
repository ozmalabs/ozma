type Variant = 'success' | 'warning' | 'error' | 'neutral';

interface BadgeProps {
  variant?: Variant;
  label: string;
  className?: string;
}

const variantClasses: Record<Variant, string> = {
  success: 'bg-emerald-400/20 text-emerald-300',
  warning: 'bg-amber-400/20 text-amber-300',
  error:   'bg-red-400/20 text-red-300',
  neutral: 'bg-zinc-600/40 text-zinc-300',
};

export default function Badge({ variant = 'neutral', label, className = '' }: BadgeProps) {
  return (
    <span
      className={`inline-flex items-center rounded-full px-2 py-0.5 text-xs font-medium ${variantClasses[variant]} ${className}`}
    >
      {label}
    </span>
  );
}
