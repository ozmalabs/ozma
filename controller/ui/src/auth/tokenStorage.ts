/**
 * Secure, SSR-safe token storage backed by localStorage.
 *
 * Rules:
 *  - Never throws; returns null on any failure.
 *  - All reads go through localStorage so multiple tabs stay in sync.
 *  - Expiry is parsed once per set() and cached to avoid repeated JWT splits.
 */

const KEY = 'ozma_token'

let _cachedExpiry: number | null = null

function parseExpiry(token: string): number | null {
  try {
    const parts = token.split('.')
    if (parts.length !== 3) return null
    const payload = JSON.parse(atob(parts[1]))
    const exp = payload?.exp
    return typeof exp === 'number' ? exp * 1000 : null
  } catch {
    return null
  }
}

function localStorageAvailable(): boolean {
  return typeof localStorage !== 'undefined'
}

export const tokenStorage = {
  get(): string | null {
    if (!localStorageAvailable()) return null
    try {
      return localStorage.getItem(KEY)
    } catch {
      return null
    }
  },

  set(token: string): void {
    _cachedExpiry = parseExpiry(token)
    if (!localStorageAvailable()) return
    try {
      localStorage.setItem(KEY, token)
    } catch {
      // quota exceeded or private-browsing restriction — ignore
    }
  },

  remove(): void {
    _cachedExpiry = null
    if (!localStorageAvailable()) return
    try {
      localStorage.removeItem(KEY)
    } catch {
      // ignore
    }
  },

  /** Returns the cached expiry (ms since epoch) for the currently stored token. */
  getExpiry(): number | null {
    const token = this.get()
    if (!token) return null
    // Re-parse if cache is cold (e.g. another tab set the token)
    if (_cachedExpiry === null) {
      _cachedExpiry = parseExpiry(token)
    }
    return _cachedExpiry
  },
}
