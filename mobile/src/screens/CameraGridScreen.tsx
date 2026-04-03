/**
 * CameraGridScreen — main camera view.
 *
 * Features:
 * - 1 / 4 / 9 grid layout toggle
 * - Tap a camera to navigate to CameraDetailScreen
 * - Pull to refresh
 * - Empty and error states
 */

import React, {useCallback, useMemo} from 'react';
import {
  ActivityIndicator,
  FlatList,
  RefreshControl,
  StyleSheet,
  Text,
  TouchableOpacity,
  useWindowDimensions,
  View,
} from 'react-native';
import {useNavigation} from '@react-navigation/native';
import {NativeStackNavigationProp} from '@react-navigation/native-stack';
import {useCameras} from '../hooks/useCameras';
import {useStore, GridLayout} from '../store/useStore';
import {VideoPlayer} from '../components/VideoPlayer';
import {ozmaClient} from '../api/client';
import {Camera} from '../api/types';
import {RootStackParamList} from '../navigation/AppNavigator';

type NavProp = NativeStackNavigationProp<RootStackParamList, 'MainTabs'>;

const GRID_OPTIONS: GridLayout[] = [1, 4, 9];
const GRID_ICONS: Record<GridLayout, string> = {1: '⊡', 4: '⊞', 9: '⋮⋮⋮'};

export function CameraGridScreen() {
  const navigation = useNavigation<NavProp>();
  const {width: screenWidth} = useWindowDimensions();

  const cameras = useStore((s) => s.cameras);
  const loading = useStore((s) => s.camerasLoading);
  const error = useStore((s) => s.camerasError);
  const gridLayout = useStore((s) => s.gridLayout);
  const setGridLayout = useStore((s) => s.setGridLayout);

  const {reload} = useCameras();

  const cellSize = useMemo(() => {
    const columns = gridLayout === 1 ? 1 : gridLayout === 4 ? 2 : 3;
    return (screenWidth - (columns - 1) * 2) / columns;
  }, [gridLayout, screenWidth]);

  const visibleCameras = useMemo(
    () => cameras.slice(0, gridLayout),
    [cameras, gridLayout],
  );

  const handleCameraPress = useCallback(
    (camera: Camera) => {
      navigation.navigate('CameraDetail', {cameraId: camera.id, cameraName: camera.name});
    },
    [navigation],
  );

  const renderCamera = useCallback(
    ({item}: {item: Camera}) => {
      const streamUrl = ozmaClient.buildStreamUrl(item);
      const snapshotUrl = ozmaClient.buildSnapshotUrl(item);

      // Skip cameras that aren't actively streaming (no stream path)
      if (!streamUrl) {
        return (
          <View style={[styles.cellWrapper, {width: cellSize, height: cellSize}]}>
            <View style={styles.offlineCell}>
              <Text style={styles.offlineCellText}>{item.name}</Text>
              <Text style={styles.offlineCellSubtext}>Not capturing</Text>
            </View>
          </View>
        );
      }

      return (
        <View style={[styles.cellWrapper, {width: cellSize, height: cellSize}]}>
          <VideoPlayer
            uri={streamUrl}
            posterUri={snapshotUrl}
            muted
            style={{width: cellSize, height: cellSize}}
            onPress={() => handleCameraPress(item)}
          />
          {gridLayout > 1 && (
            <View style={styles.cameraLabel}>
              <Text style={styles.cameraLabelText} numberOfLines={1}>
                {item.name}
              </Text>
            </View>
          )}
        </View>
      );
    },
    [cellSize, gridLayout, handleCameraPress],
  );

  if (loading && cameras.length === 0) {
    return (
      <View style={styles.centered}>
        <ActivityIndicator size="large" color="#2563EB" />
      </View>
    );
  }

  if (error && cameras.length === 0) {
    return (
      <View style={styles.centered}>
        <Text style={styles.errorText}>{error}</Text>
        <TouchableOpacity style={styles.retryButton} onPress={reload}>
          <Text style={styles.retryButtonText}>Retry</Text>
        </TouchableOpacity>
      </View>
    );
  }

  if (cameras.length === 0) {
    return (
      <View style={styles.centered}>
        <Text style={styles.emptyTitle}>No cameras</Text>
        <Text style={styles.emptySubtitle}>
          Add cameras in the controller dashboard or configure Frigate.
        </Text>
      </View>
    );
  }

  return (
    <View style={styles.container}>
      {/* Grid layout toggle */}
      <View style={styles.toolbar}>
        <Text style={styles.toolbarTitle}>
          {cameras.length} {cameras.length === 1 ? 'camera' : 'cameras'}
        </Text>
        <View style={styles.layoutButtons}>
          {GRID_OPTIONS.map((layout) => (
            <TouchableOpacity
              key={layout}
              style={[
                styles.layoutButton,
                gridLayout === layout && styles.layoutButtonActive,
              ]}
              onPress={() => setGridLayout(layout)}>
              <Text
                style={[
                  styles.layoutButtonText,
                  gridLayout === layout && styles.layoutButtonTextActive,
                ]}>
                {GRID_ICONS[layout]}
              </Text>
            </TouchableOpacity>
          ))}
        </View>
      </View>

      <FlatList
        data={visibleCameras}
        renderItem={renderCamera}
        keyExtractor={(item) => item.id}
        numColumns={gridLayout === 1 ? 1 : gridLayout === 4 ? 2 : 3}
        key={`grid-${gridLayout}`} // Force re-render on column change
        ItemSeparatorComponent={() => <View style={styles.separator} />}
        columnWrapperStyle={gridLayout > 1 ? styles.row : undefined}
        contentContainerStyle={styles.list}
        refreshControl={
          <RefreshControl refreshing={loading} onRefresh={reload} tintColor="#2563EB" />
        }
      />
    </View>
  );
}

