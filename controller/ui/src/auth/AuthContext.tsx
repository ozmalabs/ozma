import React, { createContext, useCallback, useContext, useEffect, useState } from 'react'
import { tokenStorage } from './tokenStorage'
import { parseToken, isTokenValid, isTokenExpired } from './tokenUtils'
import { api, setToken, removeToken } from '../api/client'

interface User {
  id: string
  username: string
  email: string
  roles: string[]
}

interface AuthState {
  user: User | null
  loading: boolean
}

interface AuthContextValue extends AuthState {
  login: (username: string, password: string) => Promise<void>
  logout: () => Promise<void>
  isAuthenticated: boolean
}

const AuthContext = createContext<AuthContextValue | null>(null)

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [state, setState] = useState<AuthState>({ user: null, loading: true })

  // On mount, restore session from stored token
  useEffect(() => {
    const token = tokenStorage.get()
    if (token && isTokenValid(token)) {
      const payload = parseToken(token)
      if (payload) {
        api.auth
          .me()
          .then((me) => setState({ user: me, loading: false }))
          .catch(() => {
            removeToken()
            setState({ user: null, loading: false })
          })
        return
      }
    }
    removeToken()
    setState({ user: null, loading: false })
  }, [])

  // Periodically check whether the stored token has expired mid-session and
  // log the user out automatically if so. Checks every 30 s.
  useEffect(() => {
    const CHECK_INTERVAL_MS = 30_000
    const id = setInterval(() => {
      const token = tokenStorage.get()
      if (state.user !== null && isTokenExpired(token)) {
        removeToken()
        setState({ user: null, loading: false })
      }
    }, CHECK_INTERVAL_MS)
    return () => clearInterval(id)
  }, [state.user])

  const login = useCallback(async (username: string, password: string) => {
    const res = await api.auth.login(username, password)
    setToken(res.token)
    setState({ user: res.user, loading: false })
  }, [])

  const logout = useCallback(async () => {
    try {
      await api.auth.logout()
    } finally {
      removeToken()
      setState({ user: null, loading: false })
    }
  }, [])

  return (
    <AuthContext.Provider
      value={{
        ...state,
        isAuthenticated: state.user !== null,
        login,
        logout,
      }}
    >
      {children}
    </AuthContext.Provider>
  )
}

export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext)
  if (!ctx) throw new Error('useAuth must be used inside <AuthProvider>')
  return ctx
}
