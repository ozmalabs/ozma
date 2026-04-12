/**
 * JWT utilities — parsing and validation only.
 * No side-effects; no storage access.
 */

export interface TokenPayload {
  sub: string
  exp: number
  iat?: number
  aud?: string | string[]
  iss?: string
  username?: string
  email?: string
  roles?: string[]
  scopes?: string[]
  name?: string
  avatar?: string
}

/** Milliseconds of buffer before expiry at which we consider the token "expiring". */
export const TOKEN_REFRESH_BUFFER_MS = 30_000

/**
 * Parse a JWT and return its payload, or null on any failure.
 *
 * Security notes:
 *  - Uses a JSON.parse reviver to drop __proto__ / constructor / prototype keys,
 *    preventing prototype-pollution.
 *  - Validates required numeric claims (exp) and array-of-string claims (roles, scopes).
 *  - Returns null rather than throwing so callers don't need try/catch.
 */
export function parseToken(token: string): TokenPayload | null {
  if (!token || typeof token !== 'string') return null

  const parts = token.split('.')
  if (parts.length !== 3) return null

  let payload: Record<string, unknown>
  try {
    const json = atob(parts[1])
    payload = JSON.parse(json, (key, value) => {
      // Drop prototype-polluting keys at any nesting level
      if (key === '__proto__' || key === 'constructor' || key === 'prototype') {
        return undefined
      }
      return value
    }) as Record<string, unknown>
  } catch {
    return null
  }

  if (!payload || typeof payload !== 'object' || Array.isArray(payload)) return null

  // --- Required claims ---
  if (typeof payload.sub !== 'string' || payload.sub.length === 0) return null
  if (typeof payload.exp !== 'number') return null

  // --- Optional but typed claims ---
  if (payload.iat !== undefined && typeof payload.iat !== 'number') return null

  if (payload.roles !== undefined) {
    if (!Array.isArray(payload.roles) || !payload.roles.every((r) => typeof r === 'string')) {
      return null
    }
  }

  if (payload.scopes !== undefined) {
    if (!Array.isArray(payload.scopes) || !payload.scopes.every((s) => typeof s === 'string')) {
      return null
    }
  }

  const stringOptionals: (keyof TokenPayload)[] = ['username', 'email', 'name', 'avatar', 'iss']
  for (const key of stringOptionals) {
    if (payload[key] !== undefined && typeof payload[key] !== 'string') return null
  }

  return payload as unknown as TokenPayload
}

/** True if the token is structurally valid and not yet expired. */
export function isTokenValid(token: string): boolean {
  const p = parseToken(token)
  if (!p) return false
  return Date.now() < p.exp * 1000
}

/** True if the token is missing, unparseable, or its expiry is in the past. */
export function isTokenExpired(token: string | null | undefined): boolean {
  if (!token) return true
  const p = parseToken(token)
  if (!p) return true
  return Date.now() >= p.exp * 1000
}

/** True if the token expires within TOKEN_REFRESH_BUFFER_MS. */
export function isTokenExpiringSoon(token: string): boolean {
  const p = parseToken(token)
  if (!p) return true
  return Date.now() >= p.exp * 1000 - TOKEN_REFRESH_BUFFER_MS
}

/**
 * Constant-time string comparison.
 * Iterates ALL characters even when lengths differ to avoid timing leakage.
 */
export function constantTimeEquals(a: string, b: string): boolean {
  const maxLen = Math.max(a.length, b.length)
  let result = a.length === b.length ? 0 : 1
  for (let i = 0; i < maxLen; i++) {
    // Use 0 for out-of-bounds so we always do the XOR
    result |= (a.charCodeAt(i) || 0) ^ (b.charCodeAt(i) || 0)
  }
  return result === 0
}
