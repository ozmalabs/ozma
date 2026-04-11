import { createClient, dedupExchange, cacheExchange, fetchExchange, subscriptionExchange, Client, Operation } from 'urql'
import { subscriptionTransportWs, SubscriptionClient } from '@urql/exchange-subscriptions'

// GraphQL endpoint URL
const GRAPHQL_URL = '/graphql'
const WS_URL = 'ws://localhost:7380/api/v1/ws'

// Create the subscription client for WebSocket connections
const subscriptionClient = new SubscriptionClient(WS_URL, {
  reconnect: true,
  lazy: true,
  connectionInit: () => {
    // Send authentication token if available
    const token = localStorage.getItem('ozma_token')
    if (token) {
      return { authorization: `Bearer ${token}` }
    }
    return {}
  },
})

// Create the subscription exchange
const subscriptionExchange = subscriptionExchange({
  forwardSubscription: (operation: Operation) => {
    return subscriptionClient.request(operation)
  },
})

// Create the urql client
export const client: Client = createClient({
  url: GRAPHQL_URL,
  fetchOptions: {
    headers: {
      // Add authentication token if available
      Authorization: `Bearer ${localStorage.getItem('ozma_token') || ''}`,
    },
  },
  exchanges: [
    dedupExchange,
    cacheExchange,
    fetchExchange,
    subscriptionExchange,
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
  return subscriptionClient.connected
}

// Function to get WebSocket connection status
export function getWebSocketStatus(): string {
  const { CONNECTED, CONNECTING, DISCONNECTED } = subscriptionClient
  switch (subscriptionClient.readyState) {
    case CONNECTED:
      return 'connected'
    case CONNECTING:
      return 'connecting'
    case DISCONNECTED:
    default:
      return 'disconnected'
  }
}
