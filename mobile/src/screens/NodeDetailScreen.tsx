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
import {NodeStatusBadge} from '../components/NodeStatusBadge';
import {RootStackParamList} from '../navigation/AppNavigator';
import {useQuery, useMutation} from '@urql/react';
import {graphqlClient, connectWebSocket, addWebSocketListener, closeWebSocket} from '../graphql/client';
import {
  GET_NODE,
  ACTIVATE_NODE,
  SUBSCRIBE_NODE_CHANGED,
  NODE_FRAGMENT,
} from '../graphql/queries';
import {NodeDetails, DisplayOutput, Scenario} from '../store/useStore';

// Helper to convert GraphQL node to NodeDetails for compatibility
function graphqlNodeToNodeDetails(node: any): NodeDetails | null {
  if (!node) return null;
  return {
    id: node.id,
    name: node.name,
    host: node.host,
    port: node.port,
    role: node.role || '',
    hw: node.hw || '',
    fwVersion: node.fwVersion || '',
    protoVersion: node.protoVersion || 0,
    capabilities: node.capabilities || [],
    machineClass: node.machineClass || '',
    lastSeen: node.lastSeen,
    displayOutputs: node.displayOutputs || [],
    vncHost: node.vncHost,
    vncPort: node.vncPort,
    streamPort: node.streamPort,
    streamPath: node.streamPath,
    audioType: node.audioType,
    audioSink: node.audioSink,
    audioVBANPort: node.audioVBANPort,
    micVBANPort: node.micVBANPort,
    captureDevice: node.captureDevice,
    cameraStreams: node.cameraStreams || [],
    frigateHost: node.frigateHost,
    frigatePort: node.frigatePort,
    ownerUserId: node.ownerUserId,
    owner: node.owner,
    sharedWith: node.sharedWith || [],
    sharePermissions: node.sharePermissions || [],
    parentId: node.parentId,
    sunshinePort: node.sunshinePort,
    // Legacy fields for compatibility
    online: true, // Assume online if we have data
    mac_address: null,
    direct_registered: false,
    agent_connected: false,
    ip_address: null,
    platform: null,
    os_version: null,
  };
}

type Props = NativeStackScreenProps<RootStackParamList, 'NodeDetail'>;

