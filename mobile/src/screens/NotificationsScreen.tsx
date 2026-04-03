/**
 * NotificationsScreen — history of push notifications from the controller.
 *
 * Fetches from GET /api/v1/notifications, shows snapshot thumbnails inline.
 * Marks notifications as read when viewed.
 */

import React, {useCallback, useEffect, useRef} from 'react';
import {
  ActivityIndicator,
  FlatList,
  Image,
  RefreshControl,
  StyleSheet,
  Text,
  TouchableOpacity,
  View,
} from 'react-native';
import {useNavigation} from '@react-navigation/native';
import {NativeStackNavigationProp} from '@react-navigation/native-stack';
import {ozmaClient} from '../api/client';
import {useStore} from '../store/useStore';
import {NotificationRecord, OzmaApiError} from '../api/types';
import {RootStackParamList} from '../navigation/AppNavigator';

type NavProp = NativeStackNavigationProp<RootStackParamList>;

export function NotificationsScreen() {
  const navigation = useNavigation<NavProp>();

  const notifications = useStore((s) => s.notifications);
  const loading = useStore((s) => s.notificationsLoading);
  const error = useStore((s) => s.notificationsError);
  const setNotifications = useStore((s) => s.setNotifications);
  const setNotificationsLoading = useStore((s) => s.setNotificationsLoading);
  const setNotificationsError = useStore((s) => s.setNotificationsError);
  const setUnreadCount = useStore((s) => s.setUnreadCount);
  const markRead = useStore((s) => s.markNotificationRead);

  // Track which notifications have been marked read in this session.
  const markedRef = useRef<Set<string>>(new Set());

  const fetchNotifications = useCallback(async () => {
    setNotificationsLoading(true);
    setNotificationsError(null);
    try {
      const response = await ozmaClient.listNotifications({limit: 50});
      setNotifications(response.notifications);
      setUnreadCount(response.unread_count);
    } catch (err) {
      const message =
        err instanceof OzmaApiError
          ? err.detail
          : err instanceof Error
          ? err.message
          : 'Failed to load notifications';
      setNotificationsError(message);
    } finally {
      setNotificationsLoading(false);
    }
  }, [setNotifications, setNotificationsLoading, setNotificationsError, setUnreadCount]);

  useEffect(() => {
    fetchNotifications().catch(() => undefined);
  }, [fetchNotifications]);

  const handleViewableItemsChanged = useCallback(
    ({viewableItems}: {viewableItems: Array<{item: NotificationRecord}>}) => {
      viewableItems.forEach(({item}) => {
        if (!item.read && !markedRef.current.has(item.id)) {
          markedRef.current.add(item.id);
          markRead(item.id);
          ozmaClient.markNotificationRead(item.id).catch(() => undefined);
        }
      });
    },
    [markRead],
  );

  const handleNotificationPress = useCallback(
    (notification: NotificationRecord) => {
      if (notification.camera_id) {
        navigation.navigate('CameraDetail', {
          cameraId: notification.camera_id,
          cameraName: `Camera ${notification.camera_id}`,
        });
      }
    },
    [navigation],
  );

  const renderItem = useCallback(
    ({item}: {item: NotificationRecord}) => {
      const hasCameraLink = !!item.camera_id;
      const snapshotUri = item.snapshot_url ?? null;

      return (
        <TouchableOpacity
          style={[styles.item, !item.read && styles.itemUnread]}
          onPress={() => handleNotificationPress(item)}
          activeOpacity={hasCameraLink ? 0.7 : 1}>
          {!item.read && <View style={styles.unreadDot} />}

          <View style={styles.itemContent}>
            <View style={styles.itemHeader}>
              <Text style={styles.itemTitle} numberOfLines={2}>
                {item.title}
              </Text>
              <Text style={styles.itemTime}>
                {formatRelativeTime(item.created_at)}
              </Text>
            </View>

            {item.body ? (
              <Text style={styles.itemBody} numberOfLines={3}>
                {item.body}
              </Text>
            ) : null}

            {snapshotUri && (
              <Image
                source={{uri: snapshotUri}}
                style={styles.snapshot}
                resizeMode="cover"
              />
            )}

            {item.camera_id && (
              <Text style={styles.tapHint}>Tap to view camera</Text>
            )}
          </View>
        </TouchableOpacity>
      );
    },
    [handleNotificationPress],
  );

  if (loading && notifications.length === 0) {
    return (
      <View style={styles.centered}>
        <ActivityIndicator size="large" color="#2563EB" />
      </View>
    );
  }

  if (error && notifications.length === 0) {
    return (
      <View style={styles.centered}>
        <Text style={styles.errorText}>{error}</Text>
        <TouchableOpacity style={styles.retryButton} onPress={fetchNotifications}>
          <Text style={styles.retryButtonText}>Retry</Text>
        </TouchableOpacity>
      </View>
    );
  }

  if (notifications.length === 0) {
    return (
      <View style={styles.centered}>
        <Text style={styles.emptyTitle}>No notifications</Text>
        <Text style={styles.emptySubtitle}>
          Motion alerts and system events will appear here.
        </Text>
      </View>
    );
  }

  return (
    <FlatList
      data={notifications}
      renderItem={renderItem}
      keyExtractor={(item) => item.id}
      contentContainerStyle={styles.list}
      onViewableItemsChanged={handleViewableItemsChanged}
      viewabilityConfig={{viewAreaCoveragePercentThreshold: 80}}
      refreshControl={
        <RefreshControl
          refreshing={loading}
          onRefresh={fetchNotifications}
          tintColor="#2563EB"
        />
      }
    />
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
  return new Date(iso).toLocaleDateString();
}

const styles = StyleSheet.create({
  list: {
    backgroundColor: '#111827',
    paddingVertical: 4,
  },
  centered: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    backgroundColor: '#111827',
    padding: 24,
  },
  item: {
    flexDirection: 'row',
    padding: 14,
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: '#374151',
    backgroundColor: '#111827',
  },
  itemUnread: {
    backgroundColor: '#1A2540',
  },
  unreadDot: {
    width: 8,
    height: 8,
    borderRadius: 4,
    backgroundColor: '#2563EB',
    marginTop: 6,
    marginRight: 10,
    flexShrink: 0,
  },
  itemContent: {
    flex: 1,
    gap: 6,
  },
  itemHeader: {
    flexDirection: 'row',
    justifyContent: 'space-between',
    gap: 8,
  },
  itemTitle: {
    color: '#F9FAFB',
    fontSize: 14,
    fontWeight: '600',
    flex: 1,
  },
  itemTime: {
    color: '#6B7280',
    fontSize: 12,
    flexShrink: 0,
  },
  itemBody: {
    color: '#9CA3AF',
    fontSize: 13,
    lineHeight: 18,
  },
  snapshot: {
    width: '100%',
    height: 160,
    borderRadius: 6,
    backgroundColor: '#1F2937',
    marginTop: 4,
  },
  tapHint: {
    color: '#3B82F6',
    fontSize: 12,
    marginTop: 2,
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
