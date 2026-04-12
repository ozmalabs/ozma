import { AuthResponse, NodesResponse } from '../types/api'
import { tokenStorage } from '../auth/tokenStorage'
import { isTokenExpired, isTokenExpiringSoon, isTokenValid, constantTimeEquals, parseToken } from '../auth/tokenUtils'

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------
const API_BASE = '/api/v1'
const DEFAULT_TIMEOUT = 30_000
const MAX_RETRIES = 3
const INITIAL_RETRY_DELAY = 1_000

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface RequestOptions {
  params?: Record<string, string | number | boolean>
  skipAuth?: boolean
  timeout?: number
  retry?: boolean
  signal?: AbortSignal
  body?: unknown
}

interface RefreshTokenResponse {
  token: string
  expires_at: string
}

// ---------------------------------------------------------------------------
// Error classes
// ---------------------------------------------------------------------------

export class ApiError extends Error {
  constructor(
    public readonly status: number,
    message: string,
    public readonly data?: unknown,
  ) {
    super(message)
    this.name = 'ApiError'
  }
}

export class NetworkError extends Error {
  constructor(message = 'Network error') {
    super(message)
    this.name = 'NetworkError'
  }
}

export class TimeoutError extends Error {
  constructor(message = 'Request timed out') {
    super(message)
    this.name = 'TimeoutError'
  }
}

// ---------------------------------------------------------------------------
// Token helpers (thin wrappers over tokenStorage / tokenUtils)
// ---------------------------------------------------------------------------

export function getToken(): string | null {
  return tokenStorage.get()
}

export function setToken(token: string): void {
  tokenStorage.set(token)
}

export function removeToken(): void {
  tokenStorage.remove()
}

export function isAuthenticated(): boolean {
  const token = tokenStorage.get()
  return token !== null && isTokenValid(token)
}

// ---------------------------------------------------------------------------
// URL builder
// ---------------------------------------------------------------------------

function buildUrl(base: string, params?: Record<string, string | number | boolean>): string {
  if (!params || Object.keys(params).length === 0) return base
  const url = new URL(base, 'http://localhost') // dummy origin for relative URLs
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null) url.searchParams.append(k, String(v))
  }
  // Return path + search only (strip dummy origin)
  return url.pathname + url.search
}

// ---------------------------------------------------------------------------
// Response handler
// ---------------------------------------------------------------------------

async function handleResponse<T>(res: Response): Promise<T> {
  const ct = res.headers.get('content-type') ?? ''
  const isJson = ct.includes('application/json')

  if (!res.ok) {
    let message = `HTTP ${res.status}`
    let data: unknown = null
    if (isJson) {
      try {
        data = await res.json()
        const d = data as { error?: string; message?: string; detail?: string }
        message = d.error ?? d.message ?? d.detail ?? message
      } catch { /* ignore */ }
    } else {
      try {
        const text = await res.text()
        if (text) message = text.slice(0, 500)
      } catch { /* ignore */ }
    }
    throw new ApiError(res.status, message, data)
  }

  if (isJson) {
    try { return (await res.json()) as T } catch { return null as T }
  }

  try {
    const text = await res.text()
    if (text.trimStart().startsWith('{') || text.trimStart().startsWith('[')) {
      try { return JSON.parse(text) as T } catch { /* fall through */ }
    }
    return text as T
  } catch {
    return null as T
  }
}

// ---------------------------------------------------------------------------
// Token refresh (singleton in-flight promise to avoid races)
// ---------------------------------------------------------------------------

let _refreshPromise: Promise<string> | null = null

async function refreshAuthToken(): Promise<string> {
  const currentToken = tokenStorage.get()
  if (!currentToken) {
    tokenStorage.remove()
    throw new ApiError(401, 'No token available. Please log in.')
  }

  const payload = parseToken(currentToken)
  if (!payload) {
    tokenStorage.remove()
    throw new ApiError(401, 'Invalid token. Please log in again.')
  }
  if (payload.exp * 1000 <= Date.now()) {
    tokenStorage.remove()
    throw new ApiError(401, 'Session expired. Please log in again.')
  }

  const res = await fetch(`${API_BASE}/auth/refresh`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Authorization: `Bearer ${currentToken}`,
    },
    body: JSON.stringify({}),
  })

  const data = await handleResponse<RefreshTokenResponse>(res)
  const newToken = data?.token

  if (!newToken || typeof newToken !== 'string') {
    tokenStorage.remove()
    throw new ApiError(500, 'Invalid token received from refresh endpoint.')
  }

  if (constantTimeEquals(currentToken, newToken)) {
    tokenStorage.remove()
    throw new ApiError(500, 'Token refresh returned the same token.')
  }

  const newPayload = parseToken(newToken)
  if (!newPayload || newPayload.exp * 1000 <= Date.now()) {
    tokenStorage.remove()
    throw new ApiError(500, 'Refreshed token is invalid or already expired.')
  }

  tokenStorage.set(newToken)
  return newToken
}

