/**
 * RoomCorrectionScreen — multi-step room EQ calibration wizard.
 *
 * Steps:
 *   setup      → pick node, sink, phone model, target curve, room name
 *   measuring  → play sweep on controller + record on phone
 *   processing → FFT + POST to /measure
 *   result     → show EQ curve + bands, apply or save
 *   profiles   → list / manage saved profiles
 *
 * iOS: add NSMicrophoneUsageDescription to Info.plist
 *   <key>NSMicrophoneUsageDescription</key>
 *   <string>Used to measure your room acoustics for EQ correction.</string>
 *
 * Android: RECORD_AUDIO permission is requested at runtime.
 */

import React, {useCallback, useEffect, useRef, useState} from 'react';
import {
  ActivityIndicator,
  Animated,
  Easing,
  FlatList,
  Platform,
  ScrollView,
  StyleSheet,
  Switch,
  Text,
  TextInput,
  TouchableOpacity,
  View,
  Alert,
} from 'react-native';
import {useStore} from '../store/useStore';
import {useRoomCorrection} from '../roomcorrection/useRoomCorrection';
import {EQCurveChart} from '../roomcorrection/EQCurveChart';
import type {CorrectionProfile, EQBand} from '../api/types';

// ── Target curve descriptions ─────────────────────────────────────────────────

const TARGET_CURVE_INFO: Record<string, {label: string; desc: string}> = {
  harman: {
    label: 'Harman',
    desc: 'Warm, bass-forward. Matches how most people prefer music.',
  },
  flat: {
    label: 'Flat',
    desc: 'Neutral reference. Used for mixing and mastering.',
  },
  bbc: {
    label: 'BBC',
    desc: 'Slightly warm, presence dip. Classic broadcast sound.',
  },
};

// ── Auto-detect phone model ───────────────────────────────────────────────────

function autoDetectPhone(): string {
  if (Platform.OS !== 'ios' && Platform.OS !== 'android') return 'generic';
  // Best-effort: on iOS use the model string; on Android use the device model
  // This is a coarse heuristic — user can always override in the picker
  const model = (Platform.constants as any)?.Model as string | undefined;
  if (!model) return 'generic';
  const m = model.toLowerCase();
  if (m.includes('iphone 15') || m.includes('iphone15')) return 'iphone_15';
  if (m.includes('iphone 14') || m.includes('iphone14')) return 'iphone_14';
  if (m.includes('iphone 13') || m.includes('iphone13')) return 'iphone_13';
  if (m.includes('iphone')) return 'iphone_15';
  if (m.includes('pixel 8') || m.includes('pixel8')) return 'pixel_8';
  if (m.includes('pixel 7') || m.includes('pixel7')) return 'pixel_7';
  if (m.includes('sm-s92')) return 'galaxy_s24';
  if (m.includes('sm-s91')) return 'galaxy_s23';
  return 'generic';
}

// ── Waveform animation ────────────────────────────────────────────────────────

function PulsingBar() {
  const anim = useRef(new Animated.Value(0)).current;

  useEffect(() => {
    Animated.loop(
      Animated.sequence([
        Animated.timing(anim, {
          toValue: 1,
          duration: 600,
          easing: Easing.inOut(Easing.ease),
          useNativeDriver: true,
        }),
        Animated.timing(anim, {
          toValue: 0,
          duration: 600,
          easing: Easing.inOut(Easing.ease),
          useNativeDriver: true,
        }),
      ]),
    ).start();
  }, [anim]);

  return (
    <View style={styles.waveformContainer}>
      {Array.from({length: 20}).map((_, i) => {
        const delay = (i / 20) * Math.PI * 2;
        const scaleY = anim.interpolate({
          inputRange: [0, 1],
          outputRange: [0.2 + 0.1 * Math.sin(delay), 0.6 + 0.4 * Math.abs(Math.sin(delay))],
        });
        return (
          <Animated.View
            key={i}
            style={[
              styles.waveformBar,
              {transform: [{scaleY}]},
            ]}
          />
        );
      })}
    </View>
  );
}

// ── EQ band row ───────────────────────────────────────────────────────────────

