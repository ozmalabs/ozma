/**
 * GraphQL client singleton (graphql-request).
 *
 * - Injects `Authorization: Bearer <token>` on every request.
 * - On a 401 response, clears the in-memory token and redirects to /login
 *   so the user is never silently stuck with stale credentials.
 */

import { GraphQLClient, ClientError } from 'graphql-request';
import { getToken, clearToken } from '../auth/tokenStorage';

// The controller serves the GraphQL endpoint at /graphql.
const GRAPHQL_ENDPOINT = '/graphql';

function buildHeaders(): Record<string, string> {
  const token = getToken();
  return token ? { Authorization: `Bearer ${token}` } : {};
}

function handle401(): void {
  clearToken();
  // Hard redirect — avoids circular imports with AuthContext.
  if (typeof window !== 'undefined') {
    window.location.href = '/login';
  }
}

export const gqlClient = new GraphQLClient(GRAPHQL_ENDPOINT, {
  // graphql-request calls this function before every request, so the
  // header is always fresh even after a token refresh.
  headers: () => buildHeaders(),

  // Intercept errors: if the server returns HTTP 401 (wrapped in a
  // ClientError), log out immediately.
  responseMiddleware(response) {
    if (response instanceof ClientError) {
      const status = response.response?.status;
      if (status === 401) {
        handle401();
      }
    }
  },
});

/**
 * Thin wrapper around `gqlClient.request` that also catches 401s thrown
 * as plain network errors (e.g. from the REST fallback paths).
 */
export async function gqlRequest<T>(
  document: Parameters<typeof gqlClient.request>[0],
  variables?: Parameters<typeof gqlClient.request>[1],
): Promise<T> {
  try {
    return await gqlClient.request<T>(document, variables);
  } catch (err: unknown) {
    if (err instanceof ClientError && err.response?.status === 401) {
      handle401();
    }
    throw err;
  }
}

/**
 * Authenticated REST helper — uses the same token as the GraphQL client.
 * On 401, triggers the same logout flow.
 */
export async function apiFetch(
  path: string,
  init: RequestInit = {},
): Promise<Response> {
  const token = getToken();
  const headers = new Headers(init.headers);
  if (token) headers.set('Authorization', `Bearer ${token}`);

  const res = await fetch(path, { ...init, headers });

  if (res.status === 401) {
    handle401();
  }

  return res;
}
