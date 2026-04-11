/**
 * useNodeDetail - Hook for managing node detail state with WebSocket subscriptions.
 *
 * Provides:
 *   - Node detail data fetching
 *   - WebSocket-based real-time updates
 *   - Node activation
 *   - Automatic reconnection on disconnect
 */

import {useCallback, useEffect, useRef, useState} from 'react';
import {ozmaClient} from '../api/client';
import {useStore} from '../store/useStore';
import {NodeInfo, OzmaApiError, CameraStream} from '../api/types';

export interface NodeDetailsExtended {
  id: string;
  name: string;
  host: string;
  port: number;
  machine_class: string;
  last_seen: string | null;
  online: boolean;
  mac_address: string | null;
  camera_streams: CameraStream[];
  frigate_host: string | null;
  frigate_port: number | null;
  direct_registered: boolean;
  agent_connected: boolean;
  ip_address: string | null;
  platform: string | null;
  os_version: string | null;
  // Additional derived fields
  uptime?: string;
  isActive?: boolean;
  streamPath?: string | null;
  streamPort?: number | null;
}

interface UseNodeDetailReturn {
  node: NodeDetailsExtended | null;
  loading: boolean;
  error: string | null;
  reloading: boolean;
  activating: boolean;
  wsConnected: boolean;
  reload: () => Promise<void>;
  activateNode: () => Promise<void>;
}

/**
 * Fetch and subscribe to node detail updates.
 * @param nodeId - The node ID to track
 * @returns Node data and management functions
 */
