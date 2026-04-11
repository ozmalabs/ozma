import { createClient, dedupExchange, cacheExchange, fetchExchange, Client, subscriptionExchange, Exchange } from 'urql'
import { createClient as createWSClient } from 'graphql-ws'

// GraphQL endpoint URLs - use the same base as the REST API
const GRAPHQL_URL = '/graphql'
const WS_URL = typeof window !== 'undefined' && window.location.protocol === 'https:'
  ? `wss://${window.location.host}/graphql`
  : `ws://localhost:7380/graphql`

// Track WebSocket connection state
let wsConnected = false

// Create the WebSocket client for subscriptions
const wsClient = typeof window !== 'undefined' ? createWSClient({
  url: WS_URL,
  lazy: true,
  connectionInit: () => {
    const token = localStorage.getItem('ozma_token')
    if (token) {
      return { authorization: `Bearer ${token}` }
    }
    return {}
  },
  on: {
    connected: () => {
      wsConnected = true
      console.log('WebSocket connected')
    },
    disconnected: () => {
      wsConnected = false
      console.log('WebSocket disconnected')
    },
    error: (error) => {
      wsConnected = false
      console.error('WebSocket error:', error)
    },
  },
}) : null

// Custom subscription exchange that properly handles reconnection
const createReconnectingSubscriptionExchange = (): Exchange => {
  let operationListeners = new Map<string, (data: any) => void>()

  return ({ forward }) => {
    const obs$ = (operations$) => {
      return (observer) => {
        const unsubscribe = forward(operations$).subscribe({
          next: (result) => {
            observer.next(result)
          },
          error: (error) => {
            observer.error(error)
          },
        })

        return () => {
          unsubscribe()
        }
      }
    }

    return obs$
  }
}

// Create the urql client
export const client: Client = createClient({
  url: GRAPHQL_URL,
  fetchOptions: {
    headers: {
      Authorization: `Bearer ${localStorage.getItem('ozma_token') || ''}`,
    },
  },
  exchanges: [
    dedupExchange,
    cacheExchange,
    fetchExchange,
    // WebSocket subscription exchange with reconnection
    wsClient 
      ? subscriptionExchange({
          forwardSubscription: (operation) => {
            // Ensure WebSocket is connected
            if (!wsClient.connected) {
              wsClient.connect()
            }
            return wsClient.request(operation)
          },
        })
      : fetchExchange,
  ],
})

// Function to get authentication headers
export function getAuthHeaders(): Record<string, string> {
  const headers: Record<string, string> = {}
  const token = localStorage.getItem('ozma_token')
  if (token) {
    headers.Authorization = `Bearer ${token}`
  }
  return headers
}

// Function to check if WebSocket is connected
export function isWebSocketConnected(): boolean {
  return wsClient ? wsConnected : false
}

// Function to get WebSocket connection status
export function getWebSocketStatus(): string {
  if (!wsClient) return 'disconnected'
  return wsConnected ? 'connected' : 'connecting'
}

// Function to connect WebSocket (if not already connected)
export function connectWebSocket(): Promise<boolean> {
  if (!wsClient) return Promise.resolve(false)
  if (wsClient.connected) return Promise.resolve(true)
  
  return new Promise((resolve) => {
    const timeout = setTimeout(() => {
      resolve(false)
    }, 5000)
    
    const checkConnection = () => {
      if (wsClient.connected) {
        clearTimeout(timeout)
        resolve(true)
      } else {
        setTimeout(checkConnection, 100)
      }
    }
    
    wsClient.connect()
    checkConnection()
  })
}

// Function to disconnect WebSocket
export function disconnectWebSocket(): void {
  if (wsClient) {
    wsClient.dispose()
    wsConnected = false
  }
}
