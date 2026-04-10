import { create } from 'zustand'

export interface AuthState {
  token: string | null
  setToken: (token: string | null) => void
  isAuthenticated: boolean
  login: (token: string) => void
  logout: () => void
}

export const useAuthStore = create<AuthState>((set) => ({
  token: null,
  isAuthenticated: false,
  setToken: (token) =>
    set({
      token,
      isAuthenticated: token !== null,
    }),
  login: (token) => {
    localStorage.setItem('ozma_auth_token', token)
    set({ token, isAuthenticated: true })
  },
  logout: () => {
    localStorage.removeItem('ozma_auth_token')
    set({ token: null, isAuthenticated: false })
  },
}))

// Helper to get auth header
export const getAuthHeader = () => {
  const token = localStorage.getItem('ozma_auth_token')
  return token ? `Bearer ${token}` : null
}
