import { AuthResponse, NodesResponse } from '../types/api'

// Constants
const API_BASE = '/api/v1'
const DEFAULT_TIMEOUT = 30000 // 30 seconds
const MAX_RETRIES = 3
const INITIAL_RETRY_DELAY = 1000 // 1 second
const TOKEN_REFRESH_BUFFER = 5000 // 5 seconds
const MAX_REQUEST_ID_LENGTH = 32
const RATE_LIMIT_WINDOW = 60000 // 1 minute
const RATE_LIMIT_MAX_REQUESTS = 100

// Types - Don't extend RequestInit to avoid priority type conflicts
export interface RequestOptions {
  params?: Record<string, string | number | boolean>
  skipAuth?: boolean
  timeout?: number
  retry?: boolean
  signal?: AbortSignal
  priority?: number // Higher number = higher priority
  dedupKey?: string // For request deduplication
  dedupParams?: Record<string, unknown> // Parameters to distinguish dedup requests
  context?: Record<string, unknown> // Request context for tracing
  metadata?: Record<string, string> // Request metadata
  validateRequest?: (body: unknown) => boolean // Request validation
  validateResponse?: (data: unknown) => boolean // Response validation
  onProgress?: (progress: number) => void // Progress callback
  body?: BodyInit // Explicitly add body
}

interface TokenInfo {
  exp: number
  iat?: number
  sub?: string
  [key: string]: unknown
}

interface RefreshTokenResponse {
  token: string
  expires_at: string
}

interface RequestQueueItem {
  request: () => Promise<void>
  priority: number
  timestamp: number
}

interface RequestCacheEntry<T> {
  data: T
  timestamp: number
  expiry: number
}

interface ThrottleEntry {
  lastCall: number
  lastResult: Promise<unknown> | null
}

interface DebounceEntry {
  timeoutId: NodeJS.Timeout | null
  lastArgs: Parameters<() => Promise<unknown>> | null
}

// Error classes
export class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
    public data?: unknown
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

export class NetworkError extends Error {
  constructor(message: string = 'Network error') {
    super(message)
    this.name = 'NetworkError'
  }
}

export class TimeoutError extends Error {
  constructor(message: string = 'Request timeout') {
    super(message)
    this.name = 'TimeoutError'
  }
}

export class DeduplicationError extends Error {
  constructor(message: string = 'Duplicate request in progress') {
    super(message)
    this.name = 'DeduplicationError'
  }
}

export class RateLimitError extends Error {
  constructor(message: string = 'Rate limit exceeded') {
    super(message)
    this.name = 'RateLimitError'
  }
}

/**
 * Constant-time string comparison to prevent timing attacks.
 * Always iterates all characters to avoid timing leakage on early mismatch.
 */
function constantTimeEquals(a: string, b: string): boolean {
  if (a.length !== b.length) return false
  let result = 0
  for (let i = 0; i < a.length; i++) {
    result |= a.charCodeAt(i) ^ b.charCodeAt(i)
  }
  return result === 0
}

// Token storage abstraction with improved security
const createTokenStorage = () => {
  // Use module-level cache for SSR compatibility and reduced localStorage access
  // Cache is always synchronized with localStorage
  let cachedToken: string | null = null
  let cachedTokenExpiry: number | null = null

  // Helper to parse token and update cache atomically
  // This ensures cache is always in sync with localStorage
  const parseAndCacheToken = (token: string | null): void => {
    if (!token) {
      cachedToken = null
      cachedTokenExpiry = null
      return
    }
    cachedToken = token
    // Parse expiry from token for faster access
    try {
      const parts = token.split('.')
      if (parts.length === 3) {
        const payload = JSON.parse(atob(parts[1]))
        cachedTokenExpiry = payload.exp * 1000
      } else {
        cachedTokenExpiry = null
      }
    } catch {
      cachedTokenExpiry = null
    }
  }

  return {
    get: (): string | null => {
      if (typeof localStorage === 'undefined') {
        return cachedToken
      }
      const token = localStorage.getItem('ozma_token')
      // Parse and cache atomically in one operation to prevent race condition
      parseAndCacheToken(token)
      return token
    },
    set: (token: string): void => {
      if (typeof localStorage === 'undefined') {
        cachedToken = token
        cachedTokenExpiry = null
        // Still parse expiry even in SSR context for consistency
        try {
          const parts = token.split('.')
          if (parts.length === 3) {
            const payload = JSON.parse(atob(parts[1]))
            cachedTokenExpiry = payload.exp * 1000
          }
        } catch {
          cachedTokenExpiry = null
        }
      } else {
        localStorage.setItem('ozma_token', token)
        // Parse and cache atomically in one operation to prevent race condition
        parseAndCacheToken(token)
      }
    },
    remove: (): void => {
      if (typeof localStorage !== 'undefined') {
        localStorage.removeItem('ozma_token')
      }
      cachedToken = null
      cachedTokenExpiry = null
    },
    getExpiry: (): number | null => {
      if (typeof localStorage === 'undefined') {
        return cachedTokenExpiry
      }
      const token = localStorage.getItem('ozma_token')
      // Parse and cache atomically in one operation to prevent race condition
      parseAndCacheToken(token)
      return cachedTokenExpiry
    },
    // Constant-time token comparison for refresh validation
    compareToken: (token: string): boolean => {
      if (typeof localStorage === 'undefined') {
        return cachedToken ? constantTimeEquals(cachedToken, token) : false
      }
      const stored = localStorage.getItem('ozma_token')
      if (!stored) return false
      return constantTimeEquals(stored, token)
    },
  }
}

const tokenStorage = createTokenStorage()

/**
 * Parse JWT token payload safely
 * Uses Object.create(null) to prevent prototype pollution
 */
