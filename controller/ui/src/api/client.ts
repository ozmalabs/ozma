import { ApiResponse, AuthResponse, NodesResponse } from '../types/api'

const API_BASE = '/api/v1'

interface RequestOptions extends RequestInit {
  params?: Record<string, string | number | boolean>
  skipAuth?: boolean
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

/**
 * Get the authentication token from localStorage
 */
export function getToken(): string | null {
  if (typeof localStorage === 'undefined') return null
  return localStorage.getItem('ozma_token')
}

/**
 * Set the authentication token in localStorage
 */
export function setToken(token: string): void {
  if (typeof localStorage === 'undefined') return
  localStorage.setItem('ozma_token', token)
}

/**
 * Remove the authentication token from localStorage
 */
export function removeToken(): void {
  if (typeof localStorage === 'undefined') return
  localStorage.removeItem('ozma_token')
}

/**
 * Check if user is authenticated
 */
export function isAuthenticated(): boolean {
  const token = getToken()
  if (!token) return false

  try {
    const payload = JSON.parse(atob(token.split('.')[1]))
    const expiration = payload.exp * 1000
    return Date.now() < expiration
  } catch {
    return false
  }
}

/**
 * Build URL with query parameters
 */
function buildUrl(base: string, params?: Record<string, string | number | boolean>): string {
  if (!params) return base
  const url = new URL(base, window.location.origin)
  Object.entries(params).forEach(([key, value]) => {
    if (value !== undefined && value !== null) {
      url.searchParams.append(key, String(value))
    }
  })
  return url.pathname + url.search
}

/**
 * Handle response and throw errors for non-OK responses
 */
async function handleResponse<T>(response: Response): Promise<T> {
  const contentType = response.headers.get('content-type')
  if (!contentType || !contentType.includes('application/json')) {
    throw new ApiError(response.status, 'Invalid response format')
  }

  const data = await response.json().catch(() => ({}))

  if (!response.ok) {
    const message = data?.error || data?.message || `HTTP ${response.status}`
    throw new ApiError(response.status, message, data)
  }

  return data as T
}

/**
 * Make a request to the API
 */
export async function request<T>(
  method: string,
  path: string,
  options: RequestOptions = {}
): Promise<T> {
  const { params, skipAuth, ...fetchOptions } = options
  const url = buildUrl(`${API_BASE}${path}`, params)

  const headers: HeadersInit = {
    'Content-Type': 'application/json',
    ...(fetchOptions.headers || {}),
  }

  if (!skipAuth) {
    const token = getToken()
    if (token) {
      headers['Authorization'] = `Bearer ${token}`
    }
  }

  const response = await fetch(url, {
    method,
    headers,
    ...fetchOptions,
    body: options.body ? JSON.stringify(options.body) : undefined,
  })

  return handleResponse<T>(response)
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
 * API endpoints
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
