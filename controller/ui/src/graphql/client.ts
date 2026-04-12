// GraphQL endpoint — proxied by Vite dev server to the controller
const GRAPHQL_URL = '/graphql'

function makeFetchOptions(): RequestInit {
  const token = localStorage.getItem('ozma_token')
  return {
    headers: {
      'Content-Type': 'application/json',
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
  }
}

/**
 * Execute a raw GraphQL request against the controller's /graphql endpoint.
 * Uses plain fetch to avoid urql v2/v3 API churn.
 */
export async function graphqlRequest<T = unknown>(
  query: string,
  variables?: Record<string, unknown>,
): Promise<T> {
  const response = await fetch(GRAPHQL_URL, {
    method: 'POST',
    ...makeFetchOptions(),
    body: JSON.stringify({ query, variables }),
  })

  if (!response.ok) {
    throw new Error(`GraphQL HTTP error ${response.status}`)
  }

  const json = (await response.json()) as { data?: T; errors?: { message: string }[] }

  if (json.errors && json.errors.length > 0) {
    throw new Error(json.errors.map((e) => e.message).join('; '))
  }

  return json.data as T
}

/** Returns auth headers for use in custom fetch calls. */
export function getAuthHeaders(): Record<string, string> {
  const token = localStorage.getItem('ozma_token')
  return token ? { Authorization: `Bearer ${token}` } : {}
}

/** Returns true if an auth token is present in storage. */
export function isTokenAvailable(): boolean {
  return !!localStorage.getItem('ozma_token')
}