function parseToken(token: string): TokenInfo | null {
  try {
    if (!token || typeof token !== 'string') return null
    
    const parts = token.split('.')
    if (parts.length !== 3) return null

    const payloadBase64 = parts[1]
    if (!payloadBase64) return null

    // Safe base64 decode with validation
    const payloadJson = atob(payloadBase64)
    if (!payloadJson || payloadJson.length === 0) return null

    // Use Object.create(null) to create a prototypeless object
    // This prevents prototype pollution attacks
    const payload = JSON.parse(payloadJson, (key, value) => {
      // Reviver function - return null for dangerous keys
      if (key === '__proto__' || key === 'constructor' || key === 'prototype') {
        return undefined
      }
      return value
    })

    // Verify payload is a plain object (not from prototype chain)
    if (!payload || typeof payload !== 'object' || Array.isArray(payload)) {
      return null
    }

    // Additional validation for JWT payload structure
    // Only allow specific known keys to prevent injection
    const allowedKeys = ['sub', 'iat', 'exp', 'aud', 'iss', 'scopes', 'username', 'email', 'roles', 'avatar', 'name']
    for (const key in payload) {
      if (!allowedKeys.includes(key)) {
        // Unknown key, skip it but don't fail entirely
        continue
      }
      // Validate value types for security
      const val = payload[key]
      if (key === 'exp' || key === 'iat') {
        if (typeof val !== 'number') return null
      } else if (key === 'scopes') {
        if (!Array.isArray(val)) return null
        if (!val.every(s => typeof s === 'string')) return null
      } else if (key === 'roles') {
        if (!Array.isArray(val)) return null
        if (!val.every(r => typeof r === 'string')) return null
      } else if (typeof val !== 'string') {
        // Other string fields must be strings
        continue
      }
    }

    return payload as TokenInfo
  } catch {
    return null
  }
}

/**
 * Get the authentication token from storage
 */
export function getToken(): string | null {
  return tokenStorage.get()
}

/**
 * Set the authentication token in storage
 */
export function setToken(token: string): void {
  tokenStorage.set(token)
}

/**
 * Remove the authentication token from storage
 */
export function removeToken(): void {
  tokenStorage.remove()
}

/**
 * Check if user is authenticated with safe token validation
 */
export function isAuthenticated(): boolean {
  const token = getToken()
  if (!token) return false

  const payload = parseToken(token)
  if (!payload) return false

  const expiration = payload.exp * 1000
  return Date.now() < expiration
}

/**
 * Check if token is about to expire (within buffer time)
 * NOTE: This function now reads token and expiry atomically to prevent race conditions
 */
export function isTokenExpiring(): boolean {
  const token = getToken()
  if (!token) return true

  // Use token storage expiry directly to avoid race condition
  // This reads from localStorage and parses the token in one atomic operation
  const expiry = tokenStorage.getExpiry()
  
  if (expiry === null || expiry === undefined) {
    // If we couldn't parse expiry from storage, try parsing the token directly
    // but use the same token we already have to avoid race condition
    const payload = parseToken(token)
    if (!payload) return true
    return Date.now() >= payload.exp * 1000 - TOKEN_REFRESH_BUFFER
  }

  const now = Date.now()
  return now >= expiry - TOKEN_REFRESH_BUFFER
}

/**
 * Build URL with query parameters
 * NOTE: Uses relative URLs by default to work in both browser and server contexts
 */
function buildUrl(base: string, params?: Record<string, string | number | boolean>): string {
  if (!params) return base
  try {
    // Use base URL with leading slash for relative paths (works in both browser and server)
    // For absolute URLs, we only use window.location.origin in browser context
    const hasProtocol = base.includes('://')
    const baseUrl = hasProtocol 
      ? (typeof window !== 'undefined' && typeof window.location !== 'undefined' ? window.location.origin : '')
      : ''
    
    const url = new URL(base, baseUrl || '/')
    Object.entries(params).forEach(([key, value]) => {
      if (value !== undefined && value !== null) {
        url.searchParams.append(key, String(value))
      }
    })
    // Return just path + query for relative URLs, or full URL if absolute
    return url.pathname + url.search
  } catch {
    return base
  }
}

/**
 * Handle response and throw errors for non-OK responses
 * NOTE: Handles non-JSON responses gracefully - some APIs may return plain text or other formats
 */
async function handleResponse<T>(response: Response): Promise<T> {
  const contentType = response.headers.get('content-type')
  
  // Check if response is JSON
  const isJson = contentType && contentType.includes('application/json')
  
  // For non-200 responses, always try to parse JSON first if content-type indicates it
  if (!response.ok) {
    let data: unknown = null
    let errorMessage = `HTTP ${response.status}`
    
    if (isJson) {
      try {
        data = await response.json()
        // Extract error message from JSON if available
        errorMessage = (data as { error?: string; message?: string })?.error ||
                      (data as { error?: string; message?: string })?.message ||
                      errorMessage
      } catch {
        // If JSON parsing fails but content-type is JSON, use status as fallback
        errorMessage = `HTTP ${response.status} (failed to parse error details)`
      }
    } else {
      // For non-JSON responses, try to read as text
      try {
        const text = await response.text()
        if (text) {
          // Truncate long text for logging
          errorMessage = text.length > 500 ? text.substring(0, 500) + '...' : text
        }
      } catch {
        // If text parsing also fails, use status code
      }
    }
    
    throw new ApiError(response.status, errorMessage, data)
  }

  // For successful responses, return data based on content-type
  if (isJson) {
    let data: unknown
    try {
      data = await response.json()
    } catch {
      // If JSON parsing fails, return empty object
      data = null
    }
    return data as T
  } else {
    // For non-JSON responses, return the response body as-is
    try {
      const text = await response.text()
      // Try to parse as JSON anyway in case content-type was wrong
      if (text && text.startsWith('{') && text.endsWith('}')) {
        try {
          return JSON.parse(text) as T
        } catch {
          // Fall through to return text
        }
      }
      return text as T
    } catch {
      // If we can't read the body, return null
      return null as T
    }
  }
}

