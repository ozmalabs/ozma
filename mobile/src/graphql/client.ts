/**
 * GraphQL client setup using urql with WebSocket subscription support.
 * Connects to the controller's GraphQL endpoint at /graphql and WebSocket at /api/v1/ws
 *
 * Logging levels:
 *   - debug: detailed internal operations
 *   - info: connection lifecycle events
 *   - warn: recoverable issues
 *   - error: failures requiring attention
 */

import {createClient, fetchExchange, subscriptionExchange, RequestPolicy} from '@urql/core';
import {MMKV} from 'react-native-mmkv';

const LOG_PREFIX = '[GraphQLClient]';

// ── Storage Keys ──────────────────────────────────────────────────────────────

const STORAGE_KEY_TOKENS = 'ozma.auth.tokens';
const STORAGE_KEY_CONTROLLER_URL = 'ozma.controller_url';

const storage = new MMKV({id: 'ozma-api'});

// ── Logging Utilities ────────────────────────────────────────────────────────

function log(level: 'debug' | 'info' | 'warn' | 'error', ...args: any[]) {
  const prefix = `${LOG_PREFIX} [${level.toUpperCase()}]`;
  // eslint-disable-next-line no-console
  console[level](prefix, ...args);
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/**
 * Get the controller URL from storage.
 * @throws Error if controller URL is not configured.
 */
export function getControllerUrl(): string {
  const url = storage.getString(STORAGE_KEY_CONTROLLER_URL);
  if (!url) {
    throw new Error('Controller URL not configured. Open Settings to set it.');
  }
  // Strip trailing slash for consistency
  return url.replace(/\/$/, '');
}

/**
 * Get the access token from storage.
 * Returns null if no token is available or if parsing fails.
 */
export function getAccessToken(): string | null {
  const raw = storage.getString(STORAGE_KEY_TOKENS);
  if (!raw) {
    return null;
  }
  try {
    const tokens = JSON.parse(raw) as {access_token?: string};
    return tokens.access_token ?? null;
  } catch (e) {
    log('debug', 'Failed to parse auth tokens:', e);
    return null;
  }
}

/**
 * Build authorization headers with the current access token.
 */
function getAuthHeaders(): Record<string, string> {
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    Accept: 'application/json',
  };
  const accessToken = getAccessToken();
  if (accessToken) {
    headers['Authorization'] = `Bearer ${accessToken}`;
    log('debug', 'Authorization header added');
  }
  return headers;
}

// ── GraphQL Client Configuration ──────────────────────────────────────────────

const GRAPHQL_URL = `${getControllerUrl()}/graphql`;
const WEBSOCKET_URL = 'ws://localhost:7380/api/v1/ws';

/**
 * Custom fetch implementation that handles response body conversion.
 * This ensures compatibility with urql's expectations.
 */
async function customFetch(url: string, options: RequestInit): Promise<Response> {
  log('debug', `Fetching: ${url}`);
  
  try {
    const response = await fetch(url, options);
    
    // Clone response body for urql compatibility
    const clonedResponse = response.clone();
    
    // Ensure body is properly handled for text/json parsing
    if (response.body) {
      const text = await response.text();
      log('debug', `Response status: ${response.status}, body length: ${text.length}`);
      
      // Return original response with properly typed body
      return {
        ...response,
        body: {
          text: () => Promise.resolve(text),
          json: () => Promise.resolve(JSON.parse(text)),
        } as unknown as ResponseInit['body'],
      } as Response;
    }
    
    return clonedResponse;
  } catch (error) {
    log('error', 'Fetch error:', error);
    throw error;
  }
}

/**
 * Create the urql client with proper exchanges for REST and WebSocket operations.
 */
export const graphqlClient = createClient({
  url: GRAPHQL_URL,
  
  // Fetch function with auth headers
  fetch: customFetch,
  
  // Fetch options with auth headers
  fetchOptions: () => ({
    headers: getAuthHeaders(),
  }),
  
  // Exchange pipeline
  exchanges: [
    // fetchExchange handles standard HTTP requests
    fetchExchange,
    // subscriptionExchange handles subscriptions via WebSocket
    subscriptionExchange({
      forwardSubscription: (operation) => {
        const wsUrl = getWebSocketUrl();
        log('info', `Connecting to WebSocket: ${wsUrl}`);
        
        return getWebSocketClient(wsUrl).execute({
          query: operation.query,
          variables: operation.variables,
        });
      },
    }),
  ],
  
  // Request policy fallback
  requestPolicy: 'cache-and-network' as RequestPolicy,
});

