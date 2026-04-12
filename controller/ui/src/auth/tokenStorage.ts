/**
 * In-memory JWT token store.
 *
 * Deliberately NOT backed by localStorage / sessionStorage so the token
 * is never persisted to disk and is wiped on page reload (forces re-login,
 * which is the correct behaviour for a controller dashboard).
 */

let _token: string | null = null;
let _expiry: number | null = null; // Unix timestamp (seconds)
let _watcherTimer: ReturnType<typeof setTimeout> | null = null;
let _onExpire: (() => void) | null = null;

export function getToken(): string | null {
  return _token;
}

export function getExpiry(): number | null {
  return _expiry;
}

export function setToken(token: string, expiresIn?: number): void {
  _token = token;
  _expiry = expiresIn != null ? Math.floor(Date.now() / 1000) + expiresIn : null;
}

export function clearToken(): void {
  _token = null;
  _expiry = null;
  stopExpiryWatcher();
}

/**
 * Start a timer that calls `onExpire` ~30 s before the token expires
 * (or immediately if already expired / expiry unknown).
 */
export function startExpiryWatcher(onExpire: () => void): void {
  stopExpiryWatcher();
  _onExpire = onExpire;

  if (_expiry == null) return;

  const nowSec = Math.floor(Date.now() / 1000);
  const msUntilWarn = Math.max(0, (_expiry - nowSec - 30) * 1000);

  _watcherTimer = setTimeout(() => {
    _watcherTimer = null;
    if (_onExpire) _onExpire();
  }, msUntilWarn);
}

export function stopExpiryWatcher(): void {
  if (_watcherTimer != null) {
    clearTimeout(_watcherTimer);
    _watcherTimer = null;
  }
  _onExpire = null;
}