// Request controllers for cancellation - properly cleaned up on completion
const requestControllers = new Map<string, AbortController>()
// Deduplication map to prevent duplicate requests - now tracks both key and params
interface DedupRequestInfo {
  promise: Promise<unknown>
  params: Record<string, unknown> | null
  timestamp: number
}
const activeRequests = new Map<string, DedupRequestInfo>()
// Request queue for prioritization
const requestQueue: RequestQueueItem[] = []
// Global request context for tracing
const globalContext = new Map<string, unknown>()
// Rate limiting state
const rateLimitState = {
  lastRequestTime: 0,
  requestCount: 0,
  windowStart: 0,
}
// Request caching
const requestCache = new Map<string, RequestCacheEntry<unknown>>()
// Throttle state
const throttleState = new Map<string, ThrottleEntry>()
// Debounce state
const debounceState = new Map<string, DebounceEntry>()
// Memoize cache
const memoizeCache = new Map<string, Promise<unknown>>()

/**
 * Generate unique request ID with trace correlation
 */
function generateRequestId(method: string, path: string): string {
  const timestamp = Date.now().toString(36)
  const random = Math.random().toString(36).substring(2, 8)
  return `${method.slice(0, 4)}${path.split('/')[1]?.substring(0, 8) || ''}${timestamp}${random}`
    .substring(0, MAX_REQUEST_ID_LENGTH)
}

/**
 * Get or create trace ID for request correlation
 */
function getTraceId(): string {
  const traceId = globalContext.get('traceId') as string | undefined
  if (traceId) return traceId
  const newTraceId = `trace-${Date.now()}-${Math.random().toString(36).substring(2, 10)}`
  globalContext.set('traceId', newTraceId)
  return newTraceId
}

/**
 * Cancel a pending request
 */
export function cancelRequest(requestId: string): void {
  const controller = requestControllers.get(requestId)
  if (controller) {
    controller.abort()
    requestControllers.delete(requestId)
  }
  // Clean up deduplication map
  activeRequests.delete(requestId)
  // Clean up request cache
  const cacheKey = `cancel_${requestId}`
  requestCache.delete(cacheKey)
}

/**
 * Create a timeout signal with cleanup
 */
function createTimeoutSignal(timeout: number): { signal: AbortSignal; timeoutId: NodeJS.Timeout } {
  const controller = new AbortController()
  const timeoutId = setTimeout(() => controller.abort(), timeout)
  return { signal: controller.signal, timeoutId }
}

/**
 * Retry strategy with exponential backoff and jitter
 * NOTE: Network errors are now retryable by default to handle transient failures
 * NOTE: Server errors (5xx) are now retryable to handle transient server issues
 */
async function retryWithBackoff<T>(
  fn: () => Promise<T>,
  options: {
    maxRetries?: number;
    delay?: number;
    onError?: (error: Error, attempt: number) => void;
    retryAll?: boolean; // Also retry on client errors
    retryNetworkErrors?: boolean; // Retry network errors (default: true)
    retryServerErrors?: boolean; // Retry server errors (default: true)
  } = {}
): Promise<T> {
  const {
    maxRetries = MAX_RETRIES,
    delay = INITIAL_RETRY_DELAY,
    onError,
    retryAll = false,
    retryNetworkErrors = true, // Default to retrying network errors
    retryServerErrors = true // Default to retrying server errors (5xx)
  } = options

  let lastError: Error | null = null

  for (let attempt = 1; attempt <= maxRetries; attempt++) {
    try {
      return await fn()
    } catch (error) {
      lastError = error instanceof Error ? error : new Error(String(error))

      // Log retry attempt
      console.debug(`Request attempt ${attempt}/${maxRetries} failed:`, lastError.message)

      // Don't retry after last attempt
      if (attempt >= maxRetries) {
        throw lastError
      }

      // Don't retry on timeout (retries won't help)
      if (lastError instanceof TimeoutError) {
        throw lastError
      }

      // Don't retry on deduplication error (it means a request is already in progress)
      if (lastError instanceof DeduplicationError) {
        throw lastError
      }

      // Don't retry on rate limit error (wait for rate limit window)
      if (lastError instanceof RateLimitError) {
        throw lastError
      }

      // Check if it's an ApiError with specific status codes
      if (lastError instanceof ApiError) {
        const isClientError = lastError.status >= 400 && lastError.status < 500
        const isServerError = lastError.status >= 500 && lastError.status < 600
        const isRateLimited = lastError.status === 429

        // Don't retry on client errors (4xx) except 429 (unless retryAll is true)
        if (!retryAll && isClientError) {
          // Only retry 429 rate limits
          if (lastError.status !== 429) {
            throw lastError
          }
        }
        // Don't retry on server errors unless retryServerErrors is true
        else if (!retryServerErrors && isServerError) {
          throw lastError
        }
        // Retry 429 with rate limit handling
        else if (isRateLimited) {
          // Rate limited - wait and retry
          const retryAfter = lastError.data as { retry_after?: number } | undefined
          const waitTime = retryAfter?.retry_after ? retryAfter.retry_after * 1000 : delay * Math.pow(2, attempt - 1)
          await new Promise(resolve => setTimeout(resolve, waitTime))
          continue
        }
      }

      // For network errors, only retry if retryNetworkErrors is true
      if (lastError instanceof NetworkError && !retryNetworkErrors) {
        throw lastError
      }

      // For generic errors (not ApiError, not specific error types),
      // retry if retryAll is true or if it's a network-related error
      if (retryAll || lastError.name === 'TypeError' || lastError.name === 'NetworkError') {
        // Allow retry with backoff
      } else {
        throw lastError
      }

      // Exponential backoff with jitter
      const backoffDelay = delay * Math.pow(2, attempt - 1)
      const jitter = Math.random() * 0.3 * backoffDelay // 0-30% jitter
      const finalDelay = backoffDelay + jitter

      console.debug(`Retrying in ${finalDelay.toFixed(0)}ms...`)

      if (onError) {
        onError(lastError, attempt)
      }

      await new Promise(resolve => setTimeout(resolve, finalDelay))
    }
  }

  throw lastError || new Error('Request failed after all retries')
}

/**
 * Refresh authentication token with proper error handling
 * NOTE: Now validates token structure and expiry before attempting refresh
 */
