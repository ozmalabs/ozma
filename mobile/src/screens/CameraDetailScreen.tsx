/**
 * CameraDetailScreen — full-screen single camera view.
 *
 * - Portrait: player + info card below
 * - Landscape: full-screen player, info overlay
 * - Unmuted audio
 * - Snapshot share button
 */

import React, {useCallback, useEffect, useMemo, useState} from 'react';
import {
  ActivityIndicator,
  Dimensions,
  Image,
  Platform,
  ScrollView,
  Share,
  StyleSheet,
  Text,
  TouchableOpacity,
  useWindowDimensions,
  View,
} from 'react-native';
import {NativeStackScreenProps} from '@react-navigation/native-stack';
import {useSafeAreaInsets} from 'react-native-safe-area-context';
import {VideoPlayer} from '../components/VideoPlayer';
import {ozmaClient} from '../api/client';
import {Camera, OzmaApiError} from '../api/types';
import {RootStackParamList} from '../navigation/AppNavigator';

type Props = NativeStackScreenProps<RootStackParamList, 'CameraDetail'>;

export function CameraDetailScreen({route, navigation}: Props) {
  const {cameraId, cameraName} = route.params;
  const insets = useSafeAreaInsets();
  const {width, height} = useWindowDimensions();
  const isLandscape = width > height;

  const [camera, setCamera] = useState<Camera | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [snapshotLoading, setSnapshotLoading] = useState(false);

  useEffect(() => {
    navigation.setOptions({title: cameraName});
  }, [navigation, cameraName]);

  const loadCamera = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const cam = await ozmaClient.getCamera(cameraId);
      setCamera(cam);
    } catch (err) {
      const message =
        err instanceof OzmaApiError
          ? err.detail
          : err instanceof Error
          ? err.message
          : 'Failed to load camera';
      setError(message);
    } finally {
      setLoading(false);
    }
  }, [cameraId]);

  useEffect(() => {
    loadCamera().catch(() => undefined);
  }, [loadCamera]);

  const streamUrl = useMemo(
    () => (camera ? ozmaClient.buildStreamUrl(camera) : null),
    [camera],
  );

  const snapshotUrl = useMemo(
    () => (camera ? ozmaClient.buildSnapshotUrl(camera) : null),
    [camera],
  );

  const isCapturing = camera?.active ?? false;
  const resolution = camera ? `${camera.width}x${camera.height}` : null;

  const handleShareSnapshot = useCallback(async () => {
    if (!camera) {
      return;
    }
    setSnapshotLoading(true);
    try {
      const url = ozmaClient.getSnapshotUrl(camera.id);
      await Share.share({
        title: camera.name,
        url: Platform.OS === 'ios' ? url : undefined,
        message: Platform.OS === 'android' ? url : camera.name,
      });
    } catch {
      // User cancelled share sheet — no-op.
    } finally {
      setSnapshotLoading(false);
    }
  }, [camera]);

  const playerHeight = isLandscape ? height : Math.round(width * (9 / 16));

  if (loading) {
    return (
      <View style={[styles.centered, {paddingTop: insets.top}]}>
        <ActivityIndicator size="large" color="#2563EB" />
      </View>
    );
  }

  if (error || !camera || !streamUrl) {
    return (
      <View style={[styles.centered, {paddingTop: insets.top}]}>
        <Text style={styles.errorText}>{error ?? 'Camera not found'}</Text>
        <TouchableOpacity style={styles.retryButton} onPress={loadCamera}>
          <Text style={styles.retryButtonText}>Retry</Text>
        </TouchableOpacity>
      </View>
    );
  }

  return (
    <View style={[styles.container, isLandscape && styles.containerLandscape]}>
      {/* Video player */}
      <VideoPlayer
        uri={streamUrl}
        posterUri={snapshotUrl ?? undefined}
        muted={false}
        showControls={false}
        style={{
          width,
          height: playerHeight,
        }}
      />

      {!isLandscape && (
        <ScrollView
          style={styles.infoScroll}
          contentContainerStyle={styles.infoContent}>
          {/* Camera name + status */}
          <View style={styles.infoRow}>
            <Text style={styles.cameraName}>{camera.name}</Text>
            <View
              style={[
                styles.statusBadge,
                isCapturing ? styles.statusActive : styles.statusInactive,
              ]}>
              <Text style={styles.statusBadgeText}>
                {isCapturing ? 'Live' : 'Stopped'}
              </Text>
            </View>
          </View>

          {resolution && (
            <Text style={styles.metaText}>Resolution: {resolution}</Text>
          )}
          {camera.frigate_name && (
            <Text style={styles.metaText}>
              Frigate camera: {camera.frigate_name}
            </Text>
          )}

          {/* Snapshot thumbnail */}
          {snapshotUrl && (
            <View style={styles.snapshotSection}>
              <Text style={styles.sectionLabel}>Latest snapshot</Text>
              <Image
                source={{uri: snapshotUrl}}
                style={styles.snapshotImage}
                resizeMode="cover"
              />
            </View>
          )}

          {/* Actions */}
          <View style={styles.actions}>
            <TouchableOpacity
              style={styles.actionButton}
              onPress={handleShareSnapshot}
              disabled={snapshotLoading}>
              {snapshotLoading ? (
                <ActivityIndicator size="small" color="#FFFFFF" />
              ) : (
                <Text style={styles.actionButtonText}>Share Snapshot</Text>
              )}
            </TouchableOpacity>
          </View>
        </ScrollView>
      )}
    </View>
  );
}

const styles = StyleSheet.create({
  container: {
    flex: 1,
    backgroundColor: '#111827',
  },
  containerLandscape: {
    flexDirection: 'row',
  },
  centered: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    backgroundColor: '#111827',
    padding: 24,
  },
  infoScroll: {
    flex: 1,
  },
  infoContent: {
    padding: 16,
    gap: 12,
  },
  infoRow: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
  },
  cameraName: {
    color: '#F9FAFB',
    fontSize: 20,
    fontWeight: '700',
    flex: 1,
  },
  statusBadge: {
    paddingHorizontal: 10,
    paddingVertical: 4,
    borderRadius: 20,
    marginLeft: 8,
  },
  statusActive: {
    backgroundColor: '#065F46',
  },
  statusInactive: {
    backgroundColor: '#374151',
  },
  statusBadgeText: {
    color: '#FFFFFF',
    fontSize: 12,
    fontWeight: '600',
  },
  metaText: {
    color: '#9CA3AF',
    fontSize: 13,
  },
  snapshotSection: {
    gap: 8,
  },
  sectionLabel: {
    color: '#6B7280',
    fontSize: 12,
    fontWeight: '500',
    textTransform: 'uppercase',
    letterSpacing: 0.5,
  },
  snapshotImage: {
    width: '100%',
    height: 180,
    borderRadius: 8,
    backgroundColor: '#1F2937',
  },
  actions: {
    marginTop: 8,
    gap: 10,
  },
  actionButton: {
    backgroundColor: '#2563EB',
    paddingVertical: 12,
    borderRadius: 8,
    alignItems: 'center',
  },
  actionButtonText: {
    color: '#FFFFFF',
    fontWeight: '600',
    fontSize: 15,
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
});
