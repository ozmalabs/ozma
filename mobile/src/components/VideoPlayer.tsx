/**
 * HLS video player component backed by react-native-video.
 *
 * Used in both grid (muted, small) and detail (unmuted, full screen) views.
 */

import React, {useCallback, useRef, useState} from 'react';
import {
  ActivityIndicator,
  StyleSheet,
  Text,
  TouchableOpacity,
  View,
  ViewStyle,
} from 'react-native';
import Video, {OnLoadData, OnProgressData} from 'react-native-video';

interface Props {
  /** HLS stream URL */
  uri: string;
  /** JPEG poster shown while buffering. Can be a URL or local require. */
  posterUri?: string;
  /** When true, audio is silenced. Default: true. */
  muted?: boolean;
  /** Passed to the outer container View */
  style?: ViewStyle;
  /** Called when the video is tapped */
  onPress?: () => void;
  /** Show playback controls overlay (play/pause/seek). Default: false */
  showControls?: boolean;
  /** Called when the stream fails to load */
  onError?: (message: string) => void;
}

/**
 * VideoPlayer renders an HLS stream with:
 * - Loading spinner until first frame
 * - Error state with retry button
 * - Optional tap handler
 * - Muted by default (for grid use)
 */
export function VideoPlayer({
  uri,
  posterUri,
  muted = true,
  style,
  onPress,
  showControls = false,
  onError,
}: Props) {
  const videoRef = useRef<Video>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [retryKey, setRetryKey] = useState(0);

  const handleLoad = useCallback((_data: OnLoadData) => {
    setLoading(false);
    setError(null);
  }, []);

  const handleProgress = useCallback((_data: OnProgressData) => {
    // First progress event means we're actually playing.
    if (loading) {
      setLoading(false);
    }
  }, [loading]);

  const handleError = useCallback(
    (e: {error: {errorString?: string; localizedDescription?: string}}) => {
      const msg =
        e.error.localizedDescription ??
        e.error.errorString ??
        'Stream unavailable';
      setError(msg);
      setLoading(false);
      onError?.(msg);
    },
    [onError],
  );

  const handleRetry = useCallback(() => {
    setError(null);
    setLoading(true);
    setRetryKey((k) => k + 1);
  }, []);

  return (
    <TouchableOpacity
      activeOpacity={onPress ? 0.85 : 1}
      onPress={onPress}
      style={[styles.container, style]}>
      {!error && (
        <Video
          key={retryKey}
          ref={videoRef}
          source={{uri}}
          style={styles.video}
          poster={posterUri}
          posterResizeMode="cover"
          resizeMode="cover"
          muted={muted}
          repeat
          playInBackground={false}
          playWhenInactive={false}
          ignoreSilentSwitch="ignore"
          onLoad={handleLoad}
          onProgress={handleProgress}
          onError={handleError}
          controls={showControls}
          // HLS-specific options
          bufferConfig={{
            minBufferMs: 2000,
            maxBufferMs: 10000,
            bufferForPlaybackMs: 1000,
            bufferForPlaybackAfterRebufferMs: 2000,
          }}
        />
      )}

      {loading && !error && (
        <View style={styles.overlay}>
          <ActivityIndicator size="large" color="#FFFFFF" />
        </View>
      )}

      {error && (
        <View style={styles.overlay}>
          <Text style={styles.errorText}>{error}</Text>
          <TouchableOpacity style={styles.retryButton} onPress={handleRetry}>
            <Text style={styles.retryText}>Retry</Text>
          </TouchableOpacity>
        </View>
      )}
    </TouchableOpacity>
  );
}

const styles = StyleSheet.create({
  container: {
    backgroundColor: '#000000',
    overflow: 'hidden',
  },
  video: {
    ...StyleSheet.absoluteFillObject,
  },
  overlay: {
    ...StyleSheet.absoluteFillObject,
    backgroundColor: 'rgba(0,0,0,0.6)',
    justifyContent: 'center',
    alignItems: 'center',
  },
  errorText: {
    color: '#FFFFFF',
    fontSize: 13,
    textAlign: 'center',
    marginHorizontal: 16,
    marginBottom: 12,
  },
  retryButton: {
    backgroundColor: '#2563EB',
    paddingHorizontal: 20,
    paddingVertical: 8,
    borderRadius: 6,
  },
  retryText: {
    color: '#FFFFFF',
    fontSize: 14,
    fontWeight: '600',
  },
});