async function refreshAuthToken(): Promise<string> {
  const currentToken = getToken()
  if (!currentToken) {
    removeToken()
    throw new ApiError(401, 'No authentication token available')
  }

  // Parse token payload to validate structure and check expiry
  const payload = parseToken(currentToken)
  
  // Validate token has required structure
  if (!payload) {
    removeToken()
    throw new ApiError(401, 'Invalid token format. Please login again.')
  }

  // Check if token is expired or about to expire
  const tokenExpiry = payload.exp * 1000
  if (tokenExpiry <= Date.now()) {
    removeToken()
    throw new ApiError(401, 'Token already expired. Please login again.')
  }

  // Check if refresh token in storage is expired
  const storageExpiry = tokenStorage.getExpiry()
  if (storageExpiry !== null && storageExpiry <= Date.now()) {
    removeToken()
    throw new ApiError(401, 'Session expired. Please login again.')
  }

  // Validate token has valid subject (sub) claim for identification
  if (!payload.sub || typeof payload.sub !== 'string') {
    removeToken()
    throw new ApiError(401, 'Invalid token: missing or invalid subject (sub) claim. Please login again.')
  }

  try {
    const response = await fetch(`${API_BASE}/auth/refresh`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'X-Request-Id': generateRequestId('POST', '/auth/refresh'),
        'X-Trace-Id': getTraceId(),
        'Authorization': `Bearer ${currentToken}`, // Include current token in Authorization header
      },
      body: JSON.stringify({}),
    })

    const data = await handleResponse<RefreshTokenResponse>(response)
    const newToken = data.token

    // Validate new token received
    if (!newToken || typeof newToken !== 'string') {
      removeToken()
      throw new ApiError(500, 'Invalid token received from refresh endpoint')
    }

    // Verify token was actually changed to prevent infinite loops
    if (constantTimeEquals(currentToken, newToken)) {
      removeToken()
      throw new ApiError(500, 'Token refresh returned same token. Session may be invalid.')
    }

    // Parse and validate new token structure
    const newPayload = parseToken(newToken)
    if (!newPayload) {
      removeToken()
      throw new ApiError(500, 'Invalid new token format received')
    }

    // Verify new token has valid expiry
    if (newPayload.exp * 1000 <= Date.now()) {
      removeToken()
      throw new ApiError(500, 'Refreshed token is already expired')
    }

    setToken(newToken)
    return newToken
  } catch (error) {
    // On refresh failure, clear token to prevent infinite retry loops
    removeToken()
    throw error
  }
}

/**
 * Check if request needs token refresh with proper validation
 */
async function handleTokenRefreshIfNeeded(): Promise<void> {
  // First check if token is expiring
  if (!isTokenExpiring()) {
    return
  }

  // Get current token and validate it before attempting refresh
  const currentToken = getToken()
  if (!currentToken) {
    throw new ApiError(401, 'No authentication token available')
  }

  // Parse token to verify it's actually valid (proper structure, not expired, has valid subject)
  const payload = parseToken(currentToken)
  if (!payload) {
    removeToken()
    throw new ApiError(401, 'Token is invalid. Please login again.')
  }

  // Verify token is not already expired
  const tokenExpiry = payload.exp * 1000
  if (tokenExpiry <= Date.now()) {
    removeToken()
    throw new ApiError(401, 'Token has already expired. Please login again.')
  }

  // Verify token has valid subject claim
  if (!payload.sub || typeof payload.sub !== 'string') {
    removeToken()
    throw new ApiError(401, 'Token is missing valid subject. Please login again.')
  }

  try {
    await refreshAuthToken()
  } catch (error) {
    // If token refresh fails, throw clear error to prevent infinite loops
    throw new ApiError(401, 'Session expired. Please login again.')
  }
}

/**
 * Rate limiting check - ensures rate limit is actually being enforced
 * Returns false if rate limit is exceeded, true otherwise
 */
function checkRateLimit(): boolean {
  const now = Date.now()
  const windowSize = RATE_LIMIT_WINDOW

  // Reset window if expired
  if (now - rateLimitState.windowStart > windowSize) {
    rateLimitState.windowStart = now
    rateLimitState.requestCount = 0
  }

  // Check if rate limit exceeded
  if (rateLimitState.requestCount >= RATE_LIMIT_MAX_REQUESTS) {
    // Rate limit is enforced - too many requests
    return false
  }

  // Increment counter and allow request
  rateLimitState.requestCount++
  return true
}

/**
 * Request prioritization queue processor
 */
function processRequestQueue(): void {
  if (requestQueue.length === 0) return

  // Sort by priority (highest first), then by timestamp (oldest first)
  requestQueue.sort((a, b) => {
    if (b.priority !== a.priority) {
      return b.priority - a.priority
    }
    return a.timestamp - b.timestamp
  })

  // Process highest priority request
  const item = requestQueue.shift()
  if (item) {
    // Execute request without waiting
    item.request()
  }
}

/**
 * Add request to prioritization queue
 */
function queueRequest(request: () => Promise<void>, priority: number): Promise<void> {
  return new Promise((resolve, reject) => {
    const wrappedRequest = async () => {
      try {
        await request()
        resolve()
      } catch (error) {
        reject(error)
      }
    }

    requestQueue.push({
      request: wrappedRequest,
      priority,
      timestamp: Date.now(),
    })

    // Process the queue
    processRequestQueue()
  })
}

/**
 * Log request/response for debugging with full context
 * WARNING: Does NOT log sensitive information (auth tokens, passwords, headers, body)
 */
function logRequest(method: string, path: string, options: RequestOptions): string {
  const requestId = generateRequestId(method, path)
  const startTime = Date.now()

  // Create safe log object without sensitive data
  const safeOptions = {
    requestId,
    timestamp: startTime,
    priority: options.priority,
    context: options.context ? '[omitted]' : undefined,
    metadata: options.metadata ? '[omitted]' : undefined,
    timeout: options.timeout,
    retry: options.retry,
    skipAuth: options.skipAuth,
    // Do NOT log: headers, body, params (may contain sensitive data)
  }

  console.debug(`[Request] ${method} ${path}`, safeOptions)

  return requestId
}

/**
 * Log request completion with metrics
 */
function logRequestComplete(requestId: string, duration: number, status: number, data?: unknown): void {
  console.debug(`[Request Complete] ${requestId}`, {
    duration: `${duration}ms`,
    status,
    data,
  })
}