function EQBandRow({band}: {band: EQBand}) {
  const typeLabel =
    band.type === 'low_shelf'
      ? 'Low shelf'
      : band.type === 'high_shelf'
      ? 'High shelf'
      : 'Peak';
  const gainStr = band.gain >= 0 ? `+${band.gain.toFixed(1)}` : band.gain.toFixed(1);
  return (
    <View style={styles.bandRow}>
      <Text style={styles.bandType}>{typeLabel}</Text>
      <Text style={styles.bandFreq}>
        {band.freq >= 1000
          ? `${(band.freq / 1000).toFixed(1)}kHz`
          : `${Math.round(band.freq)}Hz`}
      </Text>
      <Text style={[styles.bandGain, band.gain >= 0 ? styles.gainPos : styles.gainNeg]}>
        {gainStr} dB
      </Text>
      <Text style={styles.bandQ}>Q {band.q.toFixed(2)}</Text>
    </View>
  );
}

// ── Profile row ───────────────────────────────────────────────────────────────

function ProfileRow({
  profile,
  isActive,
  onApply,
  onDelete,
}: {
  profile: CorrectionProfile;
  isActive: boolean;
  onApply: () => void;
  onDelete: () => void;
}) {
  const [expanded, setExpanded] = useState(false);
  const date = new Date(profile.created_at * 1000).toLocaleDateString();
  const curve = TARGET_CURVE_INFO[profile.target_curve]?.label ?? profile.target_curve;

  return (
    <TouchableOpacity
      style={[styles.profileRow, isActive && styles.profileRowActive]}
      onPress={() => setExpanded(e => !e)}
      onLongPress={() =>
        Alert.alert('Delete profile', `Delete "${profile.name}"?`, [
          {text: 'Cancel', style: 'cancel'},
          {text: 'Delete', style: 'destructive', onPress: onDelete},
        ])
      }>
      <View style={styles.profileRowHeader}>
        <View style={styles.profileRowMeta}>
          <Text style={styles.profileName}>{profile.name || 'Unnamed'}</Text>
          <Text style={styles.profileSub}>
            {date} · {curve} · {profile.band_count ?? profile.bands.length} bands
          </Text>
        </View>
        {isActive && (
          <View style={styles.activeBadge}>
            <Text style={styles.activeBadgeText}>Active</Text>
          </View>
        )}
      </View>
      {expanded && (
        <View style={styles.profileExpanded}>
          {profile.bands.map((b, i) => (
            <EQBandRow key={i} band={b} />
          ))}
          {!isActive && (
            <TouchableOpacity style={styles.applyBtn} onPress={onApply}>
              <Text style={styles.applyBtnText}>Set Active</Text>
            </TouchableOpacity>
          )}
        </View>
      )}
    </TouchableOpacity>
  );
}

// ── Main screen ───────────────────────────────────────────────────────────────

