/**
 * OIDC authentication context.
 *
 * Uses react-native-app-auth to perform the OIDC Authorization Code + PKCE
 * flow via the system browser. Tokens are stored encrypted in MMKV.
 *
 * The controller proxies an Authentik OIDC provider; the discovery document
 * is at <controllerUrl>/.well-known/openid-configuration
 */

import React, {
  createContext,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from 'react';
import {
  authorize,
  AuthConfiguration,
  AuthorizeResult,
  refresh,
  revoke,
} from 'react-native-app-auth';
import {MMKV} from 'react-native-mmkv';
import {OIDCTokens} from '../api/types';
import {ozmaClient} from '../api/client';

// ── Storage ───────────────────────────────────────────────────────────────────

const storage = new MMKV({id: 'ozma-auth'});
const TOKENS_KEY = 'ozma.auth.tokens';
const CONTROLLER_URL_KEY = 'ozma.controller_url';

function loadStoredTokens(): OIDCTokens | null {
  const raw = storage.getString(TOKENS_KEY);
  if (!raw) {
    return null;
  }
  try {
    return JSON.parse(raw) as OIDCTokens;
  } catch {
    return null;
  }
}

function persistTokens(tokens: OIDCTokens): void {
  storage.set(TOKENS_KEY, JSON.stringify(tokens));
  // Keep the API client's storage layer in sync.
  ozmaClient.setTokens(tokens);
}

function clearPersistedTokens(): void {
  storage.delete(TOKENS_KEY);
  ozmaClient.clearTokens();
}

function buildAuthConfig(controllerUrl: string): AuthConfiguration {
  return {
    issuer: controllerUrl,
    clientId: 'ozma-mobile',
    redirectUrl: 'com.ozmalabs.ozma://oauth/callback',
    scopes: ['openid', 'profile', 'email', 'offline_access'],
    usePKCE: true,
    // Authentik expiry — override if needed
    additionalParameters: {},
    serviceConfiguration: undefined, // resolved from issuer/.well-known
  };
}

function tokensFromResult(result: AuthorizeResult): OIDCTokens {
  return {
    access_token: result.accessToken,
    refresh_token: result.refreshToken ?? null,
    id_token: result.idToken ?? null,
    expires_at: Math.floor(new Date(result.accessTokenExpirationDate).getTime() / 1000),
    scopes: result.scopes ?? [],
  };
}

// ── Context type ──────────────────────────────────────────────────────────────

export interface AuthState {
  tokens: OIDCTokens | null;
  /** Derived: true when access_token is present and not expired */
  isAuthenticated: boolean;
  isLoading: boolean;
  error: string | null;
}

export interface AuthContextValue extends AuthState {
  controllerUrl: string | null;
  setControllerUrl(url: string): void;
  login(): Promise<void>;
  logout(): Promise<void>;
  refreshIfNeeded(): Promise<OIDCTokens | null>;
}

// ── Context ───────────────────────────────────────────────────────────────────

export const AuthContext = createContext<AuthContextValue>({
  tokens: null,
  isAuthenticated: false,
  isLoading: true,
  error: null,
  controllerUrl: null,
  setControllerUrl: () => undefined,
  login: async () => undefined,
  logout: async () => undefined,
  refreshIfNeeded: async () => null,
});

// ── Provider ──────────────────────────────────────────────────────────────────

export function AuthProvider({children}: {children: React.ReactNode}) {
  const [tokens, setTokens] = useState<OIDCTokens | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [controllerUrl, setControllerUrlState] = useState<string | null>(
    storage.getString(CONTROLLER_URL_KEY) ?? null,
  );

  // Refresh lock — shared Promise so parallel callers wait on the same refresh.
  const refreshLock = useRef<Promise<OIDCTokens | null> | null>(null);

  // On mount: restore persisted tokens.
  useEffect(() => {
    const stored = loadStoredTokens();
    if (stored) {
      // Sync with API client on restore.
      ozmaClient.setTokens(stored);
      setTokens(stored);
    }
    setIsLoading(false);
  }, []);

  const setControllerUrl = useCallback((url: string) => {
    const clean = url.replace(/\/$/, '');
    storage.set(CONTROLLER_URL_KEY, clean);
    ozmaClient.setControllerUrl(clean);
    setControllerUrlState(clean);
  }, []);

  const login = useCallback(async () => {
    if (!controllerUrl) {
      setError('Controller URL not set. Go to Settings first.');
      return;
    }
    setError(null);
    setIsLoading(true);
    try {
      const config = buildAuthConfig(controllerUrl);
      const result = await authorize(config);
      const newTokens = tokensFromResult(result);
      persistTokens(newTokens);
      setTokens(newTokens);
    } catch (err: unknown) {
      const message =
        err instanceof Error ? err.message : 'Login failed. Please try again.';
      // User cancelling the browser flow throws a specific message; don't show it.
      if (!message.includes('cancel') && !message.includes('dismiss')) {
        setError(message);
      }
    } finally {
      setIsLoading(false);
    }
  }, [controllerUrl]);

  const logout = useCallback(async () => {
    if (!controllerUrl || !tokens) {
      clearPersistedTokens();
      setTokens(null);
      return;
    }
    setIsLoading(true);
    try {
      const config = buildAuthConfig(controllerUrl);
      if (tokens.access_token) {
        try {
          await revoke(config, {
            tokenToRevoke: tokens.access_token,
            sendClientId: true,
          });
        } catch {
          // Best-effort; continue regardless.
        }
      }
    } finally {
      clearPersistedTokens();
      setTokens(null);
      setIsLoading(false);
    }
  }, [controllerUrl, tokens]);

  const refreshIfNeeded = useCallback(async (): Promise<OIDCTokens | null> => {
    const current = tokens ?? loadStoredTokens();
    if (!current) {
      return null;
    }

    const now = Math.floor(Date.now() / 1000);
    // Refresh if expiry is within 60 seconds.
    if (current.expires_at - now >= 60) {
      return current;
    }

    if (!current.refresh_token || !controllerUrl) {
      return null;
    }

    if (!refreshLock.current) {
      refreshLock.current = (async () => {
        try {
          const config = buildAuthConfig(controllerUrl);
          const result = await refresh(config, {
            refreshToken: current.refresh_token as string,
          });
          const refreshed: OIDCTokens = {
            access_token: result.accessToken,
            refresh_token: result.refreshToken ?? current.refresh_token,
            id_token: result.idToken ?? current.id_token,
            expires_at: Math.floor(
              new Date(result.accessTokenExpirationDate).getTime() / 1000,
            ),
            scopes: result.scopes ?? current.scopes,
          };
          persistTokens(refreshed);
          setTokens(refreshed);
          return refreshed;
        } catch {
          // Refresh failed — force re-login.
          clearPersistedTokens();
          setTokens(null);
          return null;
        } finally {
          refreshLock.current = null;
        }
      })();
    }

    return refreshLock.current;
  }, [tokens, controllerUrl]);

  const isAuthenticated = useMemo(() => {
    if (!tokens?.access_token) {
      return false;
    }
    const now = Math.floor(Date.now() / 1000);
    return tokens.expires_at > now;
  }, [tokens]);

  const value = useMemo<AuthContextValue>(
    () => ({
      tokens,
      isAuthenticated,
      isLoading,
      error,
      controllerUrl,
      setControllerUrl,
      login,
      logout,
      refreshIfNeeded,
    }),
    [
      tokens,
      isAuthenticated,
      isLoading,
      error,
      controllerUrl,
      setControllerUrl,
      login,
      logout,
      refreshIfNeeded,
    ],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}