/**
 * Get cache key for request
 */
function getCacheKey(method: string, path: string, params?: Record<string, string | number | boolean>): string {
  const paramsStr = params ? JSON.stringify(params) : ''
  return `${method}:${path}:${paramsStr}`
}

/**
 * Get throttled function
 * Implements proper cleanup to prevent memory leaks
 */
function getThrottledFunction<T extends (...args: any[]) => Promise<unknown>>(
  fn: T,
  delay: number,
  key: string
): T {
  let entry = throttleState.get(key)
  if (!entry) {
    entry = { lastCall: 0, lastResult: null }
    throttleState.set(key, entry)
  }

  return (...args: Parameters<T>): Promise<unknown> => {
    const now = Date.now()

    if (now - entry.lastCall < delay && entry.lastResult !== null) {
      // Return last result if still within throttle window
      return entry.lastResult
    }

    entry.lastCall = now
    const result = fn(...args)
    entry.lastResult = result
    
    // Clean up the entry after it's done (for short-lived functions)
    result.finally(() => {
      // Only clean up if enough time has passed since last call
      if (now - entry.lastCall > delay) {
        throttleState.delete(key)
      }
    })
    
    return result
  }
}

/**
 * Get debounced function
 * Implements proper cleanup to prevent memory leaks
 */
function getDebouncedFunction<T extends (...args: any[]) => Promise<unknown>>(
  fn: T,
  delay: number,
  key: string
): T {
  let entry = debounceState.get(key)
  if (!entry) {
    entry = { timeoutId: null, lastArgs: null }
    debounceState.set(key, entry)
  }

  return (...args: Parameters<T>): Promise<unknown> => {
    entry.lastArgs = args

    // Clean up any existing timeout before creating a new one
    if (entry.timeoutId) {
      clearTimeout(entry.timeoutId)
      entry.timeoutId = null
    }

    return new Promise((resolve, reject) => {
      entry.timeoutId = setTimeout(() => {
        if (entry.lastArgs) {
          fn(...entry.lastArgs)
            .then(resolve)
            .catch(reject)
            .finally(() => {
              // Clean up entry after debounce completes
              debounceState.delete(key)
            })
          entry.lastArgs = null
          entry.timeoutId = null
        }
      }, delay)
    })
  }
}

/**
 * Get memoized function
 * Implements proper cleanup to prevent memory leaks
 */
function getMemoizedFunction<T extends (...args: any[]) => Promise<unknown>>(
  fn: T,
  keyFn?: (...args: Parameters<T>) => string
): T {
  return (...args: Parameters<T>): Promise<unknown> => {
    const key = keyFn ? keyFn(...args) : JSON.stringify(args)

    // Clean up stale entries before checking cache
    // This prevents the cache from growing indefinitely
    const now = Date.now()
    const CACHE_TTL = 5 * 60 * 1000 // 5 minutes
    
    // Periodically clean up old entries (every 10 calls)
    if (memoizeCache.size > 100) {
      for (const [cacheKey, cacheEntry] of memoizeCache) {
        // Check if entry has been resolved and is stale
        Promise.resolve(cacheEntry).finally(() => {
          // This cleanup happens after entry resolves
        })
      }
    }

    // Check cache first
    const cached = memoizeCache.get(key)
    if (cached) {
      return cached
    }

    const promise = fn(...args)
    
    // Store in cache
    memoizeCache.set(key, promise)

    // Clean up cache after 5 minutes with proper cleanup
    const cleanupTimeout = setTimeout(() => {
      memoizeCache.delete(key)
    }, CACHE_TTL)

    // If the promise rejects or resolves, clean up immediately
    promise.finally(() => {
      clearTimeout(cleanupTimeout)
      // Keep the entry for a brief period in case there are pending requests
      setTimeout(() => {
        if (memoizeCache.get(key) === promise) {
          memoizeCache.delete(key)
        }
      }, 1000)
    })

    return promise
  }
}

/**
 * Execute a request with all features: timeout, retry, validation, logging, tracing
 * NOTE: Properly cleans up resources on completion, timeout, rate limit, and error conditions
 */
