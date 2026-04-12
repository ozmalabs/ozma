import { ReactNode } from 'react'
import { Navigate, useLocation } from 'react-router-dom'
import { useAuthStore } from '../store/useAuthStore'

interface ProtectedRouteProps {
  children: ReactNode
  requiredRoles?: string[]
}

/**
 * Guards routes based on authentication status and optional role requirements.
 * Redirects unauthenticated users to /login, preserving the intended destination.
 */
export default function ProtectedRoute({ children, requiredRoles = [] }: ProtectedRouteProps) {
  const { isAuthenticated, isLoading, user } = useAuthStore()
  const location = useLocation()

  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-screen">
        <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin" />
      </div>
    )
  }

  if (!isAuthenticated) {
    return <Navigate to="/login" state={{ from: location }} replace />
  }

  if (requiredRoles.length > 0 && user) {
    const hasAccess = requiredRoles.some((role) => user.roles.includes(role))
    if (!hasAccess) {
      return <Navigate to="/nodes" replace />
    }
  }

  return <>{children}</>
}