export function NodeDetailScreen({route, navigation}: Props) {
  const {nodeId} = route.params;

  const setSelectedNodeId = useStore((s) => s.nodeStore.setSelectedNodeId);
  const setSelectedNode = useStore((s) => s.nodeStore.setSelectedNode);
  const setSelectedNodeLoading = useStore((s) => s.nodeStore.setSelectedNodeLoading);
  const setSelectedNodeError = useStore((s) => s.nodeStore.setSelectedNodeError);
  const updateSelectedNode = useStore((s) => s.nodeStore.updateSelectedNode);
  const clearSelectedNode = useStore((s) => s.nodeStore.clearSelectedNode);
  const setActiveNodeId = useStore((s) => s.setActiveNodeId);
  const activeNodeId = useStore((s) => s.activeNodeId);

  const [wsConnected, setWsConnected] = useState(false);
  const [activating, setActivating] = useState(false);
  const [toast, setToast] = useState<{message: string; type: 'success' | 'error'} | null>(null);

  // Display toast for 3 seconds
  const showToast = useCallback((message: string, type: 'success' | 'error') => {
    setToast({message, type});
    setTimeout(() => setToast(null), 3000);
  }, []);

  // ── GraphQL query for node data ─────────────────────────────────────────────

  const [{data: nodeData, fetching: nodeLoading, error: nodeError}, reexecuteQuery] = useQuery({
    query: GET_NODE,
    variables: {id: nodeId},
    pause: false,
  });

  // ── Activate node mutation ──────────────────────────────────────────────────

  const [{fetching: activateLoading}, executeActivateMutation] = useMutation(ACTIVATE_NODE);

  const handleActivateNode = useCallback(async () => {
    if (!nodeData?.node) return;
    setActivating(true);
    try {
      const response = await executeActivateMutation({id: nodeId});
      if (response.data?.activateNode) {
        setActiveNodeId(response.data.activateNode.id);
        showToast('Node activated successfully', 'success');
      } else if (response.error) {
        showToast(response.error.message || 'Failed to activate node', 'error');
      }
    } catch (err) {
      showToast(err instanceof Error ? err.message : 'Failed to activate node', 'error');
    } finally {
      setActivating(false);
    }
  }, [nodeData, nodeId, executeActivateMutation, setActiveNodeId, showToast]);

  // ── WebSocket subscription for live updates ─────────────────────────────────

  useEffect(() => {
    // Connect WebSocket
    const ws = connectWebSocket((data) => {
      // Handle subscription messages
      if (data.data?.nodeChanged) {
        const updatedNode = data.data.nodeChanged;
        if (updatedNode.id === nodeId) {
          updateSelectedNode(updatedNode);
        }
      }
      // Handle active node changes from subscription
      if (data.data?.nodeChanged && data.data.nodeChanged.id === activeNodeId) {
        // Active node changed, refresh current node data
        reexecuteQuery({requestPolicy: 'network-only'});
      }
    });

    setWsConnected(ws.readyState === WebSocket.OPEN);

    ws.onopen = () => setWsConnected(true);
    ws.onclose = () => setWsConnected(false);

    return () => {
      closeWebSocket();
    };
  }, [nodeId, activeNodeId, reexecuteQuery, updateSelectedNode]);

  // ── Cleanup on unmount ──────────────────────────────────────────────────────

  useEffect(() => {
    return () => {
      clearSelectedNode();
      closeWebSocket();
    };
  }, [clearSelectedNode]);

  // ── Update store when GraphQL data loads ───────────────────────────────────

  useEffect(() => {
    if (nodeLoading) {
      setSelectedNodeLoading(true);
    } else if (nodeError) {
      setSelectedNodeError(nodeError.message);
      setSelectedNode(null);
    } else if (nodeData?.node) {
      const nodeDetails = graphqlNodeToNodeDetails(nodeData.node);
      setSelectedNode(nodeDetails);
      setSelectedNodeError(null);
      setSelectedNodeLoading(false);
    }
  }, [nodeData, nodeLoading, nodeError, setSelectedNode, setSelectedNodeError, setSelectedNodeLoading]);

  // ── Navigation header update ────────────────────────────────────────────────

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

  // ── Derived state ───────────────────────────────────────────────────────────

  const node = useMemo(() => {
    return nodeData?.node as NodeDetails | undefined;
  }, [nodeData]);

  const streamUrl = useMemo(() => {
    if (!node?.streamPath) return null;
    const base = getControllerUrl();
    return node.streamPath.startsWith('http')
      ? node.streamPath
      : `${base}${node.streamPath}`;
  }, [node]);

  const isNodeActive = node?.id === activeNodeId;
  const uptime = useMemo(() => {
    if (!node?.lastSeen) return 'Unknown';
    try {
      const diff = Date.now() - new Date(node.lastSeen).getTime();
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
  }, [node?.lastSeen]);

  const machineClassLabel = useMemo(() => {
    if (!node?.machineClass) return 'Unknown';
    const map: Record<string, string> = {
      WORKSTATION: 'Workstation',
      SERVER: 'Server',
      KIOSK: 'Kiosk',
      CAMERA: 'Camera',
    };
    return map[node.machineClass] || 'Unknown';
  }, [node?.machineClass]);

  // ── Render loading state ────────────────────────────────────────────────────

  if (nodeLoading && !node) {
    return (
      <View style={[styles.centered, {paddingTop: useSafeAreaInsets().top}]}>
        <ActivityIndicator size="large" color="#2563EB" />
      </View>
    );
  }

  // ── Render error state ──────────────────────────────────────────────────────

  if (nodeError || !node) {
    return (
      <View style={[styles.centered, {paddingTop: useSafeAreaInsets().top}]}>
        <Text style={styles.errorText}>{nodeError?.message ?? 'Node not found'}</Text>
        <TouchableOpacity style={styles.retryButton} onPress={() => reexecuteQuery({requestPolicy: 'network-only'})}>
          <Text style={styles.retryButtonText}>Retry</Text>
        </TouchableOpacity>
      </View>
    );
  }

  // ── Render content ──────────────────────────────────────────────────────────

  return (
    <ScrollView
      style={[styles.container, {paddingTop: useSafeAreaInsets().top}]}
      contentContainerStyle={styles.content}>
      {/* Toast notification */}
      {toast && (
        <View style={[styles.toast, toast.type === 'success' ? styles.toastSuccess : styles.toastError]}>
          <Text style={styles.toastText}>{toast.message}</Text>
        </View>
      )}

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
              {node.streamPort && (
                <Text style={styles.streamPort}>Port: {node.streamPort}</Text>
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
          {node.lastSeen && (
            <DetailRow label="Last Seen" value={node.lastSeen} />
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
  toast: {
    position: 'absolute',
    top: 50,
    left: 16,
    right: 16,
    paddingVertical: 12,
    paddingHorizontal: 16,
    borderRadius: 8,
    alignItems: 'center',
    zIndex: 100,
  },
  toastSuccess: {
    backgroundColor: '#064E3B',
  },
  toastError: {
    backgroundColor: '#7F1D1D',
  },
  toastText: {
    color: '#FFFFFF',
    fontSize: 14,
    textAlign: 'center',
  },
});
