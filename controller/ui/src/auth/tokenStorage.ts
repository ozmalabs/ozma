/**
 * Secure, SSR-safe token storage backed by localStorage with an in-memory
 * fallback for non-browser environments (SSR, Node test runners, etc.).
 *
 * Rules:
 *  - Never throws; returns null on any failure.
 *  - Prefers localStorage so multiple tabs stay in sync.
 *  - Falls back to an in-memory store when localStorage is unavailable.
 *  - The localStorage availability check is memoized after the first call.
 *  - Expiry is parsed once per set() and cached to avoid repeated JWT splits.
 */

const KEY = 'ozma_token'

/** Memoized result of the localStorage availability probe. */
let _lsAvailable: boolean | null = null

/** In-memory fallback used when localStorage is not available. */
let _memoryToken: string | null = null

let _cachedExpiry: number | null = null

/**
 * Optional callback invoked by `startExpiryWatcher` when the stored token
 * is found to have expired. Typically wired to the auth store's logout action.
 */
let _onExpiredCallback: (() => void) | null = null
let _watcherTimer: ReturnType<typeof setInterval> | null = null

function parseExpiry(token: string): number | null {
  try {
    const parts = token.split('.')
    if (parts.length !== 3) return null
    const payload = JSON.parse(atob(parts[1])) as Record<string, unknown>
    const exp = payload?.exp
    return typeof exp === 'number' ? exp * 1000 : null
  } catch {
    return null
  }
}

/** Returns true if localStorage is readable and writable. Result is memoized. */
function localStorageAvailable(): boolean {
  if (_lsAvailable !== null) return _lsAvailable
  try {
    const test = '__ozma_ls_test__'
    localStorage.setItem(test, '1')
    localStorage.removeItem(test)
    _lsAvailable = true
  } catch {
    _lsAvailable = false
  }
  return _lsAvailable
}

export const tokenStorage = {
  get(): string | null {
    if (localStorageAvailable()) {
      try {
        return localStorage.getItem(KEY)
      } catch {
        return _memoryToken
      }
    }
    return _memoryToken
  },

  set(token: string): void {
    _cachedExpiry = parseExpiry(token)
    if (localStorageAvailable()) {
      try {
        localStorage.setItem(KEY, token)
        return
      } catch {
        // quota exceeded or private-browsing restriction — fall through to memory
      }
    }
    _memoryToken = token
  },

  remove(): void {
    _cachedExpiry = null
    _memoryToken = null
    if (localStorageAvailable()) {
      try {
        localStorage.removeItem(KEY)
      } catch {
        // ignore
      }
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

  /**
   * Returns true if there is no stored token, or if the stored token's expiry
   * is in the past. Never throws.
   */
  isExpired(): boolean {
    const expiry = this.getExpiry()
    if (expiry === null) return true
    return Date.now() >= expiry
  },

  /**
   * Start a periodic watcher that calls `onExpired` when the stored token
   * expires mid-session. Safe to call multiple times — only one watcher runs
   * at a time. Call `stopExpiryWatcher()` on unmount / logout.
   *
   * @param onExpired  Called once when expiry is detected.
   * @param intervalMs Poll interval (default 30 s).
   */
  startExpiryWatcher(onExpired: () => void, intervalMs = 30_000): void {
    _onExpiredCallback = onExpired
    if (_watcherTimer !== null) return // already running
    _watcherTimer = setInterval(() => {
      if (tokenStorage.isExpired()) {
        tokenStorage.stopExpiryWatcher()
        try {
          _onExpiredCallback?.()
        } catch {
          // ignore
        }
      }
    }, intervalMs)
  },

  /** Stop the expiry watcher started by `startExpiryWatcher`. */
  stopExpiryWatcher(): void {
    if (_watcherTimer !== null) {
      clearInterval(_watcherTimer)
      _watcherTimer = null
    }
    _onExpiredCallback = null
  },
}

// ---------------------------------------------------------------------------
// Named exports for watcher functions (so callers can import them directly)
// ---------------------------------------------------------------------------

/** @see tokenStorage.startExpiryWatcher */
export function startExpiryWatcher(onExpired: () => void, intervalMs = 30_000): void {
  tokenStorage.startExpiryWatcher(onExpired, intervalMs)
}

/** @see tokenStorage.stopExpiryWatcher */
export function stopExpiryWatcher(): void {
  tokenStorage.stopExpiryWatcher()
}
