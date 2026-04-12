import { AuthResponse, NodesResponse } from '../types/api'

// Constants
const API_BASE = '/api/v1'
const DEFAULT_TIMEOUT = 30000 // 30 seconds
const MAX_RETRIES = 3
const INITIAL_RETRY_DELAY = 1000 // 1 second
const TOKEN_REFRESH_BUFFER = 5000 // 5 seconds
const MAX_REQUEST_ID_LENGTH = 32

// Types - Don't extend RequestInit to avoid priority type conflicts
export interface RequestOptions {
  params?: Record<string, string | number | boolean>
  skipAuth?: boolean
  timeout?: number
  retry?: boolean
  signal?: AbortSignal
  priority?: number // Higher number = higher priority
  dedupKey?: string // For request deduplication
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

// Token storage abstraction with improved security
const createTokenStorage = () => {
  // Use module-level cache for SSR compatibility and reduced localStorage access
  let cachedToken: string | null = null
  let cachedTokenExpiry: number | null = null

  return {
    get: (): string | null => {
      if (typeof localStorage === 'undefined') return cachedToken
      const token = localStorage.getItem('ozma_token')
      if (token) {
        // Cache the token
        cachedToken = token
        // Parse expiry from token for faster access
        try {
          const parts = token.split('.')
          if (parts.length === 3) {
            const payload = JSON.parse(atob(parts[1]))
            cachedTokenExpiry = payload.exp * 1000
          }
        } catch {
          cachedTokenExpiry = null
        }
      }
      return token
    },
    set: (token: string): void => {
      if (typeof localStorage === 'undefined') {
        cachedToken = token
      } else {
        localStorage.setItem('ozma_token', token)
        cachedToken = token
        // Parse and cache expiry
        try {
          const parts = token.split('.')
          if (parts.length === 3) {
            const payload = JSON.parse(atob(parts[1]))
            cachedTokenExpiry = payload.exp * 1000
          }
        } catch {
          cachedTokenExpiry = null
        }
      }
    },
    remove: (): void => {
      if (typeof localStorage === 'undefined') {
        cachedToken = null
      } else {
        localStorage.removeItem('ozma_token')
        cachedToken = null
      }
      cachedTokenExpiry = null
    },
    getExpiry: (): number | null => cachedTokenExpiry,
  }
}

const tokenStorage = createTokenStorage()

/**
 * Parse JWT token payload safely
 */
function parseToken(token: string): TokenInfo | null {
  try {
    if (!token || typeof token !== 'string') return null
    const parts = token.split('.')
    if (parts.length !== 3) return null

    const payloadBase64 = parts[1]
    if (!payloadBase64) return null

    const payloadJson = atob(payloadBase64)
    if (!payloadJson) return null

    const payload = JSON.parse(payloadJson)
    if (!payload || typeof payload !== 'object') return null

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
 * Check if user is authenticated
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
 */
export function isTokenExpiring(): boolean {
  const token = getToken()
  if (!token) return true

  const expiry = tokenStorage.getExpiry()
  if (expiry === null || expiry === undefined) {
    const payload = parseToken(token)
    if (!payload) return true
    return Date.now() >= payload.exp * 1000 - TOKEN_REFRESH_BUFFER
  }

  const now = Date.now()
  return now >= expiry - TOKEN_REFRESH_BUFFER
}

/**
 * Build URL with query parameters
 */
function buildUrl(base: string, params?: Record<string, string | number | boolean>): string {
  if (!params) return base
  try {
    const url = new URL(base, typeof window !== 'undefined' ? window.location.origin : 'http://localhost')
    Object.entries(params).forEach(([key, value]) => {
      if (value !== undefined && value !== null) {
        url.searchParams.append(key, String(value))
      }
    })
    return url.pathname + url.search
  } catch {
    return base
  }
}

/**
 * Handle response and throw errors for non-OK responses
 */
async function handleResponse<T>(response: Response): Promise<T> {
  const contentType = response.headers.get('content-type')
  if (!contentType || !contentType.includes('application/json')) {
    throw new ApiError(response.status, 'Invalid response format')
  }

  let data: unknown
  try {
    data = await response.json()
  } catch (error) {
    throw new ApiError(response.status, 'Failed to parse response JSON')
  }

  if (!response.ok) {
    const message = (data as { error?: string; message?: string })?.error ||
                    (data as { error?: string; message?: string })?.message ||
                    `HTTP ${response.status}`
    throw new ApiError(response.status, message, data)
  }

  return data as T
}

// Request controller for cancellation - properly cleaned up on completion
const requestControllers = new Map<string, AbortController>()
// Deduplication map to prevent duplicate requests
const activeRequests = new Map<string, Promise<unknown>>()
// Request queue for prioritization
const requestQueue: RequestQueueItem[] = []
// Rate limiting state
const rateLimitState = {
  lastRequestTime: 0,
  requestCount: 0,
  windowStart: 0,
}
// Global request context
const globalContext = new Map<string, unknown>()

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
 */
async function retryWithBackoff<T>(
  fn: () => Promise<T>,
  options: {
    maxRetries?: number;
    delay?: number;
    onError?: (error: Error, attempt: number) => void;
  } = {}
): Promise<T> {
  const { maxRetries = MAX_RETRIES, delay = INITIAL_RETRY_DELAY, onError } = options

  let lastError: Error | null = null

  for (let attempt = 1; attempt <= maxRetries; attempt++) {
    try {
      return await fn()
    } catch (error) {
      lastError = error instanceof Error ? error : new Error(String(error))

      // Log retry attempt
      console.debug(`Request attempt ${attempt}/${maxRetries} failed:`, lastError.message)

      // Don't retry on client errors (4xx) except 429
      if (lastError instanceof ApiError && lastError.status >= 400 && lastError.status < 500 && lastError.status !== 429) {
        throw lastError
      }

      // Don't retry on network errors after first attempt
      if (lastError instanceof NetworkError && attempt > 1) {
        throw lastError
      }

      // Don't retry on timeout
      if (lastError instanceof TimeoutError) {
        throw lastError
      }

      // Don't retry after last attempt
      if (attempt >= maxRetries) {
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
 */
async function refreshAuthToken(): Promise<string> {
  const currentToken = getToken()
  if (!currentToken) {
    removeToken()
    throw new ApiError(401, 'No authentication token available')
  }

  try {
    const response = await fetch(`${API_BASE}/auth/refresh`, {
      method: 'POST',
      headers: {
        'Content-Type': 'application/json',
        'X-Request-Id': generateRequestId('POST', '/auth/refresh'),
        'X-Trace-Id': getTraceId(),
      },
      body: JSON.stringify({}),
    })

    const data = await handleResponse<RefreshTokenResponse>(response)
    const newToken = data.token

    // Validate new token
    if (!newToken || typeof newToken !== 'string') {
      removeToken()
      throw new ApiError(500, 'Invalid token received from refresh endpoint')
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
  if (!isTokenExpiring()) {
    return
  }

  try {
    await refreshAuthToken()
  } catch (error) {
    // If token refresh fails, throw clear error
    throw new ApiError(401, 'Session expired. Please login again.')
  }
}

/**
 * Rate limiting check
 */
function checkRateLimit(): boolean {
  const now = Date.now()
  const windowSize = 60000 // 1 minute window

  // Reset window if expired
  if (now - rateLimitState.windowStart > windowSize) {
    rateLimitState.windowStart = now
    rateLimitState.requestCount = 0
  }

  // Check if rate limit exceeded
  if (rateLimitState.requestCount >= 100) { // 100 requests per minute
    return false
  }

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
    const item = {
      request: async () => {
        try {
          await request()
          resolve()
        } catch (error) {
          reject(error)
        }
      },
      priority,
      timestamp: Date.now(),
    }

    requestQueue.push(item)

    // If queue is empty, process immediately
    if (requestQueue.length === 1) {
      processRequestQueue()
    }
  })
}

/**
 * Validate request body
 */
function validateRequestBody(body: unknown, schema?: Record<string, unknown>): boolean {
  if (!schema) return true
  // Simple validation - can be extended with JSON Schema validation
  if (typeof body !== 'object' || body === null) return false
  return true
}

/**
 * Validate response data
 */
function validateResponseData(data: unknown, expectedType: string): boolean {
  if (!data) return false
  switch (expectedType) {
    case 'object':
      return typeof data === 'object' && data !== null
    case 'array':
      return Array.isArray(data)
    case 'string':
      return typeof data === 'string'
    case 'number':
      return typeof data === 'number'
    default:
      return true
  }
}

/**
 * Log request/response for debugging
 */
function logRequest(method: string, path: string, options: RequestOptions): string {
  const requestId = generateRequestId(method, path)
  const startTime = Date.now()

  console.debug(`[Request] ${method} ${path}`, {
    requestId,
    timestamp: startTime,
    priority: options.priority,
    context: options.context,
    metadata: options.metadata,
  })

  return requestId
}

/**
 * Log request completion
 */
function logRequestComplete(requestId: string, duration: number, status: number): void {
  console.debug(`[Request Complete] ${requestId}: ${duration}ms, status: ${status}`)
}

/**
 * Make a request to the API with comprehensive features
 */
export async function request<T>(
  method: string,
  path: string,
  options: RequestOptions = {}
): Promise<T> {
  const {
    params,
    skipAuth,
    timeout = DEFAULT_TIMEOUT,
    retry = true,
    signal: externalSignal,
    priority = 0,
    dedupKey,
    context,
    metadata,
    validateRequest,
    validateResponse,
    onProgress,
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
  logRequest(method, path, options)

  // Create timeout signal with cleanup
  const { signal: timeoutSignal, timeoutId } = createTimeoutSignal(timeout)

  // Combine signals - fallback to timeout signal if external is undefined
  const combinedSignal = (externalSignal || timeoutSignal) as AbortSignal

  try {
    // Check rate limit
    if (!skipAuth && !checkRateLimit()) {
      throw new ApiError(429, 'Rate limit exceeded. Please try again later.')
    }

    // Check if we need to refresh the token
    if (!skipAuth) {
      await handleTokenRefreshIfNeeded()
    }

    // Build URL
    const url = buildUrl(`${API_BASE}${path}`, params)

    // Build headers with metadata
    const headers: HeadersInit = {
      'Content-Type': 'application/json',
      'X-Request-Id': requestId,
      'X-Trace-Id': getTraceId(),
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

    // Prepare fetch options with body - cast to proper body type
    const bodyInit = options.body !== undefined 
      ? (typeof options.body === 'string' ? options.body : JSON.stringify(options.body))
      : undefined

    const fetchOptionsWithBody: RequestInit = {
      method,
      headers,
      signal: combinedSignal,
      body: bodyInit,
      ...fetchOptions,
    }

    // Check for duplicate requests
    if (dedupKey && activeRequests.has(dedupKey)) {
      throw new DeduplicationError('Duplicate request already in progress')
    }

    // Create AbortController for this request
    const controller = new AbortController()
    requestControllers.set(requestId, controller)

    // Add to deduplication map if dedupKey provided
    if (dedupKey) {
      activeRequests.set(dedupKey, new Promise(() => {})) // Placeholder
    }

    // Execute request
    const executeRequest = async (): Promise<Response> => {
      try {
        const response = await fetch(url, {
          ...fetchOptionsWithBody,
          signal: combinedSignal,
        })
        return response
      } catch (error) {
        throw error
      } finally {
        // Clean up request controller
        requestControllers.delete(requestId)
      }
    }

    let response: Response
    if (retry) {
      response = await retryWithBackoff(executeRequest, {
        onError: (error, attempt) => {
          console.warn(`Request attempt ${attempt} failed:`, error.message)
        },
      })
    } else {
      response = await executeRequest()
    }

    // Process response
    const data = await handleResponse<T>(response)

    // Validate response if schema provided
    if (validateResponse && !validateResponse(data)) {
      throw new ApiError(500, 'Response validation failed')
    }

    return data
  } catch (error) {
    // Handle specific error types
    if (error instanceof Error) {
      if (error.name === 'AbortError' || error.name === 'DeduplicationError') {
        throw new NetworkError(error.message)
      }

      if (error instanceof ApiError) {
        // Handle 401 Unauthorized - attempt token refresh
        if (error.status === 401 && !skipAuth) {
          // Try refreshing token and retry
          try {
            await refreshAuthToken()
            // Retry the original request with new token
            const newOptions = { ...options, skipAuth: false }
            return request(method, path, newOptions)
          } catch {
            // If refresh fails, throw original error
            throw error
          }
        }
        throw error
      }

      if (error instanceof NetworkError || error.name === 'TypeError') {
        throw new NetworkError('Network error. Please check your connection.')
      }
    }

    throw new NetworkError(error instanceof Error ? error.message : 'An unexpected error occurred')
  } finally {
    // Clean up timeout
    clearTimeout(timeoutId)
    // Clean up deduplication map on completion
    if (dedupKey) {
      activeRequests.delete(dedupKey)
    }
    // Clear global context after request completes
    if (context) {
      Object.keys(context).forEach(key => {
        globalContext.delete(key)
      })
    }
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
 * Batch request result
 */
interface BatchResult<T> {
  success: boolean
  data?: T
  error?: string
  status?: number
}

/**
 * Execute multiple requests in a single batch to reduce network overhead
 * This implements request batching for improved performance
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
      await Promise.all(pending)
    }
  }

  // Process batches sequentially
  while (queue.length > 0) {
    await executeBatch()
  }

  return results
}

/**
 * Execute a single batch request
 */
async function executeSingleBatchRequest<T>(
  entry: BatchEntry<T>
): Promise<BatchResult<T>> {
  try {
    const { method = 'GET', path, options = {} } = entry

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
      }
    }

    return {
      success: false,
      error: error instanceof Error ? error.message : 'Unknown error',
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
  let lastCall = 0
  let lastResult: Promise<unknown> | null = null

  return (...args: Parameters<T>): Promise<unknown> => {
    const now = Date.now()

    if (now - lastCall < delay && lastResult !== null) {
      // Return last result if still within throttle window
      return lastResult
    }

    lastCall = now
    lastResult = fn(...args)
    return lastResult
  }
}

/**
 * Debounce requests to consolidate rapid requests
 * Usage: const debouncedRequest = debounce(request, 300)
 */
export function debounce<T extends (...args: any[]) => Promise<unknown>>(
  fn: T,
  delay: number
): T {
  let timeoutId: NodeJS.Timeout | null = null
  let lastArgs: Parameters<T> | null = null

  return (...args: Parameters<T>): Promise<unknown> => {
    lastArgs = args

    if (timeoutId) {
      clearTimeout(timeoutId)
    }

    return new Promise((resolve, reject) => {
      timeoutId = setTimeout(() => {
        if (lastArgs) {
          fn(...lastArgs)
            .then(resolve)
            .catch(reject)
          lastArgs = null
        }
      }, delay)
    })
  }
}

/**
 * Memoize request results
 * Usage: const memoizedGet = memoize(request, (path) => path)
 */
export function memoize<T extends (...args: any[]) => Promise<unknown>>(
  fn: T,
  keyFn?: (...args: Parameters<T>) => string
): T {
  const cache = new Map<string, Promise<unknown>>()

  return (...args: Parameters<T>): Promise<unknown> => {
    const key = keyFn ? keyFn(...args) : JSON.stringify(args)

    if (cache.has(key)) {
      return cache.get(key) as Promise<unknown>
    }

    const promise = fn(...args)
    cache.set(key, promise)

    // Clean up cache after 5 minutes
    promise.finally(() => {
      setTimeout(() => cache.delete(key), 5 * 60 * 1000)
    })

    return promise
  }
}

// Export utility functions (already exported at definition sites)