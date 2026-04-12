import React, { ReactNode, useState, useEffect } from 'react'
import { Link } from 'react-router-dom'
import { ROUTES } from '../router'

interface ErrorBoundaryProps {
  children: ReactNode
  fallback?: ReactNode
  onError?: (error: Error, errorInfo?: React.ErrorInfo) => void
}

interface ErrorBoundaryState {
  hasError: boolean
  error: Error | null
  errorInfo: React.ErrorInfo | null
}

/**
 * ErrorBoundary - Functional component error boundary for React 18+
 *
 * Usage:
 * <ErrorBoundary fallback={<CustomFallback />}>
 *   <App />
 * </ErrorBoundary>
 */
function ErrorBoundary({ children, fallback, onError }: ErrorBoundaryProps): React.ReactNode {
  const [state, setState] = useState<ErrorBoundaryState>({
    hasError: false,
    error: null,
    errorInfo: null,
  })

  // Effect to handle error logging
  useEffect(() => {
    if (state.hasError && state.error) {
      console.error('ErrorBoundary caught an error:', state.error, state.errorInfo)

      // Log error to console or error reporting service
      // logErrorToService(state.error, state.errorInfo)

      // Safe callback - handle case where onError is not provided
      if (onError && typeof onError === 'function') {
        try {
          onError(state.error, state.errorInfo ?? undefined)
        } catch (callbackError) {
          console.error('ErrorBoundary: onError callback threw an error:', callbackError)
        }
      }
    }
  }, [state.hasError, state.error, state.errorInfo, onError])

  if (state.hasError) {
    return (
      <ErrorFallback
        error={state.error}
        errorInfo={state.errorInfo}
        fallback={fallback}
        onReset={() => setState({ hasError: false, error: null, errorInfo: null })}
      />
    )
  }

  // Use ErrorCatcher directly inline to avoid helper function issues
  return (
    <ErrorCatcher
      onError={(error, errorInfo) => {
        setState({
          hasError: true,
          error,
          errorInfo,
        })
      }}
    >
      {children}
    </ErrorCatcher>
  )
}

// ErrorCatcher component that catches errors in its children
interface ErrorCatcherProps {
  children: ReactNode
  onError: (error: Error, errorInfo: React.ErrorInfo) => void
}

interface ErrorCatcherState {
  hasError: boolean
  error: Error | null
  errorInfo: React.ErrorInfo | null
}

// We need to use a class component for catching errors during render
// This is the only place where class components are needed in React 18+
class ErrorCatcher extends React.Component<ErrorCatcherProps, ErrorCatcherState> {
  constructor(props: ErrorCatcherProps) {
    super(props)
    this.state = {
      hasError: false,
      error: null,
      errorInfo: null,
    }
  }

  static getDerivedStateFromError(error: Error): Partial<ErrorCatcherState> {
    return {
      hasError: true,
      error,
      errorInfo: null,
    }
  }

  componentDidCatch(error: Error, errorInfo: React.ErrorInfo): void {
    this.setState({
      error,
      errorInfo,
    })
    // Safe callback - handle case where onError is not provided
    try {
      this.props.onError(error, errorInfo)
    } catch (callbackError) {
      console.error('ErrorBoundary: onError callback threw an error:', callbackError)
    }
  }

  render(): React.ReactNode {
    if (this.state.hasError) {
      return null // Don't render children when there's an error
    }
    return this.props.children
  }
}

// Error fallback component
interface ErrorFallbackProps {
  error: Error | null
  errorInfo: React.ErrorInfo | null
  fallback?: ReactNode
  onReset: () => void
}

function ErrorFallback({ error, errorInfo, fallback, onReset }: ErrorFallbackProps): React.ReactNode {
  if (fallback) {
    return fallback
  }

  return (
    <div className="min-h-screen bg-background flex items-center justify-center p-4">
      <div className="max-w-2xl w-full bg-card rounded-xl border p-8 shadow-lg">
        <div className="flex items-center gap-4 mb-6">
          <div className="flex-shrink-0">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="48"
              height="48"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              className="text-destructive"
            >
              <circle cx="12" cy="12" r="10" />
              <line x1="15" x2="9" y1="9" y2="15" />
              <line x1="9" x2="15" y1="9" y2="15" />
              <line x1="9" x2="15" y1="15" y2="9" />
            </svg>
          </div>
          <div>
            <h1 className="text-2xl font-bold text-foreground">Something went wrong</h1>
            <p className="text-muted-foreground">We're working to fix the issue</p>
          </div>
        </div>

        <div className="mb-6">
          <h2 className="text-lg font-semibold mb-2">Error Message:</h2>
          <p className="font-mono text-sm text-destructive bg-destructive/10 p-3 rounded-lg">
            {error?.message || 'Unknown error'}
          </p>
        </div>

        {errorInfo && (
          <div className="mb-6">
            <h2 className="text-lg font-semibold mb-2">Stack Trace:</h2>
            <div className="bg-muted/30 p-4 rounded-lg max-h-40 overflow-auto">
              <pre className="text-xs font-mono text-muted-foreground whitespace-pre-wrap">
                {errorInfo.componentStack}
              </pre>
            </div>
          </div>
        )}

        <div className="flex flex-wrap gap-4">
          <button
            onClick={onReset}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors"
          >
            Try Again
          </button>
          <Link
            to={ROUTES.nodes}
            className="px-4 py-2 bg-secondary text-foreground rounded-lg hover:bg-secondary/90 transition-colors"
          >
            Go to Dashboard
          </Link>
          <button
            onClick={() => window.location.href = '/'}
            className="px-4 py-2 bg-border text-foreground rounded-lg hover:bg-border/90 transition-colors"
          >
            Home
          </button>
        </div>

        {process.env.NODE_ENV === 'development' && error && (
          <div className="mt-6 pt-6 border-t">
            <h3 className="text-sm font-mono text-muted-foreground mb-2">
              Error details (development only):
            </h3>
            <p className="text-xs font-mono text-muted-foreground">
              {error.name}: {error.message}
            </p>
          </div>
        )}
      </div>
    </div>
  )
}

export default ErrorBoundary
