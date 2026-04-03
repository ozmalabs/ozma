/**
 * WebcamScreen — phone as webcam.
 *
 * Layout:
 *   ┌───────────────────────────────────────┐
 *   │           Camera Preview (60%)        │
 *   │  [Res]                   [Flip]        │
 *   │                                       │
 *   │   (red border + LIVE badge when live)  │
 *   └───────────────────────────────────────┘
 *   │  Status bar (LIVE · bitrate · fps)    │
 *   │  Stream target picker                 │
 *   │  [Start / Stop Streaming]             │
 *   │  [+ Add Target]                       │
 *   └───────────────────────────────────────┘
 *
 * Add Target modal has three tabs: Ozma, External WHIP, RTMP Relay.
 */

import React, {useCallback, useEffect, useRef, useState} from 'react';
import {
  ActivityIndicator,
  Alert,
  Animated,
  Linking,
  Modal,
  Pressable,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  TouchableOpacity,
  View,
} from 'react-native';
import {RTCView} from 'react-native-webrtc';
import {useWebcam, Resolution} from '../webcam/useWebcam';
import {
  StreamTarget,
  StreamTargetType,
  loadTargets,
  addTarget as persistTarget,
  removeTarget as deleteTarget,
  buildOzmaTarget,
  buildExternalWhipTarget,
  buildRtmpRelayTarget,
} from '../webcam/StreamTargets';
import {useAuth} from '../auth/useAuth';

// ── Helpers ───────────────────────────────────────────────────────────────────

