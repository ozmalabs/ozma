import { create } from 'zustand'
import { api } from '../api/client'

export interface User {
  id: string
  username: string
  email: string
  roles: string[]
  avatar?: string
}

export interface AuthState {
  user: User | null
  token: string | null
  isLoading: boolean
  error: string | null
  isAuthenticated: boolean
  login: (username: string, password: string) => Promise<void>
  logout: () => Promise<void>
  refresh: () => Promise<void>
  setUser: (user: User | null) => void
  setError: (error: string | null) => void
}

export const useAuthStore = create<AuthState>((set, get) => ({
  user: null,
  token: null,
  isLoading: false,
  error: null,
  isAuthenticated: false,

  login: async (username, password) => {
    set({ isLoading: true, error: null })
    try {
      const response = await api.auth.login(username, password)
      localStorage.setItem('ozma_token', response.token)
      set({ token: response.token, isLoading: false })
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Login failed'
      set({ isLoading: false, error: errorMessage })
      throw new Error(errorMessage)
    }
  },

  logout: async () => {
    set({ isLoading: true })
    try {
      await api.auth.logout()
    } catch {
      // Ignore logout errors
    } finally {
      localStorage.removeItem('ozma_token')
      set({ user: null, token: null, isAuthenticated: false, isLoading: false, error: null })
      window.location.href = '/login'
    }
  },

  refresh: async () => {
    const token = get().token
    if (!token) return

    set({ isLoading: true, error: null })
    try {
      const response = await api.auth.refresh()
      localStorage.setItem('ozma_token', response.token)
      set({ token: response.token, isLoading: false })
    } catch (error) {
      localStorage.removeItem('ozma_token')
      set({ user: null, token: null, isAuthenticated: false, isLoading: false, error: null })
    }
  },

  setUser: (user) => {
    set({ user, isAuthenticated: !!user })
  },

  setError: (error) => {
    set({ error })
  },
}))

// Hook for checking auth status
export function useAuth() {
  const { user, token, isLoading, error, isAuthenticated, login, logout, refresh, setUser, setError } =
    useAuthStore()

  return {
    user,
    token,
    isLoading,
    error,
    isAuthenticated,
    login,
    logout,
    refresh,
    setUser,
    setError,
  }
}