async function ensureFreshToken(): Promise<void> {
  const token = tokenStorage.get()
  if (!token) return // unauthenticated — let the request fail naturally with 401

  // Token is fully expired — clear it and surface a clean 401
  if (isTokenExpired(token)) {
    tokenStorage.remove()
    throw new ApiError(401, 'Session expired. Please log in again.')
  }

  if (!isTokenExpiringSoon(token)) return

  if (!_refreshPromise) {
    _refreshPromise = refreshAuthToken().finally(() => {
      _refreshPromise = null
    })
  }

  try {
    await _refreshPromise
  } catch {
    throw new ApiError(401, 'Session expired. Please log in again.')
  }
}

// ---------------------------------------------------------------------------
// Retry with exponential backoff + jitter
// ---------------------------------------------------------------------------

async function withRetry<T>(fn: () => Promise<T>, maxRetries = MAX_RETRIES): Promise<T> {
  let lastErr: unknown
  for (let attempt = 0; attempt < maxRetries; attempt++) {
    try {
      return await fn()
    } catch (err) {
      lastErr = err
      // Don't retry on client errors (4xx) except 429
      if (err instanceof ApiError && err.status >= 400 && err.status < 500 && err.status !== 429) {
        throw err
      }
      if (attempt === maxRetries - 1) break
      const delay = INITIAL_RETRY_DELAY * 2 ** attempt * (1 + Math.random() * 0.3)
      await new Promise((r) => setTimeout(r, delay))
    }
  }
  throw lastErr
}

// ---------------------------------------------------------------------------
// Core request executor
// ---------------------------------------------------------------------------

export async function request<T>(
  method: string,
  path: string,
  options: RequestOptions = {},
): Promise<T> {
  const { params, skipAuth = false, timeout = DEFAULT_TIMEOUT, retry = true, body } = options

  if (!skipAuth) {
    await ensureFreshToken()
  }

  const url = buildUrl(`${API_BASE}${path}`, params)

  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
  }
  if (!skipAuth) {
    const token = tokenStorage.get()
    if (token) headers['Authorization'] = `Bearer ${token}`
  }

  const ac = new AbortController()
  const timerId = setTimeout(() => ac.abort(), timeout)

  const doFetch = async (): Promise<T> => {
    let res: Response
    try {
      res = await fetch(url, {
        method,
        headers,
        signal: ac.signal,
        ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
      })
    } catch (err) {
      if (err instanceof DOMException && err.name === 'AbortError') {
        throw new TimeoutError()
      }
      throw new NetworkError(err instanceof Error ? err.message : 'Network error')
    }

    // On 401, attempt one token refresh then retry once
    if (res.status === 401 && !skipAuth) {
      try {
        await refreshAuthToken()
        const newToken = tokenStorage.get()
        if (newToken) headers['Authorization'] = `Bearer ${newToken}`
        res = await fetch(url, {
          method,
          headers,
          signal: ac.signal,
          ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
        })
      } catch {
        throw new ApiError(401, 'Session expired. Please log in again.')
      }
    }

    return handleResponse<T>(res)
  }

  try {
    return retry ? await withRetry(doFetch) : await doFetch()
  } finally {
    clearTimeout(timerId)
  }
}

// ---------------------------------------------------------------------------
// Convenience methods
// ---------------------------------------------------------------------------

export const get = <T>(path: string, options?: Omit<RequestOptions, 'body'>): Promise<T> =>
  request<T>('GET', path, options)

export const post = <T>(path: string, body?: unknown, options?: Omit<RequestOptions, 'body'>): Promise<T> =>
  request<T>('POST', path, { ...options, body })

export const put = <T>(path: string, body?: unknown, options?: Omit<RequestOptions, 'body'>): Promise<T> =>
  request<T>('PUT', path, { ...options, body })

export const patch = <T>(path: string, body?: unknown, options?: Omit<RequestOptions, 'body'>): Promise<T> =>
  request<T>('PATCH', path, { ...options, body })

export const del = <T>(path: string, options?: Omit<RequestOptions, 'body'>): Promise<T> =>
  request<T>('DELETE', path, options)

// ---------------------------------------------------------------------------
// Typed API surface
// ---------------------------------------------------------------------------

