import { ReactElement, ReactNode } from 'react'
import { useState } from 'react'
import { Link } from 'react-router-dom'
import { ROUTES } from '../router'

interface ErrorBoundaryProps {
  children: ReactNode
  fallback?: ReactNode
}

interface ErrorBoundaryState {
  hasError: boolean
  error: Error | null
  errorInfo: React.ErrorInfo | null
}

class ErrorBoundary extends React.Component<ErrorBoundaryProps, ErrorBoundaryState> {
  constructor(props: ErrorBoundaryProps) {
    super(props)
    this.state = {
      hasError: false,
      error: null,
      errorInfo: null,
    }
  }

  static getDerivedStateFromError(error: Error): Partial<ErrorBoundaryState> {
    return {
      hasError: true,
      error,
      errorInfo: null,
    }
  }

  componentDidCatch(error: Error, errorInfo: React.ErrorInfo): void {
    console.error('ErrorBoundary caught an error:', error, errorInfo)
    this.setState({
      error,
      errorInfo,
    })

    // Log error to console or error reporting service
    // logErrorToService(error, errorInfo)
  }

  handleReset = (): void => {
    this.setState({
      hasError: false,
      error: null,
      errorInfo: null,
    })
  }

  render(): React.ReactNode {
    if (this.state.hasError) {
      // You can render any custom fallback UI
      if (this.props.fallback) {
        return this.props.fallback
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
                {this.state.error?.message || 'Unknown error'}
              </p>
            </div>

            {this.state.errorInfo && (
              <div className="mb-6">
                <h2 className="text-lg font-semibold mb-2">Stack Trace:</h2>
                <div className="bg-muted/30 p-4 rounded-lg max-h-40 overflow-auto">
                  <pre className="text-xs font-mono text-muted-foreground whitespace-pre-wrap">
                    {this.state.errorInfo.componentStack}
                  </pre>
                </div>
              </div>
            )}

            <div className="flex flex-wrap gap-4">
              <button
                onClick={this.handleReset}
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

            {process.env.NODE_ENV === 'development' && this.state.error && (
              <div className="mt-6 pt-6 border-t">
                <h3 className="text-sm font-mono text-muted-foreground mb-2">
                  Error details (development only):
                </h3>
                <p className="text-xs font-mono text-muted-foreground">
                  {this.state.error.name}: {this.state.error.message}
                </p>
              </div>
            )}
          </div>
        </div>
      )
    }

    return this.props.children
  }
}

export default ErrorBoundary