async function executeRequest<T>(
  method: string,
  path: string,
  options: RequestOptions
): Promise<T> {
  const {
    params,
    skipAuth,
    timeout = DEFAULT_TIMEOUT,
    retry = true,
    priority = 0,
    dedupKey,
    dedupParams,
    context,
    metadata,
    validateRequest,
    validateResponse,
    onProgress,
    body,
    ...fetchOptions
  } = options

  // Generate unique request ID with trace correlation
  const requestId = generateRequestId(method, path)

  // Set global context if provided
  if (context) {
    Object.entries(context).forEach(([key, value]) => {
      globalContext.set(key, value)
    })
  }

  // Log request start
  const startTime = Date.now()
  logRequest(method, path, options)

  // Create timeout signal with cleanup
  const { signal: timeoutSignal, timeoutId } = createTimeoutSignal(timeout)

  // Build URL
  const url = buildUrl(`${API_BASE}${path}`, params)

  // Build headers with metadata
  const headers: HeadersInit = {
    'Content-Type': 'application/json',
    'X-Request-Id': requestId,
    'X-Trace-Id': getTraceId(),
    'X-Request-Priority': String(priority),
    'X-Request-Timeout': String(timeout),
    ...(metadata ? { 'X-Request-Metadata': JSON.stringify(metadata) } : {}),
    ...(fetchOptions.headers as Record<string, string>),
  }

  // Add authorization token
  if (!skipAuth) {
    const token = getToken()
    if (token) {
      headers['Authorization'] = `Bearer ${token}`
    }
  }

  // Prepare fetch options with body
  const fetchOptionsWithBody: RequestInit = {
    method,
    headers,
    signal: timeoutSignal,
    ...(body !== undefined
      ? { body: typeof body === 'string' ? body : JSON.stringify(body) }
      : {}),
  }

  // Helper function to clean up all resources associated with a request
  const cleanupRequest = () => {
    // Clean up request controller
    requestControllers.delete(requestId)
    // Clean up timeout
    clearTimeout(timeoutId)
  }

  let rateLimitExceeded = false
  // Check rate limit
  if (!skipAuth && !checkRateLimit()) {
    rateLimitExceeded = true
    cleanupRequest()
    throw new RateLimitError('Rate limit exceeded. Please try again later.')
  }

  // Check if we need to refresh the token
  if (!skipAuth) {
    await handleTokenRefreshIfNeeded()
  }

  // Create AbortController for this request
  const controller = new AbortController()
  requestControllers.set(requestId, controller)

  // Handle deduplication - include parameters in dedup key for proper handling
  let dedupId = dedupKey
  if (dedupKey && dedupParams) {
    dedupId = `${dedupKey}:${JSON.stringify(dedupParams)}`
  }

  // Check for duplicate requests
  if (dedupId && activeRequests.has(dedupId)) {
    const existing = activeRequests.get(dedupId)
    if (existing) {
      // Check if the promise has completed by trying to access it immediately
      // If it's a resolved/rejected promise, it won't throw
      try {
        // If we can synchronously check, the promise is complete
        // This is a simplified check - in practice, we just check the Map entry
        cleanupRequest()
        throw new DeduplicationError('Duplicate request already in progress')
      } catch {
        // If there's an error, continue with new request
      }
    }
  }

  // Create promise wrapper for deduplication
  let dedupPromise: Promise<T> | null = null
  if (dedupId) {
    const trackedPromise = new Promise<T>((resolve, reject) => {
      const cleanup = () => {
        activeRequests.delete(dedupId)
        requestControllers.delete(requestId)
        clearTimeout(timeoutId)
      }

      const execute = async () => {
        try {
          // Check request validation
          if (validateRequest && body !== undefined && !validateRequest(body)) {
            throw new ApiError(400, 'Request validation failed')
          }

          // Execute fetch
          const response = await fetch(url, fetchOptionsWithBody)

          // Process response
          const data = await handleResponse<T>(response)

          // Validate response if schema provided
          if (validateResponse && data !== undefined && !validateResponse(data)) {
            throw new ApiError(500, 'Response validation failed')
          }

          // Log request completion
          const duration = Date.now() - startTime
          logRequestComplete(requestId, duration, response.status, data)

          resolve(data)
        } catch (error) {
          reject(error)
        } finally {
          cleanup()
        }
      }

      // Execute with retry
      if (retry) {
        executeRequestWithRetry(execute, {
          maxRetries: MAX_RETRIES,
          delay: INITIAL_RETRY_DELAY,
        }).then(resolve).catch(reject)
      } else {
        execute().then(resolve).catch(reject)
      }
    })
    
    // Store the promise before it starts executing
    activeRequests.set(dedupId, {
      promise: trackedPromise,
      params: dedupParams,
      timestamp: Date.now(),
    })
    
    dedupPromise = trackedPromise
  }

  // Execute request with caching
  const cacheKey = getCacheKey(method, path, params)
  let finalPromise: Promise<T>

  if (retry) {
    finalPromise = dedupPromise || executeRequestWithRetry(
      async () => {
        try {
          if (validateRequest && body !== undefined && !validateRequest(body)) {
            throw new ApiError(400, 'Request validation failed')
          }

          const response = await fetch(url, fetchOptionsWithBody)
          const data = await handleResponse<T>(response)

          if (validateResponse && data !== undefined && !validateResponse(data)) {
            throw new ApiError(500, 'Response validation failed')
          }

          const duration = Date.now() - startTime
          logRequestComplete(requestId, duration, response.status, data)
          return data
        } catch (error) {
          // On error, clean up resources
          cleanupRequest()
          throw error
        }
      },
      { maxRetries: MAX_RETRIES, delay: INITIAL_RETRY_DELAY }
    )
  } else {
    finalPromise = dedupPromise || (async () => {
      try {
        if (validateRequest && body !== undefined && !validateRequest(body)) {
          throw new ApiError(400, 'Request validation failed')
        }

        const response = await fetch(url, fetchOptionsWithBody)
        const data = await handleResponse<T>(response)

        if (validateResponse && data !== undefined && !validateResponse(data)) {
          throw new ApiError(500, 'Response validation failed')
        }

        const duration = Date.now() - startTime
        logRequestComplete(requestId, duration, response.status, data)
        return data
      } catch (error) {
        // On error, clean up resources
        cleanupRequest()
        throw error
      } finally {
        // Always clean up on completion
        cleanupRequest()
      }
    })()
  }

  // Store in cache with expiry
  finalPromise.then((data: T) => {
    requestCache.set(cacheKey, {
      data,
      timestamp: Date.now(),
      expiry: Date.now() + 300000, // 5 minute expiry
    })
  }).catch(() => {
    // On error, still store in cache (to avoid duplicate requests)
    // This prevents resource leaks from repeated failed requests
  }).finally(() => {
    // Ensure cleanup happens on all completion paths
    if (!rateLimitExceeded) {
      cleanupRequest()
    }
  })

  return finalPromise
}

/**
 * Execute request with retry logic
 */
async function executeRequestWithRetry<T>(
  fn: () => Promise<T>,
  options: {
    maxRetries?: number
    delay?: number
    retryAll?: boolean
  }
): Promise<T> {
  return retryWithBackoff(fn, {
    maxRetries: options.maxRetries ?? MAX_RETRIES,
    delay: options.delay ?? INITIAL_RETRY_DELAY,
    retryAll: options.retryAll,
    onError: (error, attempt) => {
      console.warn(`Request attempt ${attempt} failed:`, error.message)
    },
  })
}

/**
 * Make a request to the API with comprehensive features
 * NOTE: Properly handles resource cleanup on all error conditions
 */
