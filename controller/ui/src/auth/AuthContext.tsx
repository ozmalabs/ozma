import React, {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
} from 'react';
import { Navigate, useLocation } from 'react-router-dom';
import {
  clearToken,
  getToken,
  setToken,
  startExpiryWatcher,
  stopExpiryWatcher,
} from './tokenStorage';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

export interface UserInfo {
  id: string;
  username: string;
  display_name: string;
  role: string;
  auth_enabled: boolean;
}

interface AuthContextValue {
  user: UserInfo | null;
  isAuthenticated: boolean;
  login: (password: string, username?: string) => Promise<void>;
  logout: () => void;
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

const AuthContext = createContext<AuthContextValue | null>(null);

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [user, setUser] = useState<UserInfo | null>(null);

  // Derive isAuthenticated from whether we have a user object AND a token.
  const isAuthenticated = user !== null && getToken() !== null;

  const logout = useCallback(() => {
    clearToken();
    stopExpiryWatcher();
    setUser(null);
  }, []);

  const login = useCallback(
    async (password: string, username = '') => {
      const res = await fetch('/api/v1/auth/token', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password, username }),
      });

      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        throw new Error(body?.detail ?? body?.error ?? 'Login failed');
      }

      const data = await res.json();
      const token: string = data.token;
      const expiresIn: number | undefined = data.expires_in;

      setToken(token, expiresIn);
      startExpiryWatcher(logout);

      // Fetch the current user profile so we can populate the context.
      const meRes = await fetch('/api/v1/users/me', {
        headers: { Authorization: `Bearer ${token}` },
      });

      if (meRes.ok) {
        const me: UserInfo = await meRes.json();
        setUser(me);
      } else {
        // Auth is on but /me failed — still mark as logged in with minimal info.
        setUser({
          id: '',
          username: username || 'admin',
          display_name: 'Admin',
          role: 'owner',
          auth_enabled: true,
        });
      }
    },
    [logout],
  );

  // On mount: if auth is disabled the API returns a synthetic user from /me.
  // Try a silent probe so the UI doesn't flash the login page unnecessarily.
  useEffect(() => {
    (async () => {
      try {
        const res = await fetch('/api/v1/users/me');
        if (res.ok) {
          const me: UserInfo = await res.json();
          if (!me.auth_enabled) {
            // Auth disabled — treat as permanently logged in.
            setUser(me);
          }
        }
      } catch {
        // Network error — leave user as null; login page will handle it.
      }
    })();
  }, []);

  const value = useMemo<AuthContextValue>(
    () => ({ user, isAuthenticated, login, logout }),
    [user, isAuthenticated, login, logout],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth must be used inside <AuthProvider>');
  return ctx;
}

// ---------------------------------------------------------------------------
// Guard component
// ---------------------------------------------------------------------------

export function RequireAuth({ children }: { children: React.ReactNode }) {
  const { isAuthenticated } = useAuth();
  const location = useLocation();

  if (!isAuthenticated) {
    return <Navigate to="/login" state={{ from: location }} replace />;
  }

  return <>{children}</>;
}
