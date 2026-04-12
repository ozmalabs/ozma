/**
 * Authentication state store (Zustand).
 *
 * Consumers should import `useAuthStore` and read:
 *   - `isAuthenticated` — true when a valid, non-expired token is present
 *   - `isLoading`       — true while the initial session check / login is in flight
 *   - `user`            — decoded token payload, or null when unauthenticated
 *   - `error`           — last login error message, or null
 *
 * The store listens for the `ozma:session-expired` window event (fired by the
 * API client's proactive token-expiry handler) and clears state automatically.
 */

import { create } from 'zustand'
import { tokenStorage } from '../auth/tokenStorage'
import { parseToken, isTokenValid, TokenPayload } from '../auth/tokenUtils'
import { api } from '../api/client'

export interface AuthUser {
  id: string
  username: string
  email: string
  roles: string[]
}

interface AuthState {
  isAuthenticated: boolean
  isLoading: boolean
  user: AuthUser | null
  error: string | null
  /** Log in with username + password. Throws on failure. */
  login: (username: string, password: string) => Promise<void>
  /** Log out and clear all auth state. */
  logout: () => Promise<void>
  /** Re-hydrate state from the stored token (call once on app mount). */
  initialize: () => Promise<void>
}

function userFromPayload(payload: TokenPayload): AuthUser {
  return {
    id: payload.sub,
    username: payload.username ?? payload.sub,
    email: payload.email ?? '',
    roles: payload.roles ?? [],
  }
}

export const useAuthStore = create<AuthState>((set) => {
  // Listen for proactive session-expiry events fired by the API client
  if (typeof window !== 'undefined') {
    window.addEventListener('ozma:session-expired', () => {
      set({ isAuthenticated: false, user: null, isLoading: false, error: null })
    })
  }

  return {
    isAuthenticated: false,
    isLoading: true,
    user: null,
    error: null,

    async initialize() {
      const token = tokenStorage.get()
      if (token && isTokenValid(token)) {
        const payload = parseToken(token)
        if (payload) {
          set({ isAuthenticated: true, user: userFromPayload(payload), isLoading: false })
          return
        }
      }
      tokenStorage.remove()
      set({ isAuthenticated: false, user: null, isLoading: false })
    },

    async login(username, password) {
      set({ isLoading: true, error: null })
      try {
        const response = await api.auth.login(username, password)
        tokenStorage.set(response.token)
        const payload = parseToken(response.token)
        set({
          isAuthenticated: true,
          user: payload ? userFromPayload(payload) : null,
          isLoading: false,
          error: null,
        })
      } catch (err) {
        const message = err instanceof Error ? err.message : 'Login failed'
        set({ isLoading: false, error: message })
        throw err
      }
    },

    async logout() {
      try {
        await api.auth.logout()
      } catch {
        // Best-effort — clear local state regardless
      } finally {
        tokenStorage.remove()
        set({ isAuthenticated: false, user: null, isLoading: false, error: null })
      }
    },
  }
})

// Convenience alias used throughout the app
export const useAuth = useAuthStore