// ── WebSocket Client ──────────────────────────────────────────────────────────

let wsClient: any = null;
let wsReconnectTimeout: NodeJS.Timeout | null = null;
let wsListeners: ((data: any) => void)[] = [];
let wsConnected = false;
let wsConnecting = false;

/**
 * Build the WebSocket URL from the controller URL.
 * Uses 'ws' protocol for HTTP and 'wss' for HTTPS.
 */
export function getWebSocketUrl(): string {
  const controllerUrl = getControllerUrl();
  
  // Determine protocol based on controller URL
  const protocol = controllerUrl.startsWith('https') ? 'wss:' : 'ws:';
  
  // Construct WebSocket URL - using the specified endpoint from task
  return `${protocol}//${controllerUrl.replace(/^https?:\/\//, '')}/api/v1/ws`;
}

/**
 * Get or create a WebSocket client for subscriptions.
 * Handles connection state and reconnection logic.
 */
function getWebSocketClient(url: string): any {
  if (wsClient) {
    log('debug', 'Returning existing WebSocket client');
    return wsClient;
  }

  wsConnecting = true;

  wsClient = {
    subscribe: (operation: any, options: any) => {
      log('info', `WebSocket subscribe: ${operation.operationName || 'unnamed'}`);

      const ws = new WebSocket(url);

      ws.onopen = () => {
        log('info', 'WebSocket connected');
        wsConnected = true;
        wsConnecting = false;

        // Send subscription payload
        const payload = {
          type: 'subscribe',
          id: operation.key || Date.now().toString(),
          payload: {
            type: 'subscription_start',
            query: operation.query,
            variables: operation.variables,
          },
        };

        try {
          ws.send(JSON.stringify(payload));
          log('debug', 'Subscription payload sent');
        } catch (error) {
          log('error', 'Failed to send subscription:', error);
        }
      };

      ws.onclose = (event) => {
        log('warn', `WebSocket closed: ${event.code} ${event.reason || ''}`);
        wsConnected = false;
        wsConnecting = false;

        // Auto-reconnect on abnormal closure
        if (event.code !== 1000) {
          log('info', 'Reconnecting WebSocket in 3 seconds...');
          wsReconnectTimeout = setTimeout(() => {
            log('info', 'Reconnecting WebSocket...');
            getWebSocketClient(url);
          }, 3000);
        }
      };

      ws.onerror = (error) => {
        log('error', 'WebSocket error:', error);
        wsConnected = false;
      };

      ws.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data.toString());
          log('debug', 'WebSocket message received:', data.type, data);

          // Handle different message types
          if (data.type === 'subscription_data' || data.type === 'data') {
            const payload = data.payload || data;

            // Notify all listeners
            wsListeners.forEach((listener) => {
              try {
                listener(payload);
              } catch (err) {
                log('error', 'Listener error:', err);
              }
            });

            // Handle subscription data with operation matching
            if (payload.data && operation.key) {
              log('debug', `Subscription data received for ${operation.key}`);
            }
          }

          if (data.type === 'subscription_ack') {
            log('info', 'Subscription acknowledged:', data.id);
          }

          if (data.type === 'subscription_error' || data.type === 'error') {
            log('error', 'Subscription error:', data.payload || data);
          }
        } catch (e) {
          log('error', 'WebSocket message parse error:', e);
        }
      };

      return {
        unsubscribe: () => {
          log('debug', 'WebSocket unsubscribe');
          if (ws.readyState === WebSocket.OPEN) {
            const unsubscribePayload = {
              type: 'unsubscribe',
              id: operation.key || 'unknown',
            };
            try {
              ws.send(JSON.stringify(unsubscribePayload));
            } catch (err) {
              log('debug', 'Failed to send unsubscribe:', err);
            }
          }
          ws.close(1000, 'Unsubscribed');
        },
        observable: {
          subscribe: (observer: any) => {
            const wrappedListener = (data: any) => {
              if (data.payload?.data) {
                observer.next(data.payload.data);
              }
            };
            wsListeners.push(wrappedListener);

            return {
              unsubscribe: () => {
                wsListeners = wsListeners.filter((l) => l !== wrappedListener);
              },
            };
          },
        },
      };
    },
  };

  return wsClient;
}

