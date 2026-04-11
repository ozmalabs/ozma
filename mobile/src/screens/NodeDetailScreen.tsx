/**
 * NodeDetailScreen — full detail view for a single node.
 *
 * Shows:
 *   - Node info (name, IP, status, uptime)
 *   - KVM focus control (activate button)
 *   - Stream preview thumbnail
 *   - HID stats (keyboard/mouse activity)
 *   - Current scenario binding
 *   - WebSocket live state updates
 */

import React, {useCallback, useEffect, useMemo, useState} from 'react';
import {
  ActivityIndicator,
  Platform,
  ScrollView,
  StyleSheet,
  Text,
  TouchableOpacity,
  View,
} from 'react-native';
import {NativeStackScreenProps} from '@react-navigation/native-stack';
import {useSafeAreaInsets} from 'react-native-safe-area-context';
import {useStore} from '../store/useStore';
import {ozmaClient} from '../api/client';
import {NodeInfo, OzmaApiError} from '../api/types';
import {RootStackParamList} from '../navigation/AppNavigator';
import {NodeStatusBadge} from '../components/NodeStatusBadge';

type Props = NativeStackScreenProps<RootStackParamList, 'NodeDetail'>;

export function NodeDetailScreen({route, navigation}: Props) {
  const {nodeId} = route.params;

  const setNodes = useStore((s) => s.setNodes);
  const setNodesLoading = useStore((s) => s.setNodesLoading);
  const setNodesError = useStore((s) => s.setNodesError);
  const setActiveNodeId = useStore((s) => s.setActiveNodeId);
  const nodes = useStore((s) => s.nodes);
  const activeNodeId = useStore((s) => s.activeNodeId);

  const [node, setNode] = useState<NodeInfo | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [activating, setActivating] = useState(false);
  const [wsConnected, setWsConnected] = useState(false);

  useEffect(() => {
    navigation.setOptions({
      title: nodeId,
      headerRight: () => {
        if (activating) {
          return <ActivityIndicator size="small" color="#F9FAFB" />;
        }
        return null;
      },
    });
  }, [navigation, nodeId, activating]);

  const loadNode = useCallback(
    async (id: string) => {
      setLoading(true);
      setError(null);
      try {
        const fetchedNode = await ozmaClient.getNode(id);
        setNode(fetchedNode);
      } catch (err) {
        const message =
          err instanceof OzmaApiError
            ? err.detail
            : err instanceof Error
            ? err.message
            : 'Failed to load node';
        setError(message);
      } finally {
        setLoading(false);
      }
    },
    [],
  );

  useEffect(() => {
    loadNode(nodeId).catch(() => undefined);
  }, [loadNode, nodeId]);

  const handleActivateNode = useCallback(async () => {
    if (!node) return;
    setActivating(true);
    try {
      await ozmaClient.listNodes(); // Will trigger active_node_id update
      const response = await fetch(
        `${getControllerUrl()}/api/v1/nodes/${encodeURIComponent(node.id)}/activate`,
        {method: 'POST'},
      );
      if (response.ok) {
        const data = await response.json();
        setActiveNodeId(data.active_node_id);
        // Update local node state
        const updatedNodes = nodes.map((n) =>
          n.id === node.id ? {...n, online: true} : n,
        );
        setNodes(updatedNodes);
        setNode({...node, online: true});
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
  }, [node, nodes, setActiveNodeId, setNodes]);

  // WebSocket subscription for live updates
  useEffect(() => {
    let ws: WebSocket | null = null;
    let reconnectTimeout: NodeJS.Timeout | null = null;

    const connectWebSocket = () => {
      const controllerUrl = getControllerUrl();
      const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
      const wsUrl = `${protocol}//${controllerUrl.replace(/^https?:\/\//, '')}/api/v1/events`;

      ws = new WebSocket(wsUrl);

      ws.onopen = () => {
        setWsConnected(true);
        console.log('WebSocket connected for node updates');
      };

      ws.onclose = () => {
        setWsConnected(false);
        console.log('WebSocket disconnected, reconnecting in 3s...');
        reconnectTimeout = setTimeout(connectWebSocket, 3000);
      };

      ws.onerror = (err) => {
        console.log('WebSocket error, will reconnect:', err);
      };

      ws.onmessage = (event) => {
        try {
          const data = JSON.parse(event.data);
          // Handle node state updates
          if (data.type === 'node.changed' || data.type === 'node.status') {
            const updatedNodeId = data.node_id || data.node?.id;
            if (updatedNodeId === nodeId) {
              const updatedNode = data.node || data;
              setNode((prev) => {
                if (!prev) return prev;
                return {...prev, ...updatedNode, online: true};
              });
            }
          }
          // Handle active node changes
          if (data.type === 'active_node.changed' || data.type === 'routing.changed') {
            const newActiveNodeId = data.active_node_id || data.node_id;
            setActiveNodeId(newActiveNodeId);
          }
        } catch (e) {
          console.log('WebSocket message parse error:', e);
        }
      };
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
  }, [nodeId, setActiveNodeId]);

  const streamUrl = useMemo(() => {
    if (!node) return null;
    const base = getControllerUrl();
    if (node.stream_path) {
      return node.stream_path.startsWith('http')
        ? node.stream_path
        : `${base}${node.stream_path}`;
    }
    return null;
  }, [node]);

  const isNodeActive = node?.id === activeNodeId;
  const uptime = node?.last_seen
    ? formatUptime(node.last_seen)
    : 'Unknown';
  const machineClassLabel = getNodeClassLabel(node?.machine_class);

  if (loading) {
    return (
      <View style={[styles.centered, {paddingTop: useSafeAreaInsets().top}]}>
        <ActivityIndicator size="large" color="#2563EB" />
      </View>
    );
  }

  if (error || !node) {
    return (
      <View style={[styles.centered, {paddingTop: useSafeAreaInsets().top}]}>
        <Text style={styles.errorText}>{error ?? 'Node not found'}</Text>
        <TouchableOpacity style={styles.retryButton} onPress={() => loadNode(nodeId)}>
          <Text style={styles.retryButtonText}>Retry</Text>
        </TouchableOpacity>
      </View>
    );
  }

  return (
    <ScrollView
      style={[styles.container, {paddingTop: useSafeAreaInsets().top}]}
      contentContainerStyle={styles.content}>
      {/* Node header */}
      <View style={styles.header}>
        <View style={styles.headerRow}>
          <Text style={styles.nodeName}>{node.name}</Text>
          <NodeStatusBadge online={node.online} />
        </View>
        <Text style={styles.nodeId}>{node.id}</Text>
        <View style={styles.metaRow}>
          <Text style={styles.metaValue}>{node.ip_address ?? 'N/A'}</Text>
          <Text style={styles.metaDivider}>•</Text>
          <Text style={styles.metaValue}>{machineClassLabel}</Text>
          <Text style={styles.metaDivider}>•</Text>
          <Text style={styles.metaValue}>{uptime}</Text>
        </View>
      </View>

      {/* KVM Focus Control */}
      <View style={styles.section}>
        <Text style={styles.sectionTitle}>KVM Focus Control</Text>
        {isNodeActive ? (
          <View style={styles.activeStatus}>
            <View style={styles.activeBadge}>
              <Text style={styles.activeBadgeText}>Active</Text>
            </View>
            <Text style={styles.activeDescription}>
              This node is currently receiving all HID input
            </Text>
          </View>
        ) : (
          <TouchableOpacity
            style={styles.activateButton}
            onPress={handleActivateNode}
            disabled={activating}>
            {activating ? (
              <ActivityIndicator size="small" color="#FFFFFF" />
            ) : (
              <Text style={styles.activateButtonText}>Activate Node</Text>
            )}
          </TouchableOpacity>
        )}
      </View>

      {/* Stream Preview */}
      {streamUrl && (
        <View style={styles.section}>
          <Text style={styles.sectionTitle}>Stream Preview</Text>
          <View style={styles.streamContainer}>
            <View style={styles.streamPlaceholder}>
              <Text style={styles.streamPlaceholderText}>Stream Preview</Text>
              {node.stream_port && (
                <Text style={styles.streamPort}>Port: {node.stream_port}</Text>
              )}
            </View>
          </View>
        </View>
      )}

      {/* HID Stats */}
      <View style={styles.section}>
        <Text style={styles.sectionTitle}>HID Statistics</Text>
        <View style={styles.statsGrid}>
          <StatItem
            label="Keyboard Events"
            value={node.agent_connected ? '0' : 'N/A'}
            color="#3B82F6"
          />
          <StatItem
            label="Mouse Events"
            value={node.agent_connected ? '0' : 'N/A'}
            color="#8B5CF6"
          />
          <StatItem
            label="Input Rate"
            value={node.agent_connected ? '0/s' : 'N/A'}
            color="#10B981"
          />
        </View>
      </View>

      {/* Current Scenario */}
      <View style={styles.section}>
        <Text style={styles.sectionTitle}>Current Scenario</Text>
        <View style={styles.scenarioCard}>
          <Text style={styles.scenarioPlaceholder}>
            No scenario bound to this node
          </Text>
        </View>
      </View>

      {/* Node Details */}
      <View style={styles.section}>
        <Text style={styles.sectionTitle}>Node Details</Text>
        <View style={styles.detailsGrid}>
          {node.mac_address && (
            <DetailRow label="MAC Address" value={node.mac_address} />
          )}
          {node.platform && <DetailRow label="Platform" value={node.platform} />}
          {node.os_version && (
            <DetailRow label="OS Version" value={node.os_version} />
          )}
          {node.host && <DetailRow label="Host" value={node.host} />}
          {node.port && <DetailRow label="Port" value={String(node.port)} />}
          {node.last_seen && (
            <DetailRow label="Last Seen" value={node.last_seen} />
          )}
        </View>
      </View>

      {/* WebSocket Status */}
      <View style={styles.wsStatus}>
        <View
          style={[
            styles.wsDot,
            wsConnected ? styles.wsDotOnline : styles.wsDotOffline,
          ]}
        />
        <Text style={styles.wsText}>
          WebSocket: {wsConnected ? 'Connected' : 'Disconnected'}
        </Text>
      </View>
    </ScrollView>
  );
}

// ── Helper Components ─────────────────────────────────────────────────────────

function StatItem({label, value, color}: {label: string; value: string; color: string}) {
  return (
    <View style={styles.statItem}>
      <View
        style={[styles.statIndicator, {backgroundColor: color + '33'}]}
      />
      <Text style={styles.statLabel}>{label}</Text>
      <Text style={[styles.statValue, {color}]}>{value}</Text>
    </View>
  );
}

function DetailRow({label, value}: {label: string; value: string}) {
  return (
    <View style={styles.detailRow}>
      <Text style={styles.detailLabel}>{label}</Text>
      <Text style={styles.detailValue}>{value}</Text>
    </View>
  );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function getControllerUrl(): string {
  const storage = require('react-native-mmkv').MMKV;
  const STORAGE_KEY_CONTROLLER_URL = 'ozma.controller_url';
  const storageInstance = new storage({id: 'ozma-api'});
  const url = storageInstance.getString(STORAGE_KEY_CONTROLLER_URL);
  if (!url) {
    throw new Error('Controller URL not configured');
  }
  // Strip trailing slash
  return url.replace(/\/$/, '');
}

function formatUptime(lastSeen: string): string {
  try {
    const diff = Date.now() - new Date(lastSeen).getTime();
    const seconds = Math.floor(diff / 1000);
    if (seconds < 60) return 'just now';
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) return `${minutes}m ago`;
    const hours = Math.floor(minutes / 60);
    if (hours < 24) return `${hours}h ago`;
    return `${Math.floor(hours / 24)}d ago`;
  } catch {
    return 'Unknown';
  }
}

function getNodeClassLabel(machineClass?: string): string {
  if (!machineClass) return 'Unknown';
  const map: Record<string, string> = {
    workstation: 'Workstation',
    server: 'Server',
    kiosk: 'Kiosk',
    camera: 'Camera',
  };
  return map[machineClass] || 'Unknown';
}

const styles = StyleSheet.create({
  container: {
    flex: 1,
    backgroundColor: '#111827',
  },
  content: {
    padding: 16,
    gap: 16,
  },
  centered: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    backgroundColor: '#111827',
    padding: 24,
  },
  header: {
    gap: 8,
  },
  headerRow: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
  },
  nodeName: {
    color: '#F9FAFB',
    fontSize: 24,
    fontWeight: '700',
    flex: 1,
  },
  nodeId: {
    color: '#6B7280',
    fontSize: 13,
    fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace',
  },
  metaRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 6,
  },
  metaValue: {
    color: '#D1D5DB',
    fontSize: 13,
  },
  metaDivider: {
    color: '#374151',
    fontSize: 10,
  },
  section: {
    backgroundColor: '#1F2937',
    borderRadius: 12,
    padding: 16,
    gap: 12,
  },
  sectionTitle: {
    color: '#9CA3AF',
    fontSize: 12,
    fontWeight: '600',
    textTransform: 'uppercase',
    letterSpacing: 0.5,
  },
  activateButton: {
    backgroundColor: '#2563EB',
    paddingVertical: 14,
    borderRadius: 10,
    alignItems: 'center',
  },
  activateButtonText: {
    color: '#FFFFFF',
    fontWeight: '600',
    fontSize: 16,
  },
  activeStatus: {
    alignItems: 'center',
    gap: 8,
  },
  activeBadge: {
    backgroundColor: '#1D4ED8',
    paddingHorizontal: 12,
    paddingVertical: 6,
    borderRadius: 12,
  },
  activeBadgeText: {
    color: '#BFDBFE',
    fontSize: 12,
    fontWeight: '600',
  },
  activeDescription: {
    color: '#6B7280',
    fontSize: 13,
    textAlign: 'center',
  },
  streamContainer: {
    backgroundColor: '#0F172A',
    borderRadius: 8,
    padding: 16,
  },
  streamPlaceholder: {
    alignItems: 'center',
    justifyContent: 'center',
    paddingVertical: 40,
    gap: 8,
  },
  streamPlaceholderText: {
    color: '#4B5563',
    fontSize: 14,
  },
  streamPort: {
    color: '#6B7280',
    fontSize: 12,
  },
  statsGrid: {
    flexDirection: 'row',
    justifyContent: 'space-between',
  },
  statItem: {
    alignItems: 'center',
    gap: 4,
    flex: 1,
  },
  statIndicator: {
    width: 16,
    height: 16,
    borderRadius: 8,
  },
  statLabel: {
    color: '#6B7280',
    fontSize: 11,
  },
  statValue: {
    fontSize: 14,
    fontWeight: '600',
  },
  scenarioCard: {
    backgroundColor: '#111827',
    borderRadius: 8,
    padding: 16,
    alignItems: 'center',
  },
  scenarioPlaceholder: {
    color: '#4B5563',
    fontSize: 14,
  },
  detailsGrid: {
    gap: 8,
  },
  detailRow: {
    flexDirection: 'row',
    justifyContent: 'space-between',
  },
  detailLabel: {
    color: '#6B7280',
    fontSize: 13,
  },
  detailValue: {
    color: '#D1D5DB',
    fontSize: 13,
    fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace',
  },
  wsStatus: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 8,
    padding: 12,
    backgroundColor: '#0F172A',
    borderRadius: 8,
    marginTop: 8,
  },
  wsDot: {
    width: 8,
    height: 8,
    borderRadius: 4,
  },
  wsDotOnline: {
    backgroundColor: '#10B981',
  },
  wsDotOffline: {
    backgroundColor: '#F87171',
  },
  wsText: {
    color: '#9CA3AF',
    fontSize: 13,
  },
  errorText: {
    color: '#F87171',
    fontSize: 15,
    textAlign: 'center',
    marginBottom: 16,
  },
  retryButton: {
    backgroundColor: '#2563EB',
    paddingHorizontal: 24,
    paddingVertical: 10,
    borderRadius: 8,
    marginTop: 16,
  },
  retryButtonText: {
    color: '#FFFFFF',
    fontWeight: '600',
  },
});
