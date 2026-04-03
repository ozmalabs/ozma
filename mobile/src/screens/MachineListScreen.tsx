/**
 * MachineListScreen — list of all nodes with their online status and WoL button.
 */

import React, {useCallback, useState} from 'react';
import {
  ActivityIndicator,
  FlatList,
  Platform,
  RefreshControl,
  StyleSheet,
  Text,
  TouchableOpacity,
  View,
} from 'react-native';
import {NodeStatusBadge} from '../components/NodeStatusBadge';
import {useMachines} from '../hooks/useMachines';
import {useStore} from '../store/useStore';
import {NodeInfo} from '../api/types';

interface WoLToast {
  nodeId: string;
  ok: boolean;
  message: string;
}

export function MachineListScreen() {
  const nodes = useStore((s) => s.nodes);
  const loading = useStore((s) => s.nodesLoading);
  const error = useStore((s) => s.nodesError);
  const activeNodeId = useStore((s) => s.activeNodeId);

  const {reload, sendWoL, wolLoading} = useMachines();
  const [toast, setToast] = useState<WoLToast | null>(null);

  const showToast = useCallback((t: WoLToast) => {
    setToast(t);
    setTimeout(() => setToast(null), 3000);
  }, []);

  const handleWoL = useCallback(
    async (node: NodeInfo) => {
      const result = await sendWoL(node.id);
      showToast({nodeId: node.id, ...result});
    },
    [sendWoL, showToast],
  );

  const renderNode = useCallback(
    ({item}: {item: NodeInfo}) => {
      const isActive = item.id === activeNodeId;
      const canWoL = !item.online && item.mac_address !== null;
      const isWolLoading = wolLoading[item.id] ?? false;

      return (
        <View style={[styles.card, isActive && styles.cardActive]}>
          <View style={styles.cardHeader}>
            <View style={styles.nameRow}>
              <Text style={styles.nodeName} numberOfLines={1}>
                {item.name}
              </Text>
              {isActive && (
                <View style={styles.activeBadge}>
                  <Text style={styles.activeBadgeText}>Active</Text>
                </View>
              )}
            </View>
            <NodeStatusBadge online={item.online} labeled />
          </View>

          <View style={styles.metaGrid}>
            <MetaRow label="Class" value={item.machine_class} />
            {item.ip_address && (
              <MetaRow label="IP" value={item.ip_address} />
            )}
            {item.platform && (
              <MetaRow label="Platform" value={item.platform} />
            )}
            {item.mac_address && (
              <MetaRow label="MAC" value={item.mac_address} />
            )}
            {item.last_seen && (
              <MetaRow
                label="Last seen"
                value={formatRelativeTime(item.last_seen)}
              />
            )}
          </View>

          {canWoL && (
            <TouchableOpacity
              style={[styles.wolButton, isWolLoading && styles.wolButtonLoading]}
              onPress={() => handleWoL(item)}
              disabled={isWolLoading}>
              {isWolLoading ? (
                <ActivityIndicator size="small" color="#FFFFFF" />
              ) : (
                <Text style={styles.wolButtonText}>Wake on LAN</Text>
              )}
            </TouchableOpacity>
          )}

          {toast?.nodeId === item.id && (
            <View style={[styles.toast, toast.ok ? styles.toastOk : styles.toastError]}>
              <Text style={styles.toastText}>{toast.message}</Text>
            </View>
          )}
        </View>
      );
    },
    [activeNodeId, wolLoading, handleWoL, toast],
  );

  if (loading && nodes.length === 0) {
    return (
      <View style={styles.centered}>
        <ActivityIndicator size="large" color="#2563EB" />
      </View>
    );
  }

  if (error && nodes.length === 0) {
    return (
      <View style={styles.centered}>
        <Text style={styles.errorText}>{error}</Text>
        <TouchableOpacity style={styles.retryButton} onPress={reload}>
          <Text style={styles.retryButtonText}>Retry</Text>
        </TouchableOpacity>
      </View>
    );
  }

  if (nodes.length === 0) {
    return (
      <View style={styles.centered}>
        <Text style={styles.emptyTitle}>No machines</Text>
        <Text style={styles.emptySubtitle}>
          Register nodes with the controller to see them here.
        </Text>
      </View>
    );
  }

  return (
    <FlatList
      data={nodes}
      renderItem={renderNode}
      keyExtractor={(item) => item.id}
      contentContainerStyle={styles.list}
      refreshControl={
        <RefreshControl refreshing={loading} onRefresh={reload} tintColor="#2563EB" />
      }
    />
  );
}

function MetaRow({label, value}: {label: string; value: string}) {
  return (
    <View style={styles.metaRow}>
      <Text style={styles.metaLabel}>{label}</Text>
      <Text style={styles.metaValue}>{value}</Text>
    </View>
  );
}

function formatRelativeTime(iso: string): string {
  const diff = Date.now() - new Date(iso).getTime();
  const seconds = Math.floor(diff / 1000);
  if (seconds < 60) {
    return 'just now';
  }
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) {
    return `${minutes}m ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return `${hours}h ago`;
  }
  return `${Math.floor(hours / 24)}d ago`;
}

const styles = StyleSheet.create({
  list: {
    padding: 12,
    gap: 10,
    backgroundColor: '#111827',
  },
  centered: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    backgroundColor: '#111827',
    padding: 24,
  },
  card: {
    backgroundColor: '#1F2937',
    borderRadius: 10,
    padding: 16,
    gap: 12,
    borderWidth: 1,
    borderColor: '#374151',
  },
  cardActive: {
    borderColor: '#2563EB',
  },
  cardHeader: {
    flexDirection: 'row',
    justifyContent: 'space-between',
    alignItems: 'flex-start',
  },
  nameRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: 8,
    flex: 1,
  },
  nodeName: {
    color: '#F9FAFB',
    fontSize: 16,
    fontWeight: '600',
    flex: 1,
  },
  activeBadge: {
    backgroundColor: '#1D4ED8',
    paddingHorizontal: 8,
    paddingVertical: 2,
    borderRadius: 10,
  },
  activeBadgeText: {
    color: '#BFDBFE',
    fontSize: 11,
    fontWeight: '600',
  },
  metaGrid: {
    gap: 6,
  },
  metaRow: {
    flexDirection: 'row',
    justifyContent: 'space-between',
  },
  metaLabel: {
    color: '#6B7280',
    fontSize: 13,
  },
  metaValue: {
    color: '#D1D5DB',
    fontSize: 13,
    fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace',
  },
  wolButton: {
    backgroundColor: '#065F46',
    paddingVertical: 10,
    borderRadius: 8,
    alignItems: 'center',
  },
  wolButtonLoading: {
    opacity: 0.6,
  },
  wolButtonText: {
    color: '#D1FAE5',
    fontWeight: '600',
    fontSize: 14,
  },
  toast: {
    paddingHorizontal: 12,
    paddingVertical: 8,
    borderRadius: 6,
  },
  toastOk: {
    backgroundColor: '#064E3B',
  },
  toastError: {
    backgroundColor: '#7F1D1D',
  },
  toastText: {
    color: '#FFFFFF',
    fontSize: 13,
    textAlign: 'center',
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
  },
  retryButtonText: {
    color: '#FFFFFF',
    fontWeight: '600',
  },
  emptyTitle: {
    color: '#E5E7EB',
    fontSize: 18,
    fontWeight: '600',
    marginBottom: 8,
  },
  emptySubtitle: {
    color: '#6B7280',
    fontSize: 14,
    textAlign: 'center',
  },
});
