import { createContext, useContext, useEffect, useState, ReactNode } from 'react'

const ThemeContext = createContext<{ isDark: boolean; toggleTheme: () => void } | undefined>(undefined)

export function ThemeProvider({ children }: { children: ReactNode }) {
  const [isDark, setIsDark] = useState(true)

  useEffect(() => {
    // Check for stored theme or default to dark
    const storedTheme = localStorage.getItem('ozma_theme')
    const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches

    if (storedTheme) {
      setIsDark(storedTheme === 'dark')
    } else {
      setIsDark(prefersDark)
    }
  }, [])

  useEffect(() => {
    if (isDark) {
      document.documentElement.classList.add('dark')
      document.documentElement.style.setProperty('--background', '0 0% 3.9%')
      document.documentElement.style.setProperty('--foreground', '0 0% 98%')
      document.documentElement.style.setProperty('--card', '0 0% 7%')
      document.documentElement.style.setProperty('--card-foreground', '0 0% 98%')
      document.documentElement.style.setProperty('--popover', '0 0% 7%')
      document.documentElement.style.setProperty('--popover-foreground', '0 0% 98%')
      document.documentElement.style.setProperty('--primary', '146 51% 63%')
      document.documentElement.style.setProperty('--primary-foreground', '0 0% 0%')
      document.documentElement.style.setProperty('--secondary', '0 0% 14.9%')
      document.documentElement.style.setProperty('--secondary-foreground', '0 0% 98%')
      document.documentElement.style.setProperty('--muted', '0 0% 14.9%')
      document.documentElement.style.setProperty('--muted-foreground', '0 0% 63.9%')
      document.documentElement.style.setProperty('--accent', '0 0% 14.9%')
      document.documentElement.style.setProperty('--accent-foreground', '0 0% 98%')
      document.documentElement.style.setProperty('--destructive', '0 84.2% 60.2%')
      document.documentElement.style.setProperty('--destructive-foreground', '0 0% 98%')
      document.documentElement.style.setProperty('--border', '0 0% 14.9%')
      document.documentElement.style.setProperty('--input', '0 0% 14.9%')
      document.documentElement.style.setProperty('--ring', '146 51% 63%')
    } else {
      document.documentElement.classList.remove('dark')
    }
    localStorage.setItem('ozma_theme', isDark ? 'dark' : 'light')
  }, [isDark])

  const toggleTheme = () => setIsDark(!isDark)

  return <ThemeContext.Provider value={{ isDark, toggleTheme }}>{children}</ThemeContext.Provider>
}

export function useTheme() {
  const context = useContext(ThemeContext)
  if (context === undefined) {
    throw new Error('useTheme must be used within a ThemeProvider')
  }
  return context
}