function formatDuration(seconds: number): string {
  const h = Math.floor(seconds / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  if (h > 0) {
    return `${h}:${String(m).padStart(2, '0')}:${String(s).padStart(2, '0')}`;
  }
  return `${m}:${String(s).padStart(2, '0')}`;
}

function formatBitrate(kbps: number): string {
  if (kbps >= 1000) {
    return `${(kbps / 1000).toFixed(1)} Mbps`;
  }
  return `${kbps} kbps`;
}

// ── Add Target Modal ──────────────────────────────────────────────────────────

type AddTargetTab = 'ozma' | 'whip' | 'rtmp';

interface AddTargetModalProps {
  visible: boolean;
  controllerUrl: string;
  onAdd(target: StreamTarget): void;
  onClose(): void;
}

function AddTargetModal({visible, controllerUrl, onAdd, onClose}: AddTargetModalProps) {
  const [tab, setTab] = useState<AddTargetTab>('ozma');
  const [externalUrl, setExternalUrl] = useState('');
  const [externalName, setExternalName] = useState('');
  const [rtmpUrl, setRtmpUrl] = useState('');
  const [rtmpName, setRtmpName] = useState('');

  function handleAdd() {
    switch (tab) {
      case 'ozma': {
        if (!controllerUrl) {
          Alert.alert('No Controller', 'Configure a controller URL in Settings first.');
          return;
        }
        onAdd(buildOzmaTarget(controllerUrl));
        break;
      }
      case 'whip': {
        if (!externalUrl.startsWith('http')) {
          Alert.alert('Invalid URL', 'Enter a valid https:// WHIP endpoint URL.');
          return;
        }
        onAdd(
          buildExternalWhipTarget(
            `whip-${Date.now()}`,
            externalName || 'External WHIP',
            externalUrl.trim(),
          ),
        );
        break;
      }
      case 'rtmp': {
        if (!rtmpUrl.startsWith('rtmp')) {
          Alert.alert('Invalid URL', 'Enter a valid rtmp:// or rtmps:// URL.');
          return;
        }
        if (!controllerUrl) {
          Alert.alert('No Controller', 'An Ozma controller is required to relay RTMP.');
          return;
        }
        onAdd(
          buildRtmpRelayTarget(
            `rtmp-${Date.now()}`,
            rtmpName || 'RTMP Relay',
            rtmpUrl.trim(),
            controllerUrl,
          ),
        );
        break;
      }
    }
    onClose();
  }

  return (
    <Modal visible={visible} transparent animationType="slide" onRequestClose={onClose}>
      <View style={styles.modalOverlay}>
        <View style={styles.modalSheet}>
          <Text style={styles.modalTitle}>Add Stream Target</Text>

          {/* Tab bar */}
          <View style={styles.tabBar}>
            {(['ozma', 'whip', 'rtmp'] as AddTargetTab[]).map(t => (
              <TouchableOpacity
                key={t}
                style={[styles.tabBtn, tab === t && styles.tabBtnActive]}
                onPress={() => setTab(t)}>
                <Text style={[styles.tabLabel, tab === t && styles.tabLabelActive]}>
                  {t === 'ozma' ? 'Ozma' : t === 'whip' ? 'External WHIP' : 'RTMP Relay'}
                </Text>
              </TouchableOpacity>
            ))}
          </View>

          {/* Tab content */}
          {tab === 'ozma' && (
            <View style={styles.tabContent}>
              <Text style={styles.helpText}>
                Stream to your Ozma controller. The controller re-serves the feed
                as HLS alongside other cameras in the dashboard.
              </Text>
              <Text style={styles.fieldLabel}>Controller URL</Text>
              <Text style={styles.fieldValue}>{controllerUrl || '(not configured)'}</Text>
            </View>
          )}

          {tab === 'whip' && (
            <View style={styles.tabContent}>
              <Text style={styles.helpText}>
                Stream directly to any WHIP-compatible server: OBS 29+,
                Cloudflare Stream, Mux, etc. No controller required.
              </Text>
              <Text style={styles.fieldLabel}>Name</Text>
              <TextInput
                style={styles.input}
                placeholder="My OBS Server"
                placeholderTextColor="#6B7280"
                value={externalName}
                onChangeText={setExternalName}
                autoCapitalize="none"
                autoCorrect={false}
              />
              <Text style={styles.fieldLabel}>WHIP Endpoint URL</Text>
              <TextInput
                style={styles.input}
                placeholder="https://..."
                placeholderTextColor="#6B7280"
                value={externalUrl}
                onChangeText={setExternalUrl}
                autoCapitalize="none"
                autoCorrect={false}
                keyboardType="url"
              />
            </View>
          )}

          {tab === 'rtmp' && (
            <View style={styles.tabContent}>
              <Text style={styles.helpText}>
                Send to an RTMP destination (YouTube Live, Twitch, etc.) via your
                Ozma controller. The controller handles the RTMP — no RTMP library
                needed on this phone.
              </Text>
              <Text style={styles.fieldLabel}>Name</Text>
              <TextInput
                style={styles.input}
                placeholder="YouTube Live"
                placeholderTextColor="#6B7280"
                value={rtmpName}
                onChangeText={setRtmpName}
                autoCapitalize="none"
                autoCorrect={false}
              />
              <Text style={styles.fieldLabel}>RTMP URL</Text>
              <TextInput
                style={styles.input}
                placeholder="rtmp://a.rtmp.youtube.com/live2/XXXX"
                placeholderTextColor="#6B7280"
                value={rtmpUrl}
                onChangeText={setRtmpUrl}
                autoCapitalize="none"
                autoCorrect={false}
                keyboardType="url"
              />
            </View>
          )}

          <View style={styles.modalButtons}>
            <TouchableOpacity style={styles.btnSecondary} onPress={onClose}>
              <Text style={styles.btnSecondaryText}>Cancel</Text>
            </TouchableOpacity>
            <TouchableOpacity style={styles.btnPrimary} onPress={handleAdd}>
              <Text style={styles.btnPrimaryText}>Add</Text>
            </TouchableOpacity>
          </View>
        </View>
      </View>
    </Modal>
  );
}

// ── Resolution badge ──────────────────────────────────────────────────────────

const RESOLUTIONS: Resolution[] = ['480p', '720p', '1080p'];

function ResolutionPicker({
  value,
  onChange,
}: {
  value: Resolution;
  onChange(r: Resolution): void;
}) {
  const [open, setOpen] = useState(false);
  return (
    <View>
      <TouchableOpacity style={styles.overlayBtn} onPress={() => setOpen(o => !o)}>
        <Text style={styles.overlayBtnText}>{value}</Text>
      </TouchableOpacity>
      {open && (
        <View style={styles.resPicker}>
          {RESOLUTIONS.map(r => (
            <TouchableOpacity
              key={r}
              style={styles.resOption}
              onPress={() => { onChange(r); setOpen(false); }}>
              <Text style={[styles.resOptionText, r === value && styles.resOptionActive]}>
                {r}
              </Text>
            </TouchableOpacity>
          ))}
        </View>
      )}
    </View>
  );
}

// ── Target row ────────────────────────────────────────────────────────────────

const TARGET_TYPE_LABELS: Record<StreamTargetType, string> = {
  ozma: 'Ozma',
  whip_external: 'WHIP',
  rtmp_relay: 'RTMP Relay',
};

function TargetRow({
  target,
  selected,
  onSelect,
  onDelete,
}: {
  target: StreamTarget;
  selected: boolean;
  onSelect(): void;
  onDelete(): void;
}) {
  return (
    <TouchableOpacity
      style={[styles.targetRow, selected && styles.targetRowSelected]}
      onPress={onSelect}
      onLongPress={onDelete}>
      <View style={styles.targetInfo}>
        <Text style={styles.targetName}>{target.name}</Text>
        <Text style={styles.targetType}>{TARGET_TYPE_LABELS[target.type]}</Text>
      </View>
      {selected && <Text style={styles.targetCheck}>✓</Text>}
    </TouchableOpacity>
  );
}

// ── Main screen ───────────────────────────────────────────────────────────────

export function WebcamScreen() {
  const {controllerUrl} = useAuth();
  const webcam = useWebcam();

  const [targets, setTargets] = useState<StreamTarget[]>([]);
  const [selectedTargetId, setSelectedTargetId] = useState<string | null>(null);
  const [showAddModal, setShowAddModal] = useState(false);

  // Pulsing red border animation
  const pulse = useRef(new Animated.Value(0)).current;
  useEffect(() => {
    if (webcam.status === 'streaming') {
      Animated.loop(
        Animated.sequence([
          Animated.timing(pulse, {toValue: 1, duration: 800, useNativeDriver: false}),
          Animated.timing(pulse, {toValue: 0, duration: 800, useNativeDriver: false}),
        ]),
      ).start();
    } else {
      pulse.stopAnimation();
      pulse.setValue(0);
    }
  }, [webcam.status, pulse]);

  const borderColor = pulse.interpolate({
    inputRange: [0, 1],
    outputRange: ['transparent', '#EF4444'],
  });

  // Load saved targets on mount; add Ozma default if controller configured
  useEffect(() => {
    let saved = loadTargets();
    if (controllerUrl && !saved.find(t => t.id === 'ozma-default')) {
      saved = [buildOzmaTarget(controllerUrl), ...saved];
    }
    setTargets(saved);
    if (saved.length > 0 && !selectedTargetId) {
      setSelectedTargetId(saved[0].id);
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [controllerUrl]);

  // Start preview on mount
  useEffect(() => {
    void webcam.startPreview();
    return () => {
      webcam.stopPreview();
    };
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const handleStartStop = useCallback(async () => {
    if (webcam.status === 'streaming') {
      await webcam.stopStreaming();
      return;
    }
    const target = targets.find(t => t.id === selectedTargetId);
    if (!target) {
      Alert.alert('No target selected', 'Select a stream target or add one below.');
      return;
    }
    await webcam.startStreaming(target);
  }, [webcam, targets, selectedTargetId]);

  function handleAddTarget(target: StreamTarget) {
    const updated = persistTarget(target);
    setTargets(updated);
    setSelectedTargetId(target.id);
  }

  function handleDeleteTarget(id: string) {
    Alert.alert('Remove target?', 'This will remove the saved stream destination.', [
      {text: 'Cancel', style: 'cancel'},
      {
        text: 'Remove',
        style: 'destructive',
        onPress: () => {
          const updated = deleteTarget(id);
          setTargets(updated);
          if (selectedTargetId === id) {
            setSelectedTargetId(updated[0]?.id ?? null);
          }
        },
      },
    ]);
  }

  const isStreaming = webcam.status === 'streaming';
  const isConnecting = webcam.status === 'connecting';
  const isError = webcam.status === 'error';
  const streamUrl = webcam.localStream ? (webcam.localStream as any).toURL?.() : undefined;

  return (
    <View style={styles.container}>
      {/* Camera preview */}
      <Animated.View style={[styles.previewWrapper, {borderColor}]}>
        {streamUrl ? (
          <RTCView
            streamURL={streamUrl}
            style={styles.preview}
            objectFit="cover"
            mirror={webcam.facingMode === 'user'}
          />
        ) : (
          <View style={styles.noPreview}>
            {webcam.status === 'requesting_permission' ? (
              <ActivityIndicator color="#3B82F6" />
            ) : (
              <Text style={styles.noPreviewText}>
                {isError ? webcam.errorMessage : 'Camera not started'}
              </Text>
            )}
          </View>
        )}

        {/* Overlay controls */}
        <View style={styles.overlayTopLeft}>
          <ResolutionPicker
            value={webcam.resolution}
            onChange={webcam.setResolution}
          />
        </View>
        <View style={styles.overlayTopRight}>
          <TouchableOpacity style={styles.overlayBtn} onPress={webcam.toggleCamera}>
            <Text style={styles.overlayBtnText}>⟲</Text>
          </TouchableOpacity>
        </View>

        {/* LIVE badge */}
        {isStreaming && (
          <View style={styles.liveBadge}>
            <View style={styles.liveDot} />
            <Text style={styles.liveBadgeText}>LIVE</Text>
          </View>
        )}
        {isConnecting && (
          <View style={styles.connectingBadge}>
            <ActivityIndicator size="small" color="#FFFFFF" />
            <Text style={styles.connectingText}> Connecting…</Text>
          </View>
        )}
      </Animated.View>

      {/* Bottom panel */}
      <ScrollView style={styles.bottomPanel} contentContainerStyle={styles.bottomContent}>
        {/* Stats bar */}
        {isStreaming && webcam.stats && (
          <View style={styles.statsBar}>
            <Text style={styles.statsText}>
              {webcam.resolution} · {formatBitrate(webcam.stats.bitrate)} · {webcam.stats.fps} fps · {webcam.stats.packetsLost} lost
            </Text>
            <Text style={styles.durationText}>{formatDuration(webcam.streamDuration)}</Text>
          </View>
        )}
        {isConnecting && !isStreaming && (
          <View style={styles.statsBar}>
            <Text style={styles.statsText}>Establishing WebRTC connection…</Text>
          </View>
        )}
        {isError && (
          <View style={styles.errorBar}>
            <Text style={styles.errorText} numberOfLines={2}>
              {webcam.errorMessage}
            </Text>
            {(webcam.errorMessage ?? '').includes('permission') && (
              <TouchableOpacity onPress={() => Linking.openSettings()}>
                <Text style={styles.settingsLink}>Open Settings</Text>
              </TouchableOpacity>
            )}
          </View>
        )}

        {/* Target picker */}
        <Text style={styles.sectionLabel}>Stream to</Text>
        {targets.length === 0 ? (
          <Text style={styles.noTargetsText}>No targets configured. Tap + Add Target below.</Text>
        ) : (
          targets.map(t => (
            <TargetRow
              key={t.id}
              target={t}
              selected={t.id === selectedTargetId}
              onSelect={() => setSelectedTargetId(t.id)}
              onDelete={() => handleDeleteTarget(t.id)}
            />
          ))
        )}

        {/* Start / Stop button */}
        <TouchableOpacity
          style={[
            styles.primaryBtn,
            isStreaming && styles.stopBtn,
            isConnecting && styles.disabledBtn,
          ]}
          onPress={handleStartStop}
          disabled={isConnecting}>
          {isConnecting ? (
            <ActivityIndicator color="#FFFFFF" />
          ) : (
            <Text style={styles.primaryBtnText}>
              {isStreaming ? 'Stop Streaming' : 'Start Streaming'}
            </Text>
          )}
        </TouchableOpacity>

        {/* Add target */}
        <TouchableOpacity
          style={styles.secondaryBtn}
          onPress={() => setShowAddModal(true)}
          disabled={isStreaming}>
          <Text style={styles.secondaryBtnText}>+ Add Target</Text>
        </TouchableOpacity>
      </ScrollView>

      {/* Add target modal */}
      <AddTargetModal
        visible={showAddModal}
        controllerUrl={controllerUrl ?? ''}
        onAdd={handleAddTarget}
        onClose={() => setShowAddModal(false)}
      />
    </View>
  );
}

// ── Styles ────────────────────────────────────────────────────────────────────

const styles = StyleSheet.create({
  container: {
    flex: 1,
    backgroundColor: '#111827',
  },

  // Preview
  previewWrapper: {
    flex: 6,
    borderWidth: 3,
    borderColor: 'transparent',
    overflow: 'hidden',
    position: 'relative',
    backgroundColor: '#000',
  },
  preview: {
    flex: 1,
  },
  noPreview: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    backgroundColor: '#1F2937',
  },
  noPreviewText: {
    color: '#9CA3AF',
    fontSize: 14,
    textAlign: 'center',
    paddingHorizontal: 24,
  },

  // Overlay buttons
  overlayTopLeft: {
    position: 'absolute',
    top: 12,
    left: 12,
  },
  overlayTopRight: {
    position: 'absolute',
    top: 12,
    right: 12,
  },
  overlayBtn: {
    backgroundColor: 'rgba(0,0,0,0.55)',
    paddingHorizontal: 10,
    paddingVertical: 6,
    borderRadius: 8,
  },
  overlayBtnText: {
    color: '#FFFFFF',
    fontSize: 13,
    fontWeight: '600',
  },

  // Resolution picker
  resPicker: {
    position: 'absolute',
    top: 36,
    left: 0,
    backgroundColor: '#1F2937',
    borderRadius: 8,
    overflow: 'hidden',
    zIndex: 10,
  },
  resOption: {
    paddingHorizontal: 14,
    paddingVertical: 8,
  },
  resOptionText: {
    color: '#9CA3AF',
    fontSize: 13,
  },
  resOptionActive: {
    color: '#3B82F6',
    fontWeight: '600',
  },

  // LIVE badge
  liveBadge: {
    position: 'absolute',
    bottom: 12,
    left: 12,
    flexDirection: 'row',
    alignItems: 'center',
    backgroundColor: 'rgba(239,68,68,0.85)',
    paddingHorizontal: 10,
    paddingVertical: 4,
    borderRadius: 6,
  },
  liveDot: {
    width: 8,
    height: 8,
    borderRadius: 4,
    backgroundColor: '#FFFFFF',
    marginRight: 6,
  },
  liveBadgeText: {
    color: '#FFFFFF',
    fontWeight: '700',
    fontSize: 13,
    letterSpacing: 1,
  },
  connectingBadge: {
    position: 'absolute',
    bottom: 12,
    left: 12,
    flexDirection: 'row',
    alignItems: 'center',
    backgroundColor: 'rgba(59,130,246,0.85)',
    paddingHorizontal: 10,
    paddingVertical: 4,
    borderRadius: 6,
  },
  connectingText: {
    color: '#FFFFFF',
    fontSize: 13,
  },

  // Bottom panel
  bottomPanel: {
    flex: 4,
  },
  bottomContent: {
    padding: 16,
    gap: 10,
  },

  // Stats
  statsBar: {
    backgroundColor: '#1F2937',
    borderRadius: 8,
    paddingHorizontal: 12,
    paddingVertical: 8,
    flexDirection: 'row',
    justifyContent: 'space-between',
    alignItems: 'center',
  },
  statsText: {
    color: '#D1FAE5',
    fontSize: 12,
    fontVariant: ['tabular-nums'],
  },
  durationText: {
    color: '#EF4444',
    fontSize: 12,
    fontVariant: ['tabular-nums'],
    fontWeight: '600',
  },

  // Error
  errorBar: {
    backgroundColor: '#450A0A',
    borderRadius: 8,
    padding: 12,
  },
  errorText: {
    color: '#FCA5A5',
    fontSize: 13,
  },
  settingsLink: {
    color: '#3B82F6',
    fontSize: 13,
    marginTop: 6,
    textDecorationLine: 'underline',
  },

  // Targets
  sectionLabel: {
    color: '#9CA3AF',
    fontSize: 12,
    fontWeight: '600',
    textTransform: 'uppercase',
    letterSpacing: 0.8,
    marginTop: 4,
    marginBottom: 2,
  },
  noTargetsText: {
    color: '#6B7280',
    fontSize: 13,
    marginBottom: 4,
  },
  targetRow: {
    backgroundColor: '#1F2937',
    borderRadius: 8,
    paddingHorizontal: 14,
    paddingVertical: 12,
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
  },
  targetRowSelected: {
    borderWidth: 1.5,
    borderColor: '#3B82F6',
  },
  targetInfo: {
    flex: 1,
  },
  targetName: {
    color: '#F9FAFB',
    fontSize: 14,
    fontWeight: '500',
  },
  targetType: {
    color: '#6B7280',
    fontSize: 12,
    marginTop: 2,
  },
  targetCheck: {
    color: '#3B82F6',
    fontSize: 16,
    fontWeight: '700',
    marginLeft: 8,
  },

  // Buttons
  primaryBtn: {
    backgroundColor: '#3B82F6',
    borderRadius: 12,
    paddingVertical: 15,
    alignItems: 'center',
    marginTop: 4,
  },
  stopBtn: {
    backgroundColor: '#EF4444',
  },
  disabledBtn: {
    opacity: 0.6,
  },
  primaryBtnText: {
    color: '#FFFFFF',
    fontSize: 16,
    fontWeight: '700',
  },
  secondaryBtn: {
    borderWidth: 1,
    borderColor: '#374151',
    borderRadius: 12,
    paddingVertical: 12,
    alignItems: 'center',
  },
  secondaryBtnText: {
    color: '#9CA3AF',
    fontSize: 14,
  },

  // Modal
  modalOverlay: {
    flex: 1,
    backgroundColor: 'rgba(0,0,0,0.6)',
    justifyContent: 'flex-end',
  },
  modalSheet: {
    backgroundColor: '#1F2937',
    borderTopLeftRadius: 20,
    borderTopRightRadius: 20,
    padding: 20,
    paddingBottom: 36,
  },
  modalTitle: {
    color: '#F9FAFB',
    fontSize: 18,
    fontWeight: '700',
    marginBottom: 16,
    textAlign: 'center',
  },
  tabBar: {
    flexDirection: 'row',
    backgroundColor: '#111827',
    borderRadius: 10,
    padding: 4,
    marginBottom: 16,
  },
  tabBtn: {
    flex: 1,
    paddingVertical: 8,
    alignItems: 'center',
    borderRadius: 8,
  },
  tabBtnActive: {
    backgroundColor: '#374151',
  },
  tabLabel: {
    color: '#6B7280',
    fontSize: 12,
    fontWeight: '600',
  },
  tabLabelActive: {
    color: '#F9FAFB',
  },
  tabContent: {
    minHeight: 150,
    gap: 8,
  },
  helpText: {
    color: '#9CA3AF',
    fontSize: 13,
    lineHeight: 18,
    marginBottom: 4,
  },
  fieldLabel: {
    color: '#D1D5DB',
    fontSize: 12,
    fontWeight: '600',
    marginTop: 4,
  },
  fieldValue: {
    color: '#6B7280',
    fontSize: 13,
    fontFamily: 'monospace',
  },
  input: {
    backgroundColor: '#111827',
    color: '#F9FAFB',
    borderRadius: 8,
    paddingHorizontal: 12,
    paddingVertical: 10,
    fontSize: 14,
    borderWidth: 1,
    borderColor: '#374151',
  },
  modalButtons: {
    flexDirection: 'row',
    gap: 12,
    marginTop: 20,
  },
  btnSecondary: {
    flex: 1,
    paddingVertical: 13,
    borderWidth: 1,
    borderColor: '#374151',
    borderRadius: 10,
    alignItems: 'center',
  },
  btnSecondaryText: {
    color: '#9CA3AF',
    fontSize: 15,
  },
  btnPrimary: {
    flex: 1,
    paddingVertical: 13,
    backgroundColor: '#3B82F6',
    borderRadius: 10,
    alignItems: 'center',
  },
  btnPrimaryText: {
    color: '#FFFFFF',
    fontSize: 15,
    fontWeight: '700',
  },
});
