import { createBrowserRouter, Navigate, Outlet } from 'react-router-dom'
import { useAuth } from './store/useAuthStore'
import Layout from './layouts/Layout'
import NodesPage from './pages/NodesPage'
import NodeDetailPage from './pages/NodeDetailPage'
import SettingsPage from './pages/SettingsPage'
import LoginPage from './pages/LoginPage'
import ProtectedRoute from './components/ProtectedRoute'
import ErrorBoundary from './components/ErrorBoundary'

// Route configuration
export const ROUTES = {
  root: '/',
  login: '/login',
  dashboard: '/dashboard',
  nodes: '/nodes',
  node: '/nodes/:id',
  settings: '/settings',
} as const

// Protected route component that checks authentication
function AuthLayout() {
  return (
    <ProtectedRoute>
      <Layout>
        <ErrorBoundary>
          <Outlet />
        </ErrorBoundary>
      </Layout>
    </ProtectedRoute>
  )
}

// Public route (only redirects if already authenticated)
function PublicRoute({ children }: { children: React.ReactNode }) {
  const { isAuthenticated } = useAuth()

  if (isAuthenticated) {
    return <Navigate to={ROUTES.nodes} replace />
  }

  return children
}

// Route definitions
export const router = createBrowserRouter([
  {
    path: ROUTES.login,
    element: (
      <PublicRoute>
        <LoginPage />
      </PublicRoute>
    ),
  },
  {
    path: ROUTES.root,
    element: <AuthLayout />,
    children: [
      {
        index: true,
        element: <Navigate to={ROUTES.dashboard} replace />,
      },
      {
        path: ROUTES.dashboard,
        element: <NodesPage />,
      },
      {
        path: ROUTES.nodes,
        element: <NodesPage />,
      },
      {
        path: ROUTES.node,
        element: <NodeDetailPage />,
      },
      {
        path: ROUTES.settings,
        element: <SettingsPage />,
      },
    ],
  },
  {
    path: '*',
    element: (
      <AuthLayout>
        <div className="flex items-center justify-center h-full">
          <div className="text-center">
            <h1 className="text-6xl font-bold text-muted-foreground mb-4">404</h1>
            <p className="text-xl text-foreground">Page not found</p>
            <button
              onClick={() => window.history.back()}
              className="mt-4 px-6 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors"
            >
              Go Back
            </button>
          </div>
        </div>
      </AuthLayout>
    ),
  },
])

export { Navigate }