export function useNodeDetail(nodeId: string): UseNodeDetailReturn {
  const setNodes = useStore((s) => s.setNodes);
  const setNodesLoading = useStore((s) => s.setNodesLoading);
  const setNodesError = useStore((s) => s.setNodesError);
  const setActiveNodeId = useStore((s) => s.setActiveNodeId);
  const setSelectedNode = useStore((s) => s.nodeStore.setSelectedNode);
  const setSelectedNodeLoading = useStore((s) => s.nodeStore.setSelectedNodeLoading);
  const setSelectedNodeError = useStore((s) => s.nodeStore.setSelectedNodeError);
  const updateSelectedNode = useStore((s) => s.nodeStore.updateSelectedNode);
  const nodes = useStore((s) => s.nodes);
  const activeNodeId = useStore((s) => s.activeNodeId);

  const [node, setNode] = useState<NodeDetailsExtended | null>(null);
  const [loading, setLoading] = useState(true);
  const [reloading, setReloading] = useState(false);
  const [activating, setActivating] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [wsConnected, setWsConnected] = useState(false);

  const wsRef = useRef<WebSocket | null>(null);
  const reconnectTimeoutRef = useRef<NodeJS.Timeout | null>(null);

  const getControllerUrl = useCallback((): string => {
    const storage = require('react-native-mmkv').MMKV;
    const STORAGE_KEY_CONTROLLER_URL = 'ozma.controller_url';
    const storageInstance = new storage({id: 'ozma-api'});
    const url = storageInstance.getString(STORAGE_KEY_CONTROLLER_URL);
    if (!url) {
      throw new Error('Controller URL not configured');
    }
    return url.replace(/\/$/, '');
  }, []);

  const loadNode = useCallback(
    async (id: string, isReload = false) => {
      const setLoadingState = isReload ? setReloading : setLoading;
      setLoadingState(true);
      setError(null);

      try {
        const fetchedNode = await ozmaClient.getNode(id);
        const extendedNode = extendNodeDetails(fetchedNode, nodes, activeNodeId);
        setNode(extendedNode);
        setSelectedNode(extendedNode);
      } catch (err) {
        const message =
          err instanceof OzmaApiError
            ? err.detail
            : err instanceof Error
            ? err.message
            : 'Failed to load node';
        setError(message);
        setSelectedNodeError(message);
      } finally {
        setLoadingState(false);
      }
    },
    [nodes, activeNodeId, setSelectedNode, setSelectedNodeError],
  );

  const activateNode = useCallback(async () => {
    if (!node) return;
    setActivating(true);
    try {
      const controllerUrl = getControllerUrl();
      const response = await fetch(
        `${controllerUrl}/api/v1/nodes/${encodeURIComponent(node.id)}/activate`,
        {method: 'POST'},
      );
      if (response.ok) {
        const data = await response.json();
        setActiveNodeId(data.active_node_id);
        // Update node state
        const updatedNode = {...node, online: true, isActive: true};
        setNode(updatedNode);
        updateSelectedNode(updatedNode);
        // Update global nodes list
        const updatedNodes = nodes.map((n) =>
          n.id === node.id ? {...n, online: true} : n,
        );
        setNodes(updatedNodes);
      } else {
        throw new Error(`Activation failed: ${response.status}`);
      }
    } catch (err) {
      const message =
        err instanceof OzmaApiError
          ? err.detail
          : err instanceof Error
          ? err.message
          : 'Failed to activate node';
      setError(message);
    } finally {
      setActivating(false);
    }
  }, [node, nodes, setActiveNodeId, setNodes, updateSelectedNode]);

  // WebSocket subscription for live updates
  useEffect(() => {
    let ws: WebSocket | null = null;
    let reconnectTimeout: NodeJS.Timeout | null = null;

    const connectWebSocket = () => {
      try {
        const controllerUrl = getControllerUrl();
        const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
        // Parse URL to get host and remove port for WebSocket
        const urlObj = new URL(controllerUrl);
        const wsUrl = `${protocol}//${urlObj.hostname}/api/v1/events`;

        ws = new WebSocket(wsUrl);

        ws.onopen = () => {
          setWsConnected(true);
          console.log(`WebSocket connected for node updates: ${nodeId}`);
        };

        ws.onclose = () => {
          setWsConnected(false);
          console.log('WebSocket disconnected, reconnecting in 3s...');
          reconnectTimeout = setTimeout(connectWebSocket, 3000);
        };

        ws.onerror = (err) => {
          console.log('WebSocket error:', err);
        };

        ws.onmessage = (event) => {
          try {
            const data = JSON.parse(event.data);
            // Handle node state updates
            if (data.type === 'node.changed' || data.type === 'node.status') {
              const updatedNodeId = data.node_id || data.node?.id;
              if (updatedNodeId === nodeId) {
                const updatedNode = data.node || data;
                const extendedNode = extendNodeDetails(updatedNode, nodes, activeNodeId);
                setNode(extendedNode);
                updateSelectedNode(extendedNode);
              }
            }
            // Handle active node changes
            if (
              data.type === 'active_node.changed' ||
              data.type === 'routing.changed' ||
              data.type === 'node.activated'
            ) {
              const newActiveNodeId = data.active_node_id || data.node_id;
              setActiveNodeId(newActiveNodeId);
            }
          } catch (e) {
            console.log('WebSocket message parse error:', e);
          }
        };
      } catch (err) {
        console.log('WebSocket connection failed, retrying...');
        reconnectTimeout = setTimeout(connectWebSocket, 3000);
      }
    };

    connectWebSocket();

    return () => {
      if (ws) {
        ws.close();
      }
      if (reconnectTimeout) {
        clearTimeout(reconnectTimeout);
      }
    };
  }, [nodeId, nodes, activeNodeId, setActiveNodeId, updateSelectedNode]);

  // Initial load
  useEffect(() => {
    loadNode(nodeId, false).catch(() => undefined);
  }, [loadNode, nodeId]);

  // Reload function
  const reload = useCallback(async () => {
    await loadNode(nodeId, true);
  }, [loadNode, nodeId]);

  return {
    node,
    loading,
    error,
    reloading,
    activating,
    wsConnected,
    reload,
    activateNode,
  };
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function extendNodeDetails(
  node: NodeInfo,
  nodes: NodeInfo[],
  activeNodeId: string | null,
): NodeDetailsExtended {
  const isActive = node.id === activeNodeId;
  const lastSeen = node.last_seen ? new Date(node.last_seen).getTime() : null;
  const uptime = lastSeen ? calculateUptime(lastSeen) : 'Unknown';

  return {
    id: node.id,
    name: node.name,
    host: node.host,
    port: node.port,
    machine_class: node.machine_class,
    last_seen: node.last_seen,
    online: node.online,
    mac_address: node.mac_address,
    camera_streams: node.camera_streams,
    frigate_host: node.frigate_host,
    frigate_port: node.frigate_port,
    direct_registered: node.direct_registered,
    agent_connected: node.agent_connected,
    ip_address: node.ip_address,
    platform: node.platform,
    os_version: node.os_version,
    uptime,
    isActive,
    streamPath: node.stream_path || null,
    streamPort: node.stream_port || null,
  };
}

function calculateUptime(lastSeen: number): string {
  const now = Date.now();
  const diff = now - lastSeen;
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) return 'just now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}