/**
 * Connect to WebSocket explicitly.
 * This can be called to establish the connection before subscriptions.
 */
export function connectWebSocket(onMessage?: (data: any) => void): WebSocket | null {
  const wsUrl = getWebSocketUrl();
  log('info', `Connecting to WebSocket: ${wsUrl}`);

  // Close existing connection
  if (wsClient) {
    wsClient = null;
  }

  const ws = new WebSocket(wsUrl);
  let subscriptionId: string | null = null;

  ws.onopen = () => {
    log('info', 'WebSocket connected');
    wsConnected = true;
    wsConnecting = false;

    if (onMessage) {
      const subscriptionPayload = {
        type: 'subscribe',
        id: 'main_subscription',
        payload: {
          type: 'subscription_start',
          query: 'subscription OnAny { anyEvent }',
          variables: {},
        },
      };

      try {
        ws.send(JSON.stringify(subscriptionPayload));
        subscriptionId = 'main_subscription';
        log('debug', 'Main subscription sent');
      } catch (error) {
        log('error', 'Failed to send subscription:', error);
      }
    }
  };

  ws.onclose = (event) => {
    log('info', `WebSocket disconnected: ${event.code} ${event.reason || ''}`);
    wsConnected = false;

    // Auto-reconnect for abnormal closures
    if (event.code !== 1000) {
      log('info', 'Reconnecting WebSocket in 3 seconds...');
      wsReconnectTimeout = setTimeout(() => {
        connectWebSocket(onMessage);
      }, 3000);
    }
  };

  ws.onerror = (error) => {
    log('error', 'WebSocket connection error:', error);
    wsConnected = false;
  };

  ws.onmessage = (event) => {
    try {
      const data = JSON.parse(event.data.toString());
      log('debug', 'WebSocket message:', data.type, data);

      // Handle subscription data
      if (data.type === 'subscription_data' || data.type === 'data') {
        if (onMessage) {
          onMessage(data.payload || data);
        }

        // Notify registered listeners
        wsListeners.forEach((listener) => {
          try {
            listener(data.payload || data);
          } catch (err) {
            log('error', 'Listener error:', err);
          }
        });
      }

      if (data.type === 'subscription_ack') {
        log('info', 'Subscription acknowledged:', data.id);
      }

      if (data.type === 'error' || data.type === 'subscription_error') {
        log('error', 'WebSocket error:', data.payload || data);
      }
    } catch (e) {
      log('error', 'WebSocket message parse error:', e);
    }
  };

  return ws;
}

/**
 * Add a listener to receive WebSocket messages.
 * @returns Unsubscribe function
 */
export function addWebSocketListener(listener: (data: any) => void): () => void {
  wsListeners.push(listener);
  log('debug', `WebSocket listener added. Total: ${wsListeners.length}`);

  return () => {
    wsListeners = wsListeners.filter((l) => l !== listener);
    log('debug', `WebSocket listener removed. Total: ${wsListeners.length}`);
  };
}

/**
 * Remove all WebSocket listeners.
 */
export function clearWebSocketListeners(): void {
  wsListeners = [];
  log('debug', 'All WebSocket listeners cleared');
}

/**
 * Close the WebSocket connection.
 */
export function closeWebSocket(): void {
  log('info', 'Closing WebSocket connection');

  if (wsClient) {
    wsClient = null;
  }

  if (wsReconnectTimeout) {
    clearTimeout(wsReconnectTimeout);
    wsReconnectTimeout = null;
  }

  wsConnected = false;
  wsConnecting = false;
  log('info', 'WebSocket connection closed');
}

/**
 * Check if WebSocket is currently connected.
 */
export function isConnectedToWebSocket(): boolean {
  return wsConnected;
}

/**
 * Get WebSocket connection status.
 * @returns Object with connection state information
 */
export function getWebSocketStatus(): {
  connected: boolean;
  connecting: boolean;
  url: string;
} {
  return {
    connected: wsConnected,
    connecting: wsConnecting,
    url: getWebSocketUrl(),
  };
}
