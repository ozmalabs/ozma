import React from 'react'
import { create } from 'zustand'
import type { AuthState } from '../types'

const useAuthStore = create<AuthState>((set) => ({
  token: null,
  setToken: (token) => set({ token }),
  isLoggedIn: false,
  login: (token) => {
    localStorage.setItem('ozma_auth_token', token)
    set({ token, isLoggedIn: true })
  },
  logout: () => {
    localStorage.removeItem('ozma_auth_token')
    set({ token: null, isLoggedIn: false })
  },
}))

export const useAuth = () => {
  const { token, setToken, isLoggedIn, login, logout } = useAuthStore()
  
  React.useEffect(() => {
    const storedToken = localStorage.getItem('ozma_auth_token')
    if (storedToken) {
      setToken(storedToken)
    }
  }, [setToken])
  
  return { token, isLoggedIn, login, logout }
}
