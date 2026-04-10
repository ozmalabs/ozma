import { useEffect, useState } from 'react'

export default function Topbar() {
  const [currentTime, setCurrentTime] = useState(new Date())

  useEffect(() => {
    const timer = setInterval(() => {
      setCurrentTime(new Date())
    }, 1000)
    return () => clearInterval(timer)
  }, [])

  return (
    <header className="h-16 border-b flex items-center justify-between px-6 bg-background">
      <div className="flex items-center gap-4">
        <h2 className="text-lg font-semibold">Dashboard</h2>
      </div>

      <div className="flex items-center gap-6">
        <div className="text-right">
          <div className="text-sm font-medium">{currentTime.toLocaleTimeString()}</div>
          <div className="text-xs text-muted-foreground">{currentTime.toLocaleDateString()}</div>
        </div>
        <div className="w-10 h-10 rounded-full bg-emerald-500 flex items-center justify-center text-white font-semibold">
          OC
        </div>
      </div>
    </header>
  )
}
