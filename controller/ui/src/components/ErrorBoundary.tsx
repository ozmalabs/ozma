import { Component, createRef, ReactNode } from 'react'

interface Props {
  children: ReactNode
  /** Custom fallback UI. Receives a reset callback. */
  fallback?: (reset: () => void) => ReactNode
  onError?: (error: Error, errorInfo: React.ErrorInfo) => void
  /** Called after the boundary resets itself. */
  onReset?: () => void
}

interface State {
  hasError: boolean
  error: Error | null
}

/**
 * Class-based error boundary (required by React — no functional equivalent).
 * Catches render-time errors in the subtree and shows a fallback UI.
 */
export default class ErrorBoundary extends Component<Props, State> {
  constructor(props: Props) {
    super(props)
    this.state = { hasError: false, error: null }
  }

  static getDerivedStateFromError(error: Error): State {
    return { hasError: true, error }
  }

  componentDidCatch(error: Error, info: React.ErrorInfo) {
    console.error('[ErrorBoundary]', error, info)
    try {
      this.props.onError?.(error, info)
    } catch {
      // Prevent onError callback from crashing the boundary itself
    }
  }

  /** Public reset — can be called via a ref: `boundaryRef.current?.resetError()` */
  resetError = () => {
    this.setState({ hasError: false, error: null })
    try {
      this.props.onReset?.()
    } catch {
      // ignore
    }
  }

  private handleReset = () => {
    this.resetError()
  }

  /** Convenience factory: attach to a component with `ref={ErrorBoundary.createRef()}` */
  static createRef() {
    return createRef<ErrorBoundary>()
  }

  render() {
    if (!this.state.hasError) {
      return this.props.children
    }

    if (this.props.fallback) {
      return this.props.fallback(this.handleReset)
    }

    return (
      <div className="flex items-center justify-center h-64">
        <div className="text-center max-w-sm">
          <p className="text-destructive font-semibold mb-2">Something went wrong</p>
          <p className="text-sm text-muted-foreground mb-4">{this.state.error?.message}</p>
          <button
            onClick={this.handleReset}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-lg hover:bg-primary/90 transition-colors text-sm"
          >
            Try Again
          </button>
        </div>
      </div>
    )
  }
}
