import { cn } from '../lib/utils'

interface StatusDotProps {
  status: 'online' | 'offline' | 'connecting' | 'error' | 'unknown'
  size?: 'sm' | 'md' | 'lg'
  className?: string
}

const statusColors = {
  online: 'bg-emerald-500',
  offline: 'bg-rose-500',
  connecting: 'bg-amber-500',
  error: 'bg-red-500',
  unknown: 'bg-slate-500',
}

const statusAnimations = {
  online: 'animate-pulse',
  offline: '',
  connecting: 'animate-pulse',
  error: 'animate-pulse',
  unknown: '',
}

export function StatusDot({ status, size = 'md', className = '' }: StatusDotProps) {
  const sizeClasses = {
    sm: 'h-2 w-2',
    md: 'h-3 w-3',
    lg: 'h-4 w-4',
  }

  const colorClass = statusColors[status] || statusColors.unknown
  const animationClass = statusAnimations[status] || ''
  const sizeClass = sizeClasses[size] || sizeClasses.md

  return (
    <span
      className={cn(
        'rounded-full',
        colorClass,
        animationClass,
        className
      )}
      style={{ display: 'inline-block' }}
      title={status}
      aria-label={`Status: ${status}`}
    />
  )
}

export default StatusDot
