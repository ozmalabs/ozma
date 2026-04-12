import { createClient, dedupExchange, cacheExchange, fetchExchange, Client } from 'urql'

// GraphQL endpoint URL
const GRAPHQL_URL = '/graphql'

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

// Function to check if token is available
export function isTokenAvailable(): boolean {
  return !!localStorage.getItem('ozma_token')
}

// Function to update auth headers
export function updateAuthHeaders(): void {
  const token = localStorage.getItem('ozma_token')
  if (token) {
    client.setContext({
      headers: {
        Authorization: `Bearer ${token}`,
      },
    })
  }
}
