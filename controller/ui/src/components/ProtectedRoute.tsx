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

  if (requiredRoles.length > 0) {
    if (!user) {
      return <Navigate to="/login" state={{ from: location }} replace />
    }
    const hasAccess = requiredRoles.some((role) => user.roles.includes(role))
    if (!hasAccess) {
      return (
        <div className="flex items-center justify-center h-screen">
          <div className="text-center max-w-sm">
            <p className="text-destructive font-semibold mb-2">Access Denied</p>
            <p className="text-sm text-muted-foreground mb-4">
              You do not have permission to view this page.
            </p>
            <a
              href="/nodes"
              className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors text-sm"
            >
              Go to Nodes
            </a>
          </div>
        </div>
      )
    }
  }

  return <>{children}</>
}
