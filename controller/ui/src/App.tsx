import { useEffect } from 'react'
import { Router } from './router'
import { useNodesStore } from './hooks/useNodes'

function App() {
  const connectWebSocket = useNodesStore((state) => state.connectWebSocket)

  useEffect(() => {
    connectWebSocket()
  }, [connectWebSocket])

  return <Router />
}

export default App
