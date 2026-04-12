import { create } from 'zustand'
import { api, setToken, removeToken, isAuthenticated as checkAuth } from '../api/client'

export interface User {
  id: string
  username: string
  email: string
  roles: string[]
}

interface AuthState {
  user: User | null
  isAuthenticated: boolean
  isLoading: boolean
  error: string | null
  login: (username: string, password: string) => Promise<void>
  logout: () => Promise<void>
  checkSession: () => void
}

export const useAuthStore = create<AuthState>((set) => ({
  user: null,
  isAuthenticated: checkAuth(),
  isLoading: false,
  error: null,

  login: async (username, password) => {
    set({ isLoading: true, error: null })
    try {
      const response = await api.auth.login(username, password)
      setToken(response.token)
      const user: User = {
        id: response.user.id,
        username: response.user.username,
        email: response.user.email,
        roles: response.user.roles,
      }
      set({ user, isAuthenticated: true, isLoading: false })
    } catch (err) {
      const message = err instanceof Error ? err.message : 'Login failed'
      set({ isLoading: false, error: message })
      throw err
    }
  },

  logout: async () => {
    try {
      await api.auth.logout()
    } catch {
      // Ignore logout errors — always clear local state
    } finally {
      removeToken()
      set({ user: null, isAuthenticated: false })
    }
  },

  checkSession: () => {
    set({ isAuthenticated: checkAuth() })
  },
}))

// Convenience alias used throughout the app
export const useAuth = useAuthStore
