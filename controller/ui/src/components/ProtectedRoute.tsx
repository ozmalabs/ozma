import { ReactElement, ReactNode } from 'react'
import { Navigate, useLocation } from 'react-router-dom'
import { useAuth } from '../store/useAuthStore'
import { ROUTES } from '../router'

interface ProtectedRouteProps {
  children: ReactNode
  requiredRoles?: string[]
}

/**
 * ProtectedRoute component that guards routes based on authentication
 * and optionally role-based access control.
 */
export default function ProtectedRoute({
  children,
  requiredRoles = [],
}: ProtectedRouteProps): ReactElement {
  const { isAuthenticated, isLoading, user } = useAuth()
  const location = useLocation()

  // Show loading state while checking auth
  if (isLoading) {
    return (
      <div className="flex items-center justify-center h-screen">
        <div className="text-center">
          <div className="w-8 h-8 border-4 border-primary border-t-transparent rounded-full animate-spin mx-auto mb-4"></div>
          <p className="text-muted-foreground">Authenticating...</p>
        </div>
      </div>
    )
  }

  // Redirect to login if not authenticated
  if (!isAuthenticated) {
    return (
      <Navigate
        to={ROUTES.login}
        state={{ from: location }}
        replace
      />
    )
  }

  // Check role-based access if requiredRoles is specified
  if (requiredRoles.length > 0 && user) {
    const hasAccess = requiredRoles.some((role) => user.roles.includes(role))
    if (!hasAccess) {
      return (
        <Navigate
          to={ROUTES.nodes}
          state={{ from: location }}
          replace
        />
      )
    }
  }

  return children as ReactElement
}
