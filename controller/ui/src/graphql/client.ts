import { createClient, dedupExchange, cacheExchange, fetchExchange, subscriptionExchange, Client } from 'urql'
import { createClient as createWSClient } from 'graphql-ws'

// GraphQL endpoint URLs
const GRAPHQL_URL = '/graphql'
const WS_URL = 'ws://localhost:7380/graphql'

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
}) : null

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
    // WebSocket subscription exchange
    wsClient ? subscriptionExchange({
      forwardSubscription: (operation) => {
        return wsClient.request(operation)
      },
    }) : fetchExchange,
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
  return wsClient ? wsClient.connected : false
}

// Function to get WebSocket connection status
export function getWebSocketStatus(): string {
  if (!wsClient) return 'disconnected'
  return wsClient.connected ? 'connected' : 'disconnected'
}