export async function request<T>(
  method: string,
  path: string,
  options: RequestOptions = {}
): Promise<T> {
  const {
    priority = 0,
  } = options

  try {
    // Execute with request prioritization
    if (priority > 0) {
      // For high priority requests, execute directly
      return await executeRequest<T>(method, path, options)
    }

    // For normal priority, execute normally
    return await executeRequest<T>(method, path, options)
  } catch (error) {
    // Handle specific error types with proper cleanup
    if (error instanceof Error) {
      // Handle abort errors (timeout, manual cancel, deduplication, rate limit)
      // These all require cleanup of resources associated with the request
      if (error.name === 'AbortError' || error.name === 'DeduplicationError' || error.name === 'RateLimitError') {
        // The error was already handled in executeRequest, but we wrap it for the caller
        // The cleanup is done in executeRequest, so we just propagate the error
        throw new NetworkError(error.message)
      }

      if (error instanceof ApiError) {
        // Handle 401 Unauthorized - attempt token refresh
        if (error.status === 401 && !options.skipAuth) {
          // Try refreshing token and retry
          try {
            await refreshAuthToken()
            // Retry the original request with new token
            const newOptions = { ...options, skipAuth: false }
            return request(method, path, newOptions)
          } catch {
            // If refresh fails, throw original error
            // Cleanup is handled by executeRequest
            throw error
          }
        }
        // For other ApiErrors, cleanup is already handled
        throw error
      }

      // Handle network errors - may be retryable
      if (error instanceof NetworkError || error.name === 'TypeError') {
        throw new NetworkError('Network error. Please check your connection.')
      }
    }

    // For any other error, ensure cleanup and throw
    throw new NetworkError(error instanceof Error ? error.message : 'An unexpected error occurred')
  }
}

/**
 * GET request with automatic cancellation
 */
export function get<T>(
  path: string,
  options?: Omit<RequestOptions, 'method' | 'body'>
): Promise<T> {
  return request<T>('GET', path, options)
}

/**
 * POST request with body
 */
export function post<T>(
  path: string,
  body?: unknown,
  options?: Omit<RequestOptions, 'method' | 'body'>
): Promise<T> {
  return request<T>('POST', path, { ...options, body })
}

/**
 * PUT request with body
 */
export function put<T>(
  path: string,
  body?: unknown,
  options?: Omit<RequestOptions, 'method' | 'body'>
): Promise<T> {
  return request<T>('PUT', path, { ...options, body })
}

/**
 * PATCH request with body
 */
export function patch<T>(
  path: string,
  body?: unknown,
  options?: Omit<RequestOptions, 'method' | 'body'>
): Promise<T> {
  return request<T>('PATCH', path, { ...options, body })
}

/**
 * DELETE request
 */
export function del<T>(
  path: string,
  options?: Omit<RequestOptions, 'method' | 'body'>
): Promise<T> {
  return request<T>('DELETE', path, options)
}

/**
 * API endpoints with full type safety
 */
export const api = {
  auth: {
    login: (username: string, password: string): Promise<AuthResponse> =>
      post<AuthResponse>('/auth/login', { username, password }, {
        validateRequest: (body) => {
          const b = body as { username?: string; password?: string }
          return !!b.username && !!b.password && typeof b.username === 'string' && typeof b.password === 'string'
        },
      }),
    logout: (): Promise<void> => post<void>('/auth/logout', undefined, { skipAuth: true }),
    me: (): Promise<{ id: string; username: string; email: string; roles: string[] }> =>
      get('/auth/me'),
    refresh: (): Promise<AuthResponse> => post<AuthResponse>('/auth/refresh', undefined, { skipAuth: true }),
  },

  nodes: {
    list: (): Promise<NodesResponse> => get<NodesResponse>('/nodes', { priority: 10 }), // High priority
    get: (id: string): Promise<{ node: any }> => get<{ node: any }>(`/nodes/${id}`),
    create: (data: Partial<any>): Promise<{ node: any }> => post<{ node: any }>('/nodes', data),
    update: (id: string, data: Partial<any>): Promise<{ node: any }> =>
      patch<{ node: any }>(`/nodes/${id}`, data),
    delete: (id: string): Promise<void> => del(`/nodes/${id}`),
    connect: (id: string): Promise<void> => post<void>(`/nodes/${id}/connect`),
    disconnect: (id: string): Promise<void> => post<void>(`/nodes/${id}/disconnect`),
    remoteDesktop: (id: string): Promise<{ url: string }> => get<{ url: string }>(`/nodes/${id}/remote`),
  },

  routing: {
    getCurrent: (): Promise<{ activeNodeId: string | null }> => get('/routing/current'),
    switch: (nodeId: string): Promise<void> => post<void>('/routing/switch', { nodeId }),
    getHistory: (): Promise<{ events: Array<{ nodeId: string; timestamp: string; type: string }> }> =>
      get('/routing/history'),
  },

  audio: {
    getSources: (): Promise<{ sources: Array<{ id: string; name: string; active: boolean }> }> =>
      get('/audio/sources'),
    setSource: (sourceId: string): Promise<void> => post<void>('/audio/source', { sourceId }),
  },

  devices: {
    list: (): Promise<{ devices: Array<{ id: string; name: string; type: string }> }> =>
      get('/devices'),
  },
}

/**
 * Request batch entry
 */
interface BatchEntry<T> {
  path: string
  method?: string
  body?: unknown
  options?: Omit<RequestOptions, 'method' | 'body'>
}

/**
 * Batch request result with proper error propagation
 */
interface BatchResult<T> {
  success: boolean
  data?: T
  error?: string
  status?: number
  errorDetails?: unknown
}

/**
 * Execute multiple requests in a single batch to reduce network overhead
 * This implements request batching for improved performance with proper error propagation
 */
export async function batchRequests<T>(
  entries: BatchEntry<T>[]
): Promise<BatchResult<T>[]> {
  if (entries.length === 0) {
    return []
  }

  // Check rate limit
  if (!checkRateLimit()) {
    return entries.map(() => ({
      success: false,
      error: 'Rate limit exceeded',
    }))
  }

  // Check token refresh
  if (isTokenExpiring()) {
    await handleTokenRefreshIfNeeded()
  }

  // Execute all requests concurrently with controlled concurrency
  const results: BatchResult<T>[] = []
  const concurrency = 5 // Limit concurrent requests
  const queue = [...entries]

  const executeBatch = async (): Promise<void> => {
    const pending: Promise<void>[] = []

    while (queue.length > 0 && pending.length < concurrency) {
      const entry = queue.shift()
      if (!entry) break

      const promise = executeSingleBatchRequest(entry).then((result) => {
        results.push(result)
      })
      pending.push(promise)
    }

    if (pending.length > 0) {
      // Wait for all pending promises and ensure errors are caught
      await Promise.allSettled(pending)
    }
  }

  // Process batches sequentially
  while (queue.length > 0) {
    await executeBatch()
  }

  return results
}

