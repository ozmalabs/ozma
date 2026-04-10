import { Node } from '../types'

const API_BASE = '/api/v1'

let authToken: string | null = null

export const setAuthToken = (token: string) => {
  authToken = token
}

export const clearAuthToken = () => {
  authToken = null
}

interface RequestOptions extends RequestInit {
  skipAuth?: boolean
}

async function fetchWithAuth(url: string, options: RequestOptions = {}): Promise<Response> {
  const headers = new Headers(options.headers)
  
  if (!options.skipAuth && authToken) {
    headers.set('Authorization', `Bearer ${authToken}`)
  }
  headers.set('Content-Type', 'application/json')

  return fetch(`${API_BASE}${url}`, {
    ...options,
    headers,
  })
}

export async function getNodes(): Promise<Node[]> {
  const response = await fetchWithAuth('/nodes')
  if (!response.ok) {
    throw new Error(`Failed to fetch nodes: ${response.status}`)
  }
  return response.json()
}

export async function getNode(id: string): Promise<Node> {
  const response = await fetchWithAuth(`/nodes/${id}`)
  if (!response.ok) {
    throw new Error(`Failed to fetch node: ${response.status}`)
  }
  return response.json()
}

export async function setNodeActive(id: string): Promise<void> {
  const response = await fetchWithAuth(`/nodes/${id}/active`, {
    method: 'PUT',
  })
  if (!response.ok) {
    throw new Error(`Failed to set node active: ${response.status}`)
  }
}