const styles = StyleSheet.create({
  container: {
    flex: 1,
    backgroundColor: '#111827',
  },
  centered: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    backgroundColor: '#111827',
    padding: 24,
  },
  toolbar: {
    flexDirection: 'row',
    justifyContent: 'space-between',
    alignItems: 'center',
    paddingHorizontal: 16,
    paddingVertical: 10,
    backgroundColor: '#1F2937',
    borderBottomWidth: 1,
    borderBottomColor: '#374151',
  },
  toolbarTitle: {
    color: '#D1D5DB',
    fontSize: 14,
    fontWeight: '500',
  },
  layoutButtons: {
    flexDirection: 'row',
    gap: 4,
  },
  layoutButton: {
    paddingHorizontal: 10,
    paddingVertical: 6,
    borderRadius: 6,
    backgroundColor: '#374151',
  },
  layoutButtonActive: {
    backgroundColor: '#2563EB',
  },
  layoutButtonText: {
    color: '#9CA3AF',
    fontSize: 15,
  },
  layoutButtonTextActive: {
    color: '#FFFFFF',
  },
  list: {
    gap: 2,
  },
  row: {
    gap: 2,
  },
  separator: {
    height: 2,
  },
  cellWrapper: {
    position: 'relative',
    backgroundColor: '#000',
  },
  offlineCell: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    backgroundColor: '#1F2937',
  },
  offlineCellText: {
    color: '#D1D5DB',
    fontSize: 13,
    fontWeight: '600',
  },
  offlineCellSubtext: {
    color: '#6B7280',
    fontSize: 11,
    marginTop: 4,
  },
  cameraLabel: {
    position: 'absolute',
    bottom: 0,
    left: 0,
    right: 0,
    backgroundColor: 'rgba(0,0,0,0.55)',
    paddingHorizontal: 6,
    paddingVertical: 3,
  },
  cameraLabelText: {
    color: '#FFFFFF',
    fontSize: 11,
    fontWeight: '500',
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