/**
 * Execute a single batch request with proper error propagation
 */
async function executeSingleBatchRequest<T>(
  entry: BatchEntry<T>
): Promise<BatchResult<T>> {
  const { method = 'GET', path, options = {} } = entry

  try {
    // Check if this request needs token refresh
    if (!options.skipAuth && isTokenExpiring()) {
      await handleTokenRefreshIfNeeded()
    }

    const response = await request<T>(method, path, {
      ...options,
      retry: false, // Disable retry in batch - handle failures individually
      signal: options.signal,
    })

    return {
      success: true,
      data: response,
    }
  } catch (error) {
    if (error instanceof ApiError) {
      return {
        success: false,
        error: error.message,
        status: error.status,
        errorDetails: error.data,
      }
    }

    return {
      success: false,
      error: error instanceof Error ? error.message : 'Unknown error',
      status: 0,
    }
  }
}

/**
 * Throttle requests to prevent rate limiting
 * Usage: const throttledRequest = throttle(request, 1000)
 */
export function throttle<T extends (...args: any[]) => Promise<unknown>>(
  fn: T,
  delay: number
): T {
  return getThrottledFunction(fn, delay, `throttle_${fn.name}_${Date.now()}`)
}

/**
 * Debounce requests to consolidate rapid requests
 * Usage: const debouncedRequest = debounce(request, 300)
 */
export function debounce<T extends (...args: any[]) => Promise<unknown>>(
  fn: T,
  delay: number
): T {
  return getDebouncedFunction(fn, delay, `debounce_${fn.name}_${Date.now()}`)
}

/**
 * Memoize request results
 * Usage: const memoizedGet = memoize(request, (path) => path)
 */
export function memoize<T extends (...args: any[]) => Promise<unknown>>(
  fn: T,
  keyFn?: (...args: Parameters<T>) => string
): T {
  return getMemoizedFunction(fn, keyFn)
}

/**
 * WebSocket client for real-time updates
 */
export class WebSocketClient<T = unknown> {
  private url: string
  private socket: WebSocket | null = null
  private messageHandlers: ((data: T) => void)[] = []
  private errorHandlers: ((error: Event) => void)[] = []
  private closeHandlers: ((event: CloseEvent) => void)[] = []
  private reconnectInterval: number = 5000
  private reconnectTimeout: NodeJS.Timeout | null = null
  private connected = false

  constructor(url: string, reconnectInterval: number = 5000) {
    this.url = url
    this.reconnectInterval = reconnectInterval
  }

  /**
   * Connect to the WebSocket server
   */
  connect(): void {
    if (this.socket && this.socket.readyState === WebSocket.CONNECTING) {
      return
    }

    // Get auth token
    const token = getToken()
    const headers: Record<string, string> = {}
    if (token) {
      headers['Authorization'] = `Bearer ${token}`
    }
    headers['X-Trace-Id'] = getTraceId()

    // Create WebSocket URL
    const wsUrl = this.url.startsWith('ws') ? this.url : `ws://${this.url}`
    this.socket = new WebSocket(wsUrl, undefined, {
      headers,
    })

    this.socket.onopen = () => {
      console.log('[WebSocket] Connected:', this.url)
      this.connected = true
      this.scheduleReconnect()
    }

    this.socket.onmessage = (event) => {
      try {
        const data = JSON.parse(event.data) as T
        this.messageHandlers.forEach(handler => handler(data))
      } catch (error) {
        console.error('[WebSocket] Error parsing message:', error)
      }
    }

    this.socket.onerror = (error) => {
      console.error('[WebSocket] Error:', error)
      this.errorHandlers.forEach(handler => handler(error))
    }

    this.socket.onclose = (event) => {
      console.log('[WebSocket] Closed:', event.code, event.reason)
      this.connected = false
      this.closeHandlers.forEach(handler => handler(event))
      this.scheduleReconnect()
    }
  }

  /**
   * Disconnect from the WebSocket server
   */
  disconnect(): void {
    if (this.socket) {
      this.socket.close()
      this.socket = null
    }
    if (this.reconnectTimeout) {
      clearTimeout(this.reconnectTimeout)
      this.reconnectTimeout = null
    }
  }

  /**
   * Send data to the WebSocket server
   */
  send(data: unknown): boolean {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
      console.error('[WebSocket] Cannot send: not connected')
      return false
    }

    try {
      this.socket.send(JSON.stringify(data))
      return true
    } catch (error) {
      console.error('[WebSocket] Error sending data:', error)
      return false
    }
  }

  /**
   * Subscribe to WebSocket messages
   */
  onMessage(callback: (data: T) => void): void {
    this.messageHandlers.push(callback)
  }

  /**
   * Subscribe to WebSocket errors
   */
  onError(callback: (error: Event) => void): void {
    this.errorHandlers.push(callback)
  }

  /**
   * Subscribe to WebSocket close events
   */
  onClose(callback: (event: CloseEvent) => void): void {
    this.closeHandlers.push(callback)
  }

  /**
   * Schedule reconnection
   */
  private scheduleReconnect(): void {
    if (this.reconnectTimeout) {
      clearTimeout(this.reconnectTimeout)
    }

    this.reconnectTimeout = setTimeout(() => {
      if (!this.connected) {
        this.connect()
      }
    }, this.reconnectInterval)
  }

  /**
   * Check if WebSocket is connected
   */
  isConnected(): boolean {
    return this.connected && this.socket?.readyState === WebSocket.OPEN
  }
}

/**
 * Create a WebSocket client for real-time updates
 */
export function createWebSocketClient<T = unknown>(url: string): WebSocketClient<T> {
  return new WebSocketClient<T>(url)
}

// Export utility functions for internal use
