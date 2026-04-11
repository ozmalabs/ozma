/**
 * GraphQL client setup using urql.
 * Connects to the controller's GraphQL endpoint at /graphql
 */

import {createClient, fetchExchange, subscriptionExchange} from '@urql/core';
import {MMKV} from 'react-native-mmkv';

const STORAGE_KEY_TOKENS = 'ozma.auth.tokens';
const STORAGE_KEY_CONTROLLER_URL = 'ozma.controller_url';

const storage = new MMKV({id: 'ozma-api'});

// ── Helpers ───────────────────────────────────────────────────────────────────

function getControllerUrl(): string {
  const url = storage.getString(STORAGE_KEY_CONTROLLER_URL);
  if (!url) {
    throw new Error('Controller URL not configured. Open Settings to set it.');
  }
  // Strip trailing slash
  return url.replace(/\/$/, '');
}

function getAccessToken(): string | null {
  const raw = storage.getString(STORAGE_KEY_TOKENS);
  if (!raw) {
    return null;
  }
  try {
    const tokens = JSON.parse(raw) as {access_token?: string};
    return tokens.access_token ?? null;
  } catch {
    return null;
  }
}

// ── Fetch client with auth ────────────────────────────────────────────────────

const getFetchOptions = () => {
  const accessToken = getAccessToken();
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    Accept: 'application/json',
  };
  if (accessToken) {
    headers['Authorization'] = `Bearer ${accessToken}`;
  }
  return {headers};
};

// ── urql client configuration ─────────────────────────────────────────────────

export const graphqlClient = createClient({
  url: `${getControllerUrl()}/graphql`,
  fetchOptions: getFetchOptions,
  fetch: async (url, options) => {
    const response = await fetch(url, options);
    return {
      ...response,
      body: response.body
        ? await response.text().then((text) => ({
            text: () => Promise.resolve(text),
            json: () => Promise.resolve(JSON.parse(text)),
          }))
        : undefined,
    };
  },
  exchanges: [
    fetchExchange,
    // For subscriptions, we would need the subscriptions-core exchange
    // but for now we'll use WebSocket directly for subscriptions
  ],
});

// ── WebSocket connection for subscriptions ────────────────────────────────────

let wsConnection: WebSocket | null = null;
let wsUrl: string | null = null;
let reconnectTimeout: NodeJS.Timeout | null = null;
let listeners: ((data: any) => void)[] = [];

export function getWsUrl(): string {
  const controllerUrl = getControllerUrl();
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
  return `${protocol}//${controllerUrl.replace(/^https?:\/\//, '')}/api/v1/ws`;
}

export function connectWebSocket(onMessage: (data: any) => void) {
  const wsUrl = getWsUrl();

  if (wsConnection) {
    wsConnection.close();
  }

  wsConnection = new WebSocket(wsUrl);

  wsConnection.onopen = () => {
    console.log('GraphQL WebSocket connected');
  };

  wsConnection.onclose = () => {
    console.log('GraphQL WebSocket disconnected, reconnecting in 3s...');
    reconnectTimeout = setTimeout(() => connectWebSocket(onMessage), 3000);
  };

  wsConnection.onerror = (err) => {
    console.log('GraphQL WebSocket error:', err);
  };

  wsConnection.onmessage = (event) => {
    try {
      const data = JSON.parse(event.data);
      // Forward to all listeners
      listeners.forEach((listener) => listener(data));
    } catch (e) {
      console.log('WebSocket message parse error:', e);
    }
  };

  return wsConnection;
}

export function addWebSocketListener(listener: (data: any) => void) {
  listeners.push(listener);
  return () => {
    listeners = listeners.filter((l) => l !== listener);
  };
}

export function closeWebSocket() {
  if (wsConnection) {
    wsConnection.close();
    wsConnection = null;
  }
  if (reconnectTimeout) {
    clearTimeout(reconnectTimeout);
    reconnectTimeout = null;
  }
}

export function isConnectedToWebSocket(): boolean {
  return wsConnection?.readyState === WebSocket.OPEN || false;
}
