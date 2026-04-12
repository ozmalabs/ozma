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
      
      // Store token in localStorage directly (using the token storage abstraction)
      localStorage.setItem('ozma_token', response.token)
      
      // Parse and extract user info from token
      try {
        const parts = response.token.split('.')
        if (parts.length === 3) {
          const payload = JSON.parse(atob(parts[1]))
          const user: User = {
            id: payload.sub || payload.id?.toString() || payload.sub || 'unknown',
            username: payload.username || 'unknown',
            email: payload.email || '',
            roles: Array.isArray(payload.roles) ? payload.roles : [],
            avatar: payload.avatar,
          }
          set({ 
            user, 
            token: response.token, 
            isAuthenticated: true,
            isLoading: false 
          })
          return
        }
      } catch {
        // If token parsing fails, store the token anyway
      }
      
      // Fallback if token parsing fails
      set({ 
        token: response.token, 
        isAuthenticated: true,
        isLoading: false 
      })
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
      
      // Store token in localStorage
      localStorage.setItem('ozma_token', response.token)
      
      // Parse and extract user info from token
      try {
        const parts = response.token.split('.')
        if (parts.length === 3) {
          const payload = JSON.parse(atob(parts[1]))
          const user: User = {
            id: payload.sub || payload.id?.toString() || 'unknown',
            username: payload.username || 'unknown',
            email: payload.email || '',
            roles: Array.isArray(payload.roles) ? payload.roles : [],
            avatar: payload.avatar,
          }
          set({ 
            user,
            token: response.token, 
            isAuthenticated: true,
            isLoading: false 
          })
          return
        }
      } catch {
        // If token parsing fails, store the token anyway
      }
      
      // Fallback if token parsing fails
      set({ 
        token: response.token, 
        isAuthenticated: true,
        isLoading: false 
      })
    } catch (error) {
      // On refresh failure, clear token
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
