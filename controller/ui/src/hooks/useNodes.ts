import { create } from 'zustand';
import { NodeInfo } from '../types/node';
import { fetchNodes } from '../utils/api';

interface WebSocketMessage {
  type: 'nodes_updated' | 'node_status_changed';
  nodes?: NodeInfo[];
  nodeId?: string;
  status?: NodeInfo['status'];
}

interface NodesStore {
  nodes: NodeInfo[];
  loading: boolean;
  error: string | null;
  selectedNodeId: string | null;
  webSocketStatus: 'connected' | 'disconnected' | 'error';
  webSocket: WebSocket | null;

  // Actions
  fetchNodes: () => Promise<void>;
  selectNode: (id: string) => void;
  updateNodeStatus: (id: string, status: NodeInfo['status']) => void;
  connectWebSocket: () => void;
  disconnectWebSocket: () => void;
  clearError: () => void;
}

const API_BASE_URL = '/api/v1';

export const useNodesStore = create<NodesStore>((set, get) => ({
  nodes: [],
  loading: true,
  error: null,
  selectedNodeId: null,
  webSocketStatus: 'disconnected',
  webSocket: null,

  fetchNodes: async () => {
    try {
      set((state) => ({ ...state, loading: true, error: null }));
      const nodes = await fetchNodes();
      set({ nodes, loading: false, error: null });
    } catch (error) {
      set((state) => ({
        ...state,
        loading: false,
        error: error instanceof Error ? error.message : 'Failed to fetch nodes',
      }));
    }
  },

  selectNode: (id: string) => {
    set({ selectedNodeId: id });
  },

  updateNodeStatus: (id: string, status: NodeInfo['status']) => {
    set((state) => ({
      nodes: state.nodes.map((node) =>
        node.id === id ? { ...node, status: status as NodeInfo['status'] } : node
      ),
    }));
  },

  connectWebSocket: () => {
    const { webSocket } = get();
    if (webSocket && webSocket.readyState === WebSocket.OPEN) {
      return;
    }

    try {
      const ws = new WebSocket(`ws://localhost:7380${API_BASE_URL}/ws/nodes`);

      ws.onopen = () => {
        console.log('WebSocket connected');
        set({ webSocketStatus: 'connected', webSocket: ws });
      };

      ws.onmessage = (event) => {
        try {
          const message: WebSocketMessage = JSON.parse(event.data);

          switch (message.type) {
            case 'nodes_updated':
              if (message.nodes) {
                set({ nodes: message.nodes });
              }
              break;
            case 'node_status_changed':
              if (message.nodeId && message.status) {
                set((state) => ({
                  nodes: state.nodes.map((node) =>
                    node.id === message.nodeId ? { ...node, status: message.status } : node
                  ),
                }));
              }
              break;
          }
        } catch (error) {
          console.error('Failed to parse WebSocket message:', error);
        }
      };

      ws.onerror = (error) => {
        console.error('WebSocket error:', error);
        set({ webSocketStatus: 'error' });
      };

      ws.onclose = () => {
        console.log('WebSocket disconnected');
        set({ webSocketStatus: 'disconnected', webSocket: null });
      };

      set({ webSocket: ws, webSocketStatus: 'connected' });
    } catch (error) {
      console.error('Failed to connect WebSocket:', error);
      set({ webSocketStatus: 'error', error: 'WebSocket connection failed' });
    }
  },

  disconnectWebSocket: () => {
    const { webSocket } = get();
    if (webSocket) {
      webSocket.close();
      set({ webSocket: null, webSocketStatus: 'disconnected' });
    }
  },

  clearError: () => {
    set({ error: null });
  },
}));

// Hook for using nodes store
export const useNodes = () => {
  const { fetchNodes, nodes, loading, error, selectedNodeId, selectNode, webSocketStatus } =
    useNodesStore();

  return {
    nodes,
    loading,
    error,
    selectedNodeId,
    webSocketStatus,
    actions: { fetchNodes, selectNode },
  };
};
