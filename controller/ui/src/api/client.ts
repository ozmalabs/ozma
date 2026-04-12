import { AuthResponse, NodesResponse } from '../types/api'

// Constants
const API_BASE = '/api/v1'
const DEFAULT_TIMEOUT = 30000 // 30 seconds
const MAX_RETRIES = 3
const RETRY_DELAY = 1000 // 1 second
const TOKEN_REFRESH_BUFFER = 5000 // 5 seconds

// Types
interface RequestOptions extends RequestInit {
  params?: Record<string, string | number | boolean>
  skipAuth?: boolean
  timeout?: number
  retry?: boolean
  signal?: AbortSignal
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

class ApiError extends Error {
  constructor(
    public status: number,
    message: string,
    public data?: unknown
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

class NetworkError extends Error {
  constructor(message: string = 'Network error') {
    super(message)
    this.name = 'NetworkError'
  }
}

class TimeoutError extends Error {
  constructor(message: string = 'Request timeout') {
    super(message)
    this.name = 'TimeoutError'
  }
}

// Token storage abstraction (works in SSR and browser)
const createTokenStorage = () => {
  // Use module-level cache for SSR compatibility
  let cachedToken: string | null = null

  return {
    get: (): string | null => {
      if (typeof localStorage === 'undefined') return cachedToken
      return localStorage.getItem('ozma_token')
    },
    set: (token: string): void => {
      if (typeof localStorage === 'undefined') {
        cachedToken = token
      } else {
        localStorage.setItem('ozma_token', token)
      }
    },
    remove: (): void => {
      if (typeof localStorage === 'undefined') {
        cachedToken = null
      } else {
        localStorage.removeItem('ozma_token')
      }
    },
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

  const payload = parseToken(token)
  if (!payload) return true

  const expiration = payload.exp * 1000
  const now = Date.now()
  
  // Token is expiring if it's within the buffer time of expiring
  // or if it has already expired
  return now >= expiration - TOKEN_REFRESH_BUFFER
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

// Request controller for cancellation
const requestControllers = new Map<string, AbortController>()

/**
 * Cancel a pending request
 */
export function cancelRequest(requestId: string): void {
  const controller = requestControllers.get(requestId)
  if (controller) {
    controller.abort()
    requestControllers.delete(requestId)
  }
}

/**
 * Create a timeout signal
 */
function createTimeoutSignal(timeout: number): { signal: AbortSignal; timeoutId: NodeJS.Timeout } {
  const controller = new AbortController()
  const timeoutId = setTimeout(() => controller.abort(), timeout)
  return { signal: controller.signal, timeoutId }
}

/**
 * Retry strategy with exponential backoff
 */
async function retryWithBackoff<T>(
  fn: () => Promise<T>,
  options: { maxRetries?: number; delay?: number } = {}
): Promise<T> {
  const { maxRetries = MAX_RETRIES, delay = RETRY_DELAY } = options
  
  let lastError: Error | null = null
  
  for (let attempt = 1; attempt <= maxRetries; attempt++) {
    try {
      return await fn()
    } catch (error) {
      lastError = error instanceof Error ? error : new Error(String(error))
      
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
      
      if (attempt < maxRetries) {
        // Exponential backoff
        const backoffDelay = delay * Math.pow(2, attempt - 1)
        await new Promise(resolve => setTimeout(resolve, backoffDelay))
      }
    }
  }
  
  throw lastError || new Error('Request failed after all retries')
}

/**
 * Refresh authentication token
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
      },
      body: JSON.stringify({}),
    })

    const data = await handleResponse<RefreshTokenResponse>(response)
    const newToken = data.token

    setToken(newToken)
    return newToken
  } catch (error) {
    removeToken()
    throw error
  }
}

/**
 * Check if request needs token refresh
 */
async function handleTokenRefreshIfNeeded(): Promise<void> {
  if (isTokenExpiring()) {
    await refreshAuthToken()
  }
}

/**
 * Make a request to the API with retry, timeout, and cancellation support
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
    ...fetchOptions
  } = options

  // Generate unique request ID
  const requestId = `${method}-${path}-${Date.now()}-${Math.random().toString(36).substring(2, 11)}`
  
  // Create timeout signal
  const { signal: timeoutSignal, timeoutId } = createTimeoutSignal(timeout)
  
  // Combine signals - fallback to timeout signal if external is undefined
  const combinedSignal = (externalSignal || timeoutSignal) as AbortSignal

  try {
    // Check if we need to refresh the token
    if (!skipAuth) {
      await handleTokenRefreshIfNeeded()
    }

    const url = buildUrl(`${API_BASE}${path}`, params)

    const headers: HeadersInit = {
      'Content-Type': 'application/json',
      ...(fetchOptions.headers as Record<string, string>),
    }

    if (!skipAuth) {
      const token = getToken()
      if (token) {
        headers['Authorization'] = `Bearer ${token}`
      }
    }

    // Prepare fetch options
    const fetchOptionsWithBody: RequestInit = {
      method,
      headers,
      signal: combinedSignal,
      ...fetchOptions,
    }

    if (options.body !== undefined) {
      fetchOptionsWithBody.body = JSON.stringify(options.body)
    }

    // Execute request with retry if enabled
    const executeRequest = async (): Promise<Response> => {
      requestControllers.set(requestId, new AbortController())
      try {
        const response = await fetch(url, fetchOptionsWithBody)
        requestControllers.delete(requestId)
        return response
      } catch (error) {
        requestControllers.delete(requestId)
        throw error
      }
    }

    let response: Response
    if (retry) {
      response = await retryWithBackoff(executeRequest)
    } else {
      response = await executeRequest()
    }

    return handleResponse<T>(response)
  } catch (error) {
    // Handle specific error types
    if (error instanceof Error) {
      if (error.name === 'AbortError') {
        throw new NetworkError('Request was cancelled')
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
  }
}

/**
 * GET request
 */
export function get<T>(path: string, options?: Omit<RequestOptions, 'method' | 'body'>): Promise<T> {
  return request<T>('GET', path, options)
}

/**
 * POST request
 */
export function post<T>(
  path: string,
  body?: unknown,
  options?: Omit<RequestOptions, 'method' | 'body'>
): Promise<T> {
  return request<T>('POST', path, { ...options, body })
}

/**
 * PUT request
 */
export function put<T>(
  path: string,
  body?: unknown,
  options?: Omit<RequestOptions, 'method' | 'body'>
): Promise<T> {
  return request<T>('PUT', path, { ...options, body })
}

/**
 * PATCH request
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
      post<AuthResponse>('/auth/login', { username, password }),
    logout: (): Promise<void> => post<void>('/auth/logout', undefined, { skipAuth: true }),
    me: (): Promise<{ id: string; username: string; email: string; roles: string[] }> =>
      get('/auth/me'),
    refresh: (): Promise<AuthResponse> => post<AuthResponse>('/auth/refresh', undefined, { skipAuth: true }),
  },

  nodes: {
    list: (): Promise<NodesResponse> => get<NodesResponse>('/nodes'),
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