export function RoomCorrectionScreen() {
  const nodes = useStore(s => s.nodes);
  const {
    state,
    fetchSinks,
    fetchStatus,
    fetchProfiles,
    startMeasurement,
    cancelMeasurement,
    applyProfile,
    deleteProfile,
    removeCorrection,
    goToProfiles,
    goToSetup,
    setPhoneModel,
    setTargetCurve,
    setRoomName,
    setSink,
    setContributeToDatabase,
  } = useRoomCorrection();

  // Result step state — must be at top level (Rules of Hooks)
  const [applyLoading, setApplyLoading] = useState(false);
  const [applied, setApplied] = useState(false);

  // Reset apply state when a new profile arrives
  useEffect(() => {
    setApplyLoading(false);
    setApplied(false);
  }, [state.profile?.id]);

  // Initialise
  useEffect(() => {
    fetchStatus();
  }, [fetchStatus]);

  // ── Setup step ──────────────────────────────────────────────────────

  const onNodePress = useCallback(
    (nodeId: string) => {
      fetchSinks(nodeId);
    },
    [fetchSinks],
  );

  const detectedPhone = autoDetectPhone();
  const phoneModelOptions = [
    'auto',
    ...(state.availablePhoneModels.length > 0
      ? state.availablePhoneModels
      : ['iphone_15', 'iphone_14', 'iphone_13', 'pixel_8', 'pixel_7', 'galaxy_s24', 'galaxy_s23', 'generic']),
  ];

  if (state.step === 'setup') {
    return (
      <ScrollView style={styles.container} contentContainerStyle={styles.scrollContent}>
        <Text style={styles.sectionTitle}>Speaker system</Text>

        {/* Node picker */}
        <Text style={styles.label}>Node</Text>
        {nodes.length === 0 ? (
          <Text style={styles.emptyHint}>No nodes discovered</Text>
        ) : (
          nodes.map(node => (
            <TouchableOpacity
              key={node.id}
              style={[
                styles.optionRow,
                state.selectedNodeId === node.id && styles.optionRowSelected,
              ]}
              onPress={() => onNodePress(node.id)}>
              <Text style={styles.optionText}>{node.name}</Text>
              {!node.online && (
                <Text style={styles.offlineBadge}>offline</Text>
              )}
            </TouchableOpacity>
          ))
        )}

        {/* Sink picker */}
        {state.selectedNodeId !== '' && (
          <>
            <Text style={styles.label}>Speaker output</Text>
            {state.availableSinks.length === 0 ? (
              <Text style={styles.emptyHint}>No sinks found on this node</Text>
            ) : (
              state.availableSinks.map(sink => (
                <TouchableOpacity
                  key={sink.name}
                  style={[
                    styles.optionRow,
                    state.selectedSink === sink.name && styles.optionRowSelected,
                  ]}
                  onPress={() => setSink(sink.name)}>
                  <Text style={styles.optionText}>{sink.description || sink.name}</Text>
                </TouchableOpacity>
              ))
            )}
          </>
        )}

        <Text style={styles.sectionTitle}>Calibration settings</Text>

        {/* Phone model picker */}
        <Text style={styles.label}>Phone model</Text>
        <ScrollView horizontal showsHorizontalScrollIndicator={false} style={styles.chipRow}>
          {phoneModelOptions.map(m => {
            const effectiveModel = m === 'auto' ? detectedPhone : m;
            const label = m === 'auto' ? `Auto (${detectedPhone})` : m.replace(/_/g, ' ');
            const selected =
              m === 'auto'
                ? state.phoneModel === detectedPhone
                : state.phoneModel === m;
            return (
              <TouchableOpacity
                key={m}
                style={[styles.chip, selected && styles.chipSelected]}
                onPress={() => setPhoneModel(effectiveModel)}>
                <Text style={[styles.chipText, selected && styles.chipTextSelected]}>
                  {label}
                </Text>
              </TouchableOpacity>
            );
          })}
        </ScrollView>

        {/* Target curve picker */}
        <Text style={styles.label}>Target curve</Text>
        {(['harman', 'flat', 'bbc'] as const).map(curve => {
          const info = TARGET_CURVE_INFO[curve];
          const selected = state.targetCurve === curve;
          return (
            <TouchableOpacity
              key={curve}
              style={[styles.curveOption, selected && styles.curveOptionSelected]}
              onPress={() => setTargetCurve(curve)}>
              <Text style={[styles.curveLabel, selected && styles.curveLabelSelected]}>
                {info.label}
              </Text>
              <Text style={styles.curveDesc}>{info.desc}</Text>
            </TouchableOpacity>
          );
        })}

        {/* Room name */}
        <Text style={styles.label}>Room name (optional)</Text>
        <TextInput
          style={styles.textInput}
          placeholder="e.g. Living Room"
          placeholderTextColor="#6B7280"
          value={state.roomName}
          onChangeText={setRoomName}
        />

        {/* Mic DB consent */}
        <View style={styles.consentRow}>
          <Switch
            value={state.contributeToDatabase}
            onValueChange={setContributeToDatabase}
            trackColor={{false: '#374151', true: '#10B981'}}
            thumbColor={state.contributeToDatabase ? '#fff' : '#9CA3AF'}
          />
          <View style={styles.consentText}>
            <Text style={styles.consentTitle}>
              Contribute to phone mic database
            </Text>
            <Text style={styles.consentDesc}>
              {`Help improve corrections for all ${
                state.phoneModel === 'generic' ? 'users' : state.phoneModel.replace(/_/g, ' ') + ' users'
              }. Sends only your frequency response and phone model — no room info, no audio.`}
            </Text>
          </View>
        </View>

        {state.error && <Text style={styles.errorText}>{state.error}</Text>}

        {/* Start button */}
        <TouchableOpacity
          style={[
            styles.primaryBtn,
            (!state.selectedNodeId || !state.selectedSink) && styles.primaryBtnDisabled,
          ]}
          disabled={!state.selectedNodeId || !state.selectedSink}
          onPress={startMeasurement}>
          <Text style={styles.primaryBtnText}>Start Measurement</Text>
        </TouchableOpacity>

        <TouchableOpacity style={styles.secondaryBtn} onPress={goToProfiles}>
          <Text style={styles.secondaryBtnText}>Saved Profiles</Text>
        </TouchableOpacity>
      </ScrollView>
    );
  }

  // ── Measuring step ──────────────────────────────────────────────────

  if (state.step === 'measuring') {
    return (
      <View style={styles.fullscreenStep}>
        <Text style={styles.measureTitle}>Measuring</Text>
        <Text style={styles.measureInstruction}>
          Place your phone in your primary listening position.{'\n'}Keep it still.
        </Text>

        {state.isRecording ? (
          <>
            <PulsingBar />
            <View style={styles.progressBarContainer}>
              <View
                style={[
                  styles.progressBarFill,
                  {width: `${Math.round(state.recordingProgress * 100)}%`},
                ]}
              />
            </View>
            <Text style={styles.progressLabel}>
              {Math.ceil((1 - state.recordingProgress) * 5.5)}s remaining…
            </Text>
          </>
        ) : (
          <ActivityIndicator color="#10B981" size="large" style={{marginTop: 32}} />
        )}

        <TouchableOpacity style={styles.cancelBtn} onPress={cancelMeasurement}>
          <Text style={styles.cancelBtnText}>Cancel</Text>
        </TouchableOpacity>
      </View>
    );
  }

  // ── Processing step ─────────────────────────────────────────────────

  if (state.step === 'processing') {
    return (
      <View style={styles.fullscreenStep}>
        <ActivityIndicator color="#10B981" size="large" />
        <Text style={styles.processingLabel}>Analysing…</Text>
      </View>
    );
  }

  // ── Result step ─────────────────────────────────────────────────────

  if (state.step === 'result' && state.profile) {
    const {profile} = state;

    const onApplyNow = async () => {
      setApplyLoading(true);
      try {
        await applyProfile(profile.id);
        setApplied(true);
      } catch (e: any) {
        Alert.alert('Apply failed', e.message);
      } finally {
        setApplyLoading(false);
      }
    };

    return (
      <ScrollView style={styles.container} contentContainerStyle={styles.scrollContent}>
        <Text style={styles.profileTitle}>{profile.name || 'Correction Profile'}</Text>

        <EQCurveChart
          corrected={
            profile.bands.length > 0
              ? profile.bands.map(b => [b.freq, b.gain] as [number, number])
              : undefined
          }
          width={320}
          height={200}
        />

        <Text style={styles.sectionTitle}>EQ Bands</Text>
        {profile.bands.map((band, i) => (
          <EQBandRow key={i} band={band} />
        ))}

        {state.error && <Text style={styles.errorText}>{state.error}</Text>}

        {applied ? (
          <View style={styles.appliedBanner}>
            <Text style={styles.appliedBannerText}>Correction applied</Text>
          </View>
        ) : (
          <TouchableOpacity
            style={[styles.primaryBtn, applyLoading && styles.primaryBtnDisabled]}
            disabled={applyLoading}
            onPress={onApplyNow}>
            {applyLoading ? (
              <ActivityIndicator color="#fff" size="small" />
            ) : (
              <Text style={styles.primaryBtnText}>Apply Now</Text>
            )}
          </TouchableOpacity>
        )}

        <TouchableOpacity style={styles.secondaryBtn} onPress={goToSetup}>
          <Text style={styles.secondaryBtnText}>Measure Again</Text>
        </TouchableOpacity>

        <TouchableOpacity style={styles.secondaryBtn} onPress={goToProfiles}>
          <Text style={styles.secondaryBtnText}>View Saved Profiles</Text>
        </TouchableOpacity>
      </ScrollView>
    );
  }

  // ── Profiles step ───────────────────────────────────────────────────

  if (state.step === 'profiles') {
    return (
      <View style={styles.container}>
        <View style={styles.profilesHeader}>
          <TouchableOpacity onPress={goToSetup}>
            <Text style={styles.backLink}>← Back</Text>
          </TouchableOpacity>
          <Text style={styles.profilesTitle}>Saved Profiles</Text>
          {state.activeProfileId ? (
            <TouchableOpacity
              onPress={() =>
                Alert.alert('Remove correction?', 'This disables the active EQ.', [
                  {text: 'Cancel', style: 'cancel'},
                  {text: 'Remove', style: 'destructive', onPress: removeCorrection},
                ])
              }>
              <Text style={styles.removeLink}>Remove</Text>
            </TouchableOpacity>
          ) : (
            <View style={{width: 60}} />
          )}
        </View>
        <FlatList
          data={state.profiles}
          keyExtractor={item => item.id}
          contentContainerStyle={{paddingBottom: 32}}
          ListEmptyComponent={
            <Text style={styles.emptyHint}>No saved profiles yet.</Text>
          }
          renderItem={({item}) => (
            <ProfileRow
              profile={item}
              isActive={item.id === state.activeProfileId}
              onApply={() =>
                applyProfile(item.id).catch(e =>
                  Alert.alert('Apply failed', e.message),
                )
              }
              onDelete={() => deleteProfile(item.id)}
            />
          )}
        />
      </View>
    );
  }

  return null;
}

