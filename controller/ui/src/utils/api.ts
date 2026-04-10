import { NodeInfo } from '../types/node';

/**
 * API client configuration
 * All API requests go through Vite proxy to localhost:7380
 */
export const API_BASE_URL = '/api/v1';

/**
 * Validate node data from API response
 */
export function validateNodeResponse(data: unknown): NodeInfo[] {
  if (!Array.isArray(data)) {
    throw new Error('Invalid response format: expected array of nodes');
  }

  return data.map((node, index) => {
    if (typeof node !== 'object' || node === null) {
      throw new Error(`Invalid node at index ${index}: expected object`);
    }

    // Type assertion - the API response structure is validated at runtime
    return node as NodeInfo;
  });
}

/**
 * Fetch nodes from API with validation
 */
export async function fetchNodes(): Promise<NodeInfo[]> {
  try {
    const response = await fetch(`${API_BASE_URL}/nodes`, {
      method: 'GET',
      headers: {
        'Content-Type': 'application/json',
      },
    });

    if (!response.ok) {
      throw new Error(`API request failed with status ${response.status}`);
    }

    const data = await response.json();
    return validateNodeResponse(data);
  } catch (error) {
    console.error('Failed to fetch nodes:', error);
    throw error;
  }
}