export const api = {
  auth: {
    login: (username: string, password: string): Promise<AuthResponse> =>
      post<AuthResponse>('/auth/login', { username, password }, { skipAuth: true }),
    logout: (): Promise<void> =>
      post<void>('/auth/logout'),
    me: (): Promise<{ id: string; username: string; email: string; roles: string[] }> =>
      get('/auth/me'),
    refresh: (): Promise<AuthResponse> =>
      post<AuthResponse>('/auth/refresh', undefined, { skipAuth: true }),
  },

  nodes: {
    list: (): Promise<NodesResponse> => get<NodesResponse>('/nodes'),
    get: (id: string): Promise<{ node: unknown }> => get(`/nodes/${id}`),
    create: (data: Record<string, unknown>): Promise<{ node: unknown }> => post('/nodes', data),
    update: (id: string, data: Record<string, unknown>): Promise<{ node: unknown }> =>
      patch(`/nodes/${id}`, data),
    delete: (id: string): Promise<void> => del(`/nodes/${id}`),
    connect: (id: string): Promise<void> => post(`/nodes/${id}/connect`),
    disconnect: (id: string): Promise<void> => post(`/nodes/${id}/disconnect`),
    remoteDesktop: (id: string): Promise<{ url: string }> => get(`/nodes/${id}/remote`),
  },

  routing: {
    getCurrent: (): Promise<{ activeNodeId: string | null }> => get('/routing/current'),
    switch: (nodeId: string): Promise<void> => post('/routing/switch', { nodeId }),
    getHistory: (): Promise<{ events: Array<{ nodeId: string; timestamp: string; type: string }> }> =>
      get('/routing/history'),
  },

  audio: {
    getSources: (): Promise<{ sources: Array<{ id: string; name: string; active: boolean }> }> =>
      get('/audio/sources'),
    setSource: (sourceId: string): Promise<void> => post('/audio/source', { sourceId }),
  },

  devices: {
    list: (): Promise<{ devices: Array<{ id: string; name: string; type: string }> }> =>
      get('/devices'),
  },
}

// ---------------------------------------------------------------------------
// WebSocket client
// ---------------------------------------------------------------------------

/** Maximum number of automatic reconnection attempts (0 = unlimited). */
const WS_MAX_RECONNECT_ATTEMPTS = 10
const WS_BASE_RECONNECT_MS = 1_000
const WS_MAX_RECONNECT_MS = 30_000

export class WebSocketClient<T = unknown> {
  private socket: WebSocket | null = null
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null
  private _connected = false
  /** Set to true by `disconnect()` to suppress automatic reconnection. */
  private _intentionallyClosed = false
  private _reconnectAttempts = 0
  private readonly messageHandlers: Array<(data: T) => void> = []
  private readonly errorHandlers: Array<(err: Event) => void> = []
  private readonly closeHandlers: Array<(ev: CloseEvent) => void> = []
  private readonly openHandlers: Array<() => void> = []

  constructor(private readonly url: string) {}

  connect(): void {
    if (
      this.socket?.readyState === WebSocket.CONNECTING ||
      this.socket?.readyState === WebSocket.OPEN
    ) return

    this._intentionallyClosed = false
    const wsUrl = this.url.startsWith('ws') ? this.url : `ws://${location.host}${this.url}`
    this.socket = new WebSocket(wsUrl)

    this.socket.onopen = () => {
      this._connected = true
      this._reconnectAttempts = 0
      const token = tokenStorage.get()
      if (token) this.socket?.send(JSON.stringify({ type: 'auth', token }))
      this.openHandlers.forEach((h) => h())
    }

    this.socket.onmessage = (ev) => {
      try {
        const data = JSON.parse(ev.data as string) as T
        this.messageHandlers.forEach((h) => h(data))
      } catch {
        // malformed message — ignore
      }
    }

    this.socket.onerror = (ev) => {
      this.errorHandlers.forEach((h) => h(ev))
    }

    this.socket.onclose = (ev) => {
      this._connected = false
      this.closeHandlers.forEach((h) => h(ev))
      if (!this._intentionallyClosed) {
        this._scheduleReconnect()
      }
    }
  }

  disconnect(): void {
    this._intentionallyClosed = true
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer)
      this.reconnectTimer = null
    }
    this.socket?.close()
    this.socket = null
    this._connected = false
    this._reconnectAttempts = 0
  }

  send(data: unknown): boolean {
    if (this.socket?.readyState !== WebSocket.OPEN) return false
    try {
      this.socket.send(JSON.stringify(data))
      return true
    } catch {
      return false
    }
  }

  onOpen(cb: () => void): void { this.openHandlers.push(cb) }
  onMessage(cb: (data: T) => void): void { this.messageHandlers.push(cb) }
  onError(cb: (err: Event) => void): void { this.errorHandlers.push(cb) }
  onClose(cb: (ev: CloseEvent) => void): void { this.closeHandlers.push(cb) }
  isConnected(): boolean { return this._connected && this.socket?.readyState === WebSocket.OPEN }
  reconnectAttempts(): number { return this._reconnectAttempts }

  private _scheduleReconnect(): void {
    if (WS_MAX_RECONNECT_ATTEMPTS > 0 && this._reconnectAttempts >= WS_MAX_RECONNECT_ATTEMPTS) {
      console.warn(`[WebSocketClient] Max reconnect attempts (${WS_MAX_RECONNECT_ATTEMPTS}) reached for ${this.url}`)
      return
    }
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer)
    // Exponential backoff with jitter, capped at WS_MAX_RECONNECT_MS
    const delay = Math.min(
      WS_BASE_RECONNECT_MS * 2 ** this._reconnectAttempts * (1 + Math.random() * 0.3),
      WS_MAX_RECONNECT_MS,
    )
    this._reconnectAttempts++
    this.reconnectTimer = setTimeout(() => {
      if (!this._intentionallyClosed && !this._connected) this.connect()
    }, delay)
  }
}

export function createWebSocketClient<T = unknown>(url: string): WebSocketClient<T> {
  return new WebSocketClient<T>(url)
}