// ── Styles ────────────────────────────────────────────────────────────────────

const styles = StyleSheet.create({
  container: {
    flex: 1,
    backgroundColor: '#111827',
  },
  scrollContent: {
    padding: 16,
    paddingBottom: 48,
  },
  sectionTitle: {
    color: '#9CA3AF',
    fontSize: 12,
    fontWeight: '600',
    letterSpacing: 0.8,
    textTransform: 'uppercase',
    marginTop: 24,
    marginBottom: 8,
  },
  label: {
    color: '#D1D5DB',
    fontSize: 14,
    fontWeight: '500',
    marginTop: 16,
    marginBottom: 6,
  },
  emptyHint: {
    color: '#6B7280',
    fontSize: 13,
    marginVertical: 8,
  },
  optionRow: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    backgroundColor: '#1F2937',
    borderRadius: 8,
    paddingHorizontal: 14,
    paddingVertical: 12,
    marginBottom: 6,
    borderWidth: 1,
    borderColor: '#374151',
  },
  optionRowSelected: {
    borderColor: '#10B981',
    backgroundColor: '#064E3B',
  },
  optionText: {
    color: '#F9FAFB',
    fontSize: 14,
  },
  offlineBadge: {
    color: '#EF4444',
    fontSize: 11,
  },
  chipRow: {
    flexGrow: 0,
    marginBottom: 4,
  },
  chip: {
    backgroundColor: '#1F2937',
    borderRadius: 16,
    paddingHorizontal: 12,
    paddingVertical: 6,
    marginRight: 8,
    borderWidth: 1,
    borderColor: '#374151',
  },
  chipSelected: {
    borderColor: '#10B981',
    backgroundColor: '#064E3B',
  },
  chipText: {
    color: '#9CA3AF',
    fontSize: 12,
  },
  chipTextSelected: {
    color: '#10B981',
  },
  curveOption: {
    backgroundColor: '#1F2937',
    borderRadius: 8,
    padding: 14,
    marginBottom: 8,
    borderWidth: 1,
    borderColor: '#374151',
  },
  curveOptionSelected: {
    borderColor: '#3B82F6',
    backgroundColor: '#1E3A5F',
  },
  curveLabel: {
    color: '#D1D5DB',
    fontSize: 14,
    fontWeight: '600',
    marginBottom: 2,
  },
  curveLabelSelected: {
    color: '#93C5FD',
  },
  curveDesc: {
    color: '#6B7280',
    fontSize: 12,
  },
  textInput: {
    backgroundColor: '#1F2937',
    borderRadius: 8,
    borderWidth: 1,
    borderColor: '#374151',
    color: '#F9FAFB',
    fontSize: 14,
    paddingHorizontal: 14,
    paddingVertical: 10,
  },
  primaryBtn: {
    backgroundColor: '#10B981',
    borderRadius: 10,
    paddingVertical: 14,
    alignItems: 'center',
    marginTop: 24,
  },
  primaryBtnDisabled: {
    opacity: 0.4,
  },
  primaryBtnText: {
    color: '#fff',
    fontSize: 16,
    fontWeight: '600',
  },
  secondaryBtn: {
    borderRadius: 10,
    paddingVertical: 12,
    alignItems: 'center',
    marginTop: 10,
  },
  secondaryBtnText: {
    color: '#6B7280',
    fontSize: 14,
  },
  errorText: {
    color: '#EF4444',
    fontSize: 13,
    marginTop: 12,
  },
  consentRow: {
    flexDirection: 'row',
    alignItems: 'flex-start',
    marginTop: 20,
    gap: 12,
  },
  consentText: {
    flex: 1,
  },
  consentTitle: {
    color: '#D1D5DB',
    fontSize: 14,
    fontWeight: '500',
    marginBottom: 2,
  },
  consentDesc: {
    color: '#6B7280',
    fontSize: 12,
    lineHeight: 17,
  },
  // Measuring step
  fullscreenStep: {
    flex: 1,
    backgroundColor: '#111827',
    alignItems: 'center',
    justifyContent: 'center',
    padding: 32,
  },
  measureTitle: {
    color: '#F9FAFB',
    fontSize: 24,
    fontWeight: '700',
    marginBottom: 12,
  },
  measureInstruction: {
    color: '#9CA3AF',
    fontSize: 15,
    textAlign: 'center',
    lineHeight: 22,
    marginBottom: 32,
  },
  waveformContainer: {
    flexDirection: 'row',
    alignItems: 'center',
    height: 60,
    gap: 3,
    marginBottom: 24,
  },
  waveformBar: {
    width: 4,
    height: 40,
    backgroundColor: '#10B981',
    borderRadius: 2,
  },
  progressBarContainer: {
    width: 240,
    height: 4,
    backgroundColor: '#374151',
    borderRadius: 2,
    overflow: 'hidden',
    marginBottom: 10,
  },
  progressBarFill: {
    height: '100%',
    backgroundColor: '#10B981',
    borderRadius: 2,
  },
  progressLabel: {
    color: '#6B7280',
    fontSize: 13,
    marginBottom: 32,
  },
  cancelBtn: {
    paddingVertical: 10,
    paddingHorizontal: 24,
  },
  cancelBtnText: {
    color: '#6B7280',
    fontSize: 14,
  },
  // Processing step
  processingLabel: {
    color: '#9CA3AF',
    fontSize: 16,
    marginTop: 20,
  },
  // Result step
  profileTitle: {
    color: '#F9FAFB',
    fontSize: 18,
    fontWeight: '700',
    marginBottom: 16,
  },
  bandRow: {
    flexDirection: 'row',
    alignItems: 'center',
    paddingVertical: 8,
    borderBottomWidth: 1,
    borderBottomColor: '#1F2937',
    gap: 12,
  },
  bandType: {
    color: '#9CA3AF',
    fontSize: 12,
    width: 70,
  },
  bandFreq: {
    color: '#D1D5DB',
    fontSize: 13,
    fontVariant: ['tabular-nums'],
    width: 64,
  },
  bandGain: {
    fontSize: 13,
    fontVariant: ['tabular-nums'],
    width: 60,
  },
  gainPos: {color: '#34D399'},
  gainNeg: {color: '#F87171'},
  bandQ: {
    color: '#6B7280',
    fontSize: 12,
  },
  appliedBanner: {
    backgroundColor: '#064E3B',
    borderRadius: 8,
    padding: 14,
    alignItems: 'center',
    marginTop: 24,
    borderWidth: 1,
    borderColor: '#10B981',
  },
  appliedBannerText: {
    color: '#34D399',
    fontSize: 15,
    fontWeight: '600',
  },
  // Profiles step
  profilesHeader: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    paddingHorizontal: 16,
    paddingVertical: 14,
    borderBottomWidth: 1,
    borderBottomColor: '#374151',
  },
  profilesTitle: {
    color: '#F9FAFB',
    fontSize: 16,
    fontWeight: '600',
  },
  backLink: {
    color: '#3B82F6',
    fontSize: 14,
    width: 60,
  },
  removeLink: {
    color: '#EF4444',
    fontSize: 14,
    textAlign: 'right',
    width: 60,
  },
  profileRow: {
    backgroundColor: '#1F2937',
    marginHorizontal: 12,
    marginTop: 10,
    borderRadius: 10,
    borderWidth: 1,
    borderColor: '#374151',
    overflow: 'hidden',
  },
  profileRowActive: {
    borderColor: '#10B981',
  },
  profileRowHeader: {
    flexDirection: 'row',
    alignItems: 'center',
    padding: 14,
    justifyContent: 'space-between',
  },
  profileRowMeta: {
    flex: 1,
  },
  profileName: {
    color: '#F9FAFB',
    fontSize: 14,
    fontWeight: '600',
  },
  profileSub: {
    color: '#6B7280',
    fontSize: 12,
    marginTop: 2,
  },
  activeBadge: {
    backgroundColor: '#064E3B',
    borderRadius: 12,
    paddingHorizontal: 10,
    paddingVertical: 3,
    borderWidth: 1,
    borderColor: '#10B981',
    marginLeft: 8,
  },
  activeBadgeText: {
    color: '#10B981',
    fontSize: 11,
    fontWeight: '600',
  },
  profileExpanded: {
    paddingHorizontal: 14,
    paddingBottom: 14,
    borderTopWidth: 1,
    borderTopColor: '#374151',
  },
  applyBtn: {
    backgroundColor: '#10B981',
    borderRadius: 8,
    paddingVertical: 10,
    alignItems: 'center',
    marginTop: 12,
  },
  applyBtnText: {
    color: '#fff',
    fontSize: 14,
    fontWeight: '600',
  },
});
