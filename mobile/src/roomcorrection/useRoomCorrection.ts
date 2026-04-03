/**
 * useRoomCorrection — state and logic for the room correction workflow.
 *
 * Handles the full pipeline:
 *   setup → play sweep on controller → record via phone mic → FFT → POST to
 *   /measure → display result → apply / save.
 *
 * Audio capture uses react-native-audio-recorder-player for WAV recording
 * and react-native-fs to read the PCM bytes for FFT computation.
 */

import {useCallback, useReducer, useRef} from 'react';
import {Platform, PermissionsAndroid} from 'react-native';
import AudioRecorderPlayer from 'react-native-audio-recorder-player';
import RNFS from 'react-native-fs';
import {MMKV} from 'react-native-mmkv';
import {useAuth} from '../auth/useAuth';
import type {CorrectionProfile, AudioSink} from '../api/types';

const _storage = new MMKV();
const MIC_DB_CONSENT_KEY = 'mic_db_consent';

// ── USB mic autocomplete result type ─────────────────────────────────────────

export interface UsbMicResult {
  name: string;
  key: string;
  has_curve: boolean;
  community?: boolean;
}

// ── 1/3-octave centre frequencies (20 Hz – 20 kHz, 20 bands) ─────────────────
const OCTAVE_CENTRES: number[] = [
  20, 31.5, 63, 125, 250, 500, 1000, 2000, 4000, 6000, 8000, 10000, 12000,
  16000, 20000,
];

// ── State ─────────────────────────────────────────────────────────────────────

export type WizardStep =
  | 'setup'
  | 'measuring'
  | 'processing'
  | 'result'
  | 'profiles';

export interface RoomCorrectionState {
  step: WizardStep;
  // Setup
  selectedNodeId: string;
  selectedSink: string;
  phoneModel: string;
  targetCurve: 'harman' | 'flat' | 'bbc';
  roomName: string;
  availableSinks: AudioSink[];
  availablePhoneModels: string[];
  // Mic source
  micSource: 'phone' | 'usb';
  usbMicQuery: string;        // current text in the autocomplete field
  usbMicResults: UsbMicResult[];
  usbMicName: string;         // selected USB mic display name
  usbMicKey: string;          // normalised key (e.g. "blue_yeti")
  // Mic DB consent
  contributeToDatabase: boolean;
  // Measuring
  isRecording: boolean;
  recordingProgress: number; // 0–1
  // Processing
  isProcessing: boolean;
  // Result
  profile: CorrectionProfile | null;
  // Profiles tab
  profiles: CorrectionProfile[];
  activeProfileId: string;
  // Errors
  error: string | null;
}

type Action =
  | {type: 'SET_STEP'; step: WizardStep}
  | {type: 'SET_NODE'; nodeId: string}
  | {type: 'SET_SINK'; sink: string}
  | {type: 'SET_PHONE_MODEL'; model: string}
  | {type: 'SET_TARGET_CURVE'; curve: 'harman' | 'flat' | 'bbc'}
  | {type: 'SET_ROOM_NAME'; name: string}
  | {type: 'SET_SINKS'; sinks: AudioSink[]}
  | {type: 'SET_PHONE_MODELS'; models: string[]}
  | {type: 'SET_MIC_SOURCE'; src: 'phone' | 'usb'}
  | {type: 'SET_USB_MIC_QUERY'; query: string}
  | {type: 'SET_USB_MIC_RESULTS'; results: UsbMicResult[]}
  | {type: 'SET_USB_MIC'; name: string; key: string}
  | {type: 'SET_CONTRIBUTE'; contribute: boolean}
  | {type: 'SET_RECORDING'; isRecording: boolean}
  | {type: 'SET_PROGRESS'; progress: number}
  | {type: 'SET_PROCESSING'; isProcessing: boolean}
  | {type: 'SET_PROFILE'; profile: CorrectionProfile | null}
  | {type: 'SET_PROFILES'; profiles: CorrectionProfile[]}
  | {type: 'SET_ACTIVE_PROFILE_ID'; id: string}
  | {type: 'SET_ERROR'; error: string | null}
  | {type: 'RESET_TO_SETUP'};

const INITIAL_STATE: RoomCorrectionState = {
  step: 'setup',
  selectedNodeId: '',
  selectedSink: '',
  phoneModel: 'generic',
  targetCurve: 'harman',
  roomName: '',
  availableSinks: [],
  availablePhoneModels: [],
  micSource: 'phone',
  usbMicQuery: '',
  usbMicResults: [],
  usbMicName: '',
  usbMicKey: '',
  // Default to true if previously consented, false on first use
  contributeToDatabase: _storage.getBoolean(MIC_DB_CONSENT_KEY) ?? false,
  isRecording: false,
  recordingProgress: 0,
  isProcessing: false,
  profile: null,
  profiles: [],
  activeProfileId: '',
  error: null,
};

function reducer(
  state: RoomCorrectionState,
  action: Action,
): RoomCorrectionState {
  switch (action.type) {
    case 'SET_STEP':
      return {...state, step: action.step, error: null};
    case 'SET_NODE':
      return {...state, selectedNodeId: action.nodeId, selectedSink: '', availableSinks: []};
    case 'SET_SINK':
      return {...state, selectedSink: action.sink};
    case 'SET_PHONE_MODEL':
      return {...state, phoneModel: action.model};
    case 'SET_TARGET_CURVE':
      return {...state, targetCurve: action.curve};
    case 'SET_ROOM_NAME':
      return {...state, roomName: action.name};
    case 'SET_SINKS':
      return {...state, availableSinks: action.sinks};
    case 'SET_PHONE_MODELS':
      return {...state, availablePhoneModels: action.models};
    case 'SET_MIC_SOURCE':
      // Reset USB selection when switching back to phone
      return {
        ...state,
        micSource: action.src,
        usbMicQuery: '',
        usbMicResults: [],
        ...(action.src === 'phone' ? {usbMicName: '', usbMicKey: ''} : {}),
      };
    case 'SET_USB_MIC_QUERY':
      return {...state, usbMicQuery: action.query};
    case 'SET_USB_MIC_RESULTS':
      return {...state, usbMicResults: action.results};
    case 'SET_USB_MIC':
      return {
        ...state,
        usbMicName: action.name,
        usbMicKey: action.key,
        usbMicQuery: action.name,
        usbMicResults: [],
      };
    case 'SET_CONTRIBUTE':
      return {...state, contributeToDatabase: action.contribute};
    case 'SET_RECORDING':
      return {...state, isRecording: action.isRecording};
    case 'SET_PROGRESS':
      return {...state, recordingProgress: action.progress};
    case 'SET_PROCESSING':
      return {...state, isProcessing: action.isProcessing};
    case 'SET_PROFILE':
      return {...state, profile: action.profile};
    case 'SET_PROFILES':
      return {...state, profiles: action.profiles};
    case 'SET_ACTIVE_PROFILE_ID':
      return {...state, activeProfileId: action.id};
    case 'SET_ERROR':
      return {...state, error: action.error};
    case 'RESET_TO_SETUP':
      return {
        ...INITIAL_STATE,
        availablePhoneModels: state.availablePhoneModels,
        profiles: state.profiles,
        activeProfileId: state.activeProfileId,
        contributeToDatabase: state.contributeToDatabase,
      };
    default:
      return state;
  }
}

// ── Goertzel DFT for 1/3-octave bands ────────────────────────────────────────

/**
 * Compute power at a single frequency using the Goertzel algorithm.
 * Operates on a window of `windowSize` samples centred at the target frequency.
 */
function goertzelPower(
  samples: Float32Array,
  targetFreq: number,
  sampleRate: number,
): number {
  // Window size: ~6 cycles of the target frequency for reasonable resolution
  const windowSize = Math.min(
    samples.length,
    Math.round((sampleRate / targetFreq) * 6),
  );
  // Use the last windowSize samples (sweep end is loudest for that freq)
  const offset = Math.max(0, samples.length - windowSize);
  const N = windowSize;
  const k = (targetFreq / sampleRate) * N;
  const coeff = 2.0 * Math.cos((2.0 * Math.PI * k) / N);

  let s0 = 0;
  let s1 = 0;
  let s2 = 0;
  for (let i = 0; i < N; i++) {
    s2 = s1;
    s1 = s0;
    s0 = coeff * s1 - s2 + (samples[offset + i] ?? 0);
  }
  // Power = s0² + s1² - coeff*s0*s1
  return s0 * s0 + s1 * s1 - coeff * s0 * s1;
}

/**
 * Compute a 1/3-octave frequency response from raw PCM samples.
 * Returns [[freq_hz, db], ...] for each OCTAVE_CENTRES frequency.
 */
export function computeFFT(
  samples: Float32Array,
  sampleRate: number,
): [number, number][] {
  const result: [number, number][] = [];
  for (const freq of OCTAVE_CENTRES) {
    if (freq > sampleRate / 2) {
      // Above Nyquist — skip
      continue;
    }
    const power = goertzelPower(samples, freq, sampleRate);
    // Convert to dBFS, clamp to sensible range
    const db =
      power > 0
        ? Math.max(-80, 20 * Math.log10(Math.sqrt(power / samples.length)))
        : -80;
    result.push([freq, db]);
  }
  return result;
}

// ── SNR estimate ──────────────────────────────────────────────────────────────

/**
 * Compute a simple SNR estimate (dB) from a recording.
 *
 * signal = mean RMS of the full recording (in dBFS)
 * noise  = mean RMS of the first 0.3 s (before sweep starts)
 * snr    = signal_db - noise_db
 *
 * Higher = better recording quality.
 */
export function estimateSnr(samples: Float32Array, sampleRate: number): number {
  if (samples.length === 0) return 0;

  // Full-signal RMS
  let sumSq = 0;
  for (let i = 0; i < samples.length; i++) {
    sumSq += samples[i] * samples[i];
  }
  const rmsSignal = Math.sqrt(sumSq / samples.length);
  const dbSignal = rmsSignal > 0 ? 20 * Math.log10(rmsSignal) : -120;

  // Noise floor: first 0.3 s
  const noiseSamples = Math.min(Math.round(sampleRate * 0.3), samples.length);
  let noiseSumSq = 0;
  for (let i = 0; i < noiseSamples; i++) {
    noiseSumSq += samples[i] * samples[i];
  }
  const rmsNoise = noiseSamples > 0 ? Math.sqrt(noiseSumSq / noiseSamples) : 0;
  const dbNoise = rmsNoise > 0 ? 20 * Math.log10(rmsNoise) : -120;

  return dbSignal - dbNoise;
}

// ── WAV reader ────────────────────────────────────────────────────────────────

/**
 * Parse a minimal WAV file (16-bit PCM, any sample rate) from a base64 string.
 * Returns {samples: Float32Array, sampleRate: number}.
 * Throws if the file is not a valid PCM WAV.
 */
function parseWav(base64: string): {samples: Float32Array; sampleRate: number} {
  // Decode base64 → byte array
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  const view = new DataView(bytes.buffer);

  // RIFF header
  const riff = String.fromCharCode(...bytes.slice(0, 4));
  const wave = String.fromCharCode(...bytes.slice(8, 12));
  if (riff !== 'RIFF' || wave !== 'WAVE') {
    throw new Error('Not a RIFF/WAVE file');
  }

  let offset = 12;
  let fmtOffset = -1;
  let dataOffset = -1;
  let dataSize = 0;

  // Walk chunks
  while (offset + 8 <= bytes.length) {
    const chunkId = String.fromCharCode(...bytes.slice(offset, offset + 4));
    const chunkSize = view.getUint32(offset + 4, true);
    if (chunkId === 'fmt ') {
      fmtOffset = offset + 8;
    } else if (chunkId === 'data') {
      dataOffset = offset + 8;
      dataSize = chunkSize;
      break;
    }
    offset += 8 + chunkSize;
  }

  if (fmtOffset < 0 || dataOffset < 0) {
    throw new Error('Missing fmt or data chunk');
  }

  const audioFormat = view.getUint16(fmtOffset, true); // 1 = PCM
  const sampleRate = view.getUint32(fmtOffset + 4, true);
  const bitsPerSample = view.getUint16(fmtOffset + 14, true);

  if (audioFormat !== 1 || bitsPerSample !== 16) {
    throw new Error(`Unsupported WAV format: audioFormat=${audioFormat} bits=${bitsPerSample}`);
  }

  const numSamples = Math.floor(dataSize / 2);
  const samples = new Float32Array(numSamples);
  for (let i = 0; i < numSamples; i++) {
    const s16 = view.getInt16(dataOffset + i * 2, true);
    samples[i] = s16 / 32768.0;
  }

  return {samples, sampleRate};
}

// ── Helper ────────────────────────────────────────────────────────────────────

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms));
}

// ── Hook ──────────────────────────────────────────────────────────────────────

export function useRoomCorrection() {
  const [state, dispatch] = useReducer(reducer, INITIAL_STATE);
  const {controllerUrl, tokens} = useAuth();
  const token = tokens?.access_token ?? null;
  const recorderRef = useRef<AudioRecorderPlayer | null>(null);
  const recordingPathRef = useRef<string>('');

  // ── Auth header ──────────────────────────────────────────────────────

  const headers = useCallback(
    (): Record<string, string> => ({
      'Content-Type': 'application/json',
      ...(token ? {Authorization: `Bearer ${token}`} : {}),
    }),
    [token],
  );

  const apiBase = controllerUrl?.replace(/\/$/, '') ?? '';

  // ── Fetch available sinks for a node ─────────────────────────────────

  const fetchSinks = useCallback(
    async (nodeId: string) => {
      dispatch({type: 'SET_NODE', nodeId});
      if (!nodeId) return;
      try {
        const resp = await fetch(
          `${apiBase}/api/v1/audio/room-correction/node-audio?node_id=${encodeURIComponent(nodeId)}`,
          {headers: headers()},
        );
        const data = await resp.json();
        const rawNodes: Array<{name: string; description: string}> =
          data.nodes ?? [];
        // Strip verbose prefixes for display
        const sinks: AudioSink[] = rawNodes.map(n => ({
          name: n.name,
          description: n.description
            .replace(/^alsa_output\./, '')
            .replace(/_/g, ' '),
        }));
        dispatch({type: 'SET_SINKS', sinks});
      } catch (e: any) {
        dispatch({type: 'SET_ERROR', error: `Could not load sinks: ${e.message}`});
      }
    },
    [apiBase, headers],
  );

  // ── Fetch status (phone models, target curves, active profile) ────────

  const fetchStatus = useCallback(async () => {
    try {
      const resp = await fetch(
        `${apiBase}/api/v1/audio/room-correction/status`,
        {headers: headers()},
      );
      const data = await resp.json();
      if (data.phone_models) {
        dispatch({type: 'SET_PHONE_MODELS', models: data.phone_models});
      }
      if (data.active_profile_id) {
        dispatch({type: 'SET_ACTIVE_PROFILE_ID', id: data.active_profile_id});
      }
    } catch (_) {
      // Non-fatal — phone models have defaults
    }
  }, [apiBase, headers]);

  // ── Fetch saved profiles ──────────────────────────────────────────────

  const fetchProfiles = useCallback(async () => {
    try {
      const resp = await fetch(
        `${apiBase}/api/v1/audio/room-correction/profiles`,
        {headers: headers()},
      );
      const data = await resp.json();
      dispatch({type: 'SET_PROFILES', profiles: data.profiles ?? []});
    } catch (e: any) {
      dispatch({type: 'SET_ERROR', error: `Could not load profiles: ${e.message}`});
    }
  }, [apiBase, headers]);

  // ── Request microphone permission ────────────────────────────────────

  const requestMicPermission = useCallback(async (): Promise<boolean> => {
    if (Platform.OS === 'android') {
      const granted = await PermissionsAndroid.request(
        PermissionsAndroid.PERMISSIONS.RECORD_AUDIO,
        {
          title: 'Microphone Permission',
          message:
            'Ozma needs to record audio from your microphone to measure the room response.',
          buttonPositive: 'Allow',
          buttonNegative: 'Deny',
        },
      );
      return granted === PermissionsAndroid.RESULTS.GRANTED;
    }
    // iOS: permission is requested implicitly when startRecorder is called.
    // Add NSMicrophoneUsageDescription to Info.plist:
    //   <key>NSMicrophoneUsageDescription</key>
    //   <string>Used to measure your room acoustics for EQ correction.</string>
    return true;
  }, []);

  // ── Start measurement ─────────────────────────────────────────────────

  // ── Submit measurement to controller ─────────────────────────────────
  // Defined before startMeasurement so the closure reference is valid.

  const submitMeasurement = useCallback(
    async (
      freqResponse: [number, number][],
      phoneModel: string,
      targetCurve: string,
      roomName: string,
      nodeId: string,
      micType: 'phone' | 'usb' = 'phone',
      micModel: string = '',
    ) => {
      const resp = await fetch(
        `${apiBase}/api/v1/audio/room-correction/measure`,
        {
          method: 'POST',
          headers: headers(),
          body: JSON.stringify({
            frequency_response: freqResponse,
            phone_model: phoneModel,
            target_curve: targetCurve,
            room_name: roomName,
            node_id: nodeId,
            mic_type: micType,
            mic_model: micModel,
          }),
        },
      );
      const data = await resp.json();
      if (!data.ok) {
        throw new Error(data.error ?? 'measure failed');
      }
      dispatch({type: 'SET_PROFILE', profile: data.profile});
      dispatch({type: 'SET_STEP', step: 'result'});
    },
    [apiBase, headers],
  );

  const startMeasurement = useCallback(async () => {
    const {
      selectedNodeId, selectedSink, targetCurve, phoneModel, roomName,
      micSource, usbMicName, usbMicKey,
    } = state;

    dispatch({type: 'SET_STEP', step: 'measuring'});
    dispatch({type: 'SET_ERROR', error: null});

    // Microphone permission
    const hasPerm = await requestMicPermission();
    if (!hasPerm) {
      dispatch({type: 'SET_ERROR', error: 'Microphone permission denied.'});
      dispatch({type: 'SET_STEP', step: 'setup'});
      return;
    }

    // Tell controller to start playing the sweep (non-blocking)
    try {
      const sweepResp = await fetch(
        `${apiBase}/api/v1/audio/room-correction/play-sweep`,
        {
          method: 'POST',
          headers: headers(),
          body: JSON.stringify({
            node_id: selectedNodeId,
            sink: selectedSink,
            duration: 5,
          }),
        },
      );
      const sweepData = await sweepResp.json();
      if (!sweepData.ok) {
        throw new Error(sweepData.error ?? 'play-sweep failed');
      }
    } catch (e: any) {
      dispatch({
        type: 'SET_ERROR',
        error: `Failed to start sweep: ${e.message}`,
      });
      dispatch({type: 'SET_STEP', step: 'setup'});
      return;
    }

    // Start recording on the phone
    try {
      const recorder = new AudioRecorderPlayer();
      recorderRef.current = recorder;

      const recordingPath =
        Platform.OS === 'android'
          ? `${RNFS.CachesDirectoryPath}/ozma_sweep_recording.wav`
          : 'ozma_sweep_recording.wav';
      recordingPathRef.current = recordingPath;

      await recorder.startRecorder(recordingPath, {
        SampleRate: 48000,
        Channels: 1,
        // Force PCM WAV output for cross-platform WAV parsing
        AudioQuality: 'High' as any,
        OutputFormat: 'DEFAULT' as any, // PCM WAV on iOS; on Android may need WAVE
        AudioEncoder: 'LPCM' as any,
        AudioEncodingBitRate: 768000,
      });

      dispatch({type: 'SET_RECORDING', isRecording: true});

      // Animate progress over 5.5 seconds (sweep + tail)
      const RECORD_MS = 5500;
      const TICK_MS = 100;
      const ticks = RECORD_MS / TICK_MS;
      for (let i = 0; i <= ticks; i++) {
        dispatch({type: 'SET_PROGRESS', progress: i / ticks});
        await sleep(TICK_MS);
      }

      await recorder.stopRecorder();
      recorder.removeRecordBackListener();
      recorderRef.current = null;
      dispatch({type: 'SET_RECORDING', isRecording: false});
    } catch (e: any) {
      dispatch({type: 'SET_ERROR', error: `Recording failed: ${e.message}`});
      dispatch({type: 'SET_STEP', step: 'setup'});
      return;
    }

    // Process the recording
    dispatch({type: 'SET_STEP', step: 'processing'});
    dispatch({type: 'SET_PROCESSING', isProcessing: true});

    try {
      // Read WAV file as base64
      const base64 = await RNFS.readFile(recordingPathRef.current, 'base64');
      const {samples, sampleRate} = parseWav(base64);

      // Compute frequency response and SNR estimate
      const freqResponse = computeFFT(samples, sampleRate);
      const snrEstimate = estimateSnr(samples, sampleRate);

      const effectiveMicType: 'phone' | 'usb' = micSource === 'usb' ? 'usb' : 'phone';
      const effectiveMicModel = micSource === 'usb' ? (usbMicName || 'Generic USB') : '';

      // POST to controller
      await submitMeasurement(
        freqResponse,
        phoneModel,
        targetCurve,
        roomName,
        selectedNodeId,
        effectiveMicType,
        effectiveMicModel,
      );

      // Contribute to mic DB if user consented (fire-and-forget)
      if (state.contributeToDatabase) {
        contributeMeasurement(
          freqResponse,
          [],  // correction_applied filled server-side by the /measure handler
          phoneModel,
          targetCurve,
          snrEstimate,
          effectiveMicType,
          effectiveMicModel,
        );
      }
    } catch (e: any) {
      dispatch({
        type: 'SET_ERROR',
        error: `Analysis failed: ${e.message}`,
      });
      dispatch({type: 'SET_STEP', step: 'setup'});
    } finally {
      dispatch({type: 'SET_PROCESSING', isProcessing: false});
    }
  }, [state, apiBase, headers, requestMicPermission, submitMeasurement, contributeMeasurement]);

  // ── Cancel measurement ────────────────────────────────────────────────

  const cancelMeasurement = useCallback(async () => {
    // Stop any in-progress recording
    if (recorderRef.current) {
      try {
        await recorderRef.current.stopRecorder();
        recorderRef.current.removeRecordBackListener();
      } catch (_) {}
      recorderRef.current = null;
    }
    // Tell controller to stop sweep playback
    try {
      await fetch(`${apiBase}/api/v1/audio/room-correction/stop-sweep`, {
        method: 'POST',
        headers: headers(),
        body: JSON.stringify({node_id: state.selectedNodeId}),
      });
    } catch (_) {}
    dispatch({type: 'RESET_TO_SETUP'});
  }, [apiBase, headers, state.selectedNodeId]);

  // ── Apply profile ─────────────────────────────────────────────────────

  const applyProfile = useCallback(
    async (profileId: string, nodeId?: string) => {
      const resp = await fetch(
        `${apiBase}/api/v1/audio/room-correction/apply`,
        {
          method: 'POST',
          headers: headers(),
          body: JSON.stringify({
            profile_id: profileId,
            node_id: nodeId ?? state.selectedNodeId,
          }),
        },
      );
      const data = await resp.json();
      if (!data.ok) {
        throw new Error(data.error ?? 'apply failed');
      }
      dispatch({type: 'SET_ACTIVE_PROFILE_ID', id: profileId});
    },
    [apiBase, headers, state.selectedNodeId],
  );

  // ── Delete profile ────────────────────────────────────────────────────

  const deleteProfile = useCallback(
    async (profileId: string) => {
      await fetch(
        `${apiBase}/api/v1/audio/room-correction/profiles/${encodeURIComponent(profileId)}`,
        {method: 'DELETE', headers: headers()},
      );
      dispatch({
        type: 'SET_PROFILES',
        profiles: state.profiles.filter(p => p.id !== profileId),
      });
      if (state.activeProfileId === profileId) {
        dispatch({type: 'SET_ACTIVE_PROFILE_ID', id: ''});
      }
    },
    [apiBase, headers, state.profiles, state.activeProfileId],
  );

  // ── Remove active correction ──────────────────────────────────────────

  const removeCorrection = useCallback(async () => {
    await fetch(`${apiBase}/api/v1/audio/room-correction/remove`, {
      method: 'POST',
      headers: headers(),
      body: JSON.stringify({node_id: state.selectedNodeId}),
    });
    dispatch({type: 'SET_ACTIVE_PROFILE_ID', id: ''});
  }, [apiBase, headers, state.selectedNodeId]);

  // ── Navigation helpers ────────────────────────────────────────────────

  const goToProfiles = useCallback(() => {
    fetchProfiles();
    dispatch({type: 'SET_STEP', step: 'profiles'});
  }, [fetchProfiles]);

  const goToSetup = useCallback(() => {
    dispatch({type: 'RESET_TO_SETUP'});
  }, []);

  // ── Setters forwarded from screen ─────────────────────────────────────

  const setPhoneModel = useCallback(
    (model: string) => dispatch({type: 'SET_PHONE_MODEL', model}),
    [],
  );
  const setTargetCurve = useCallback(
    (curve: 'harman' | 'flat' | 'bbc') =>
      dispatch({type: 'SET_TARGET_CURVE', curve}),
    [],
  );
  const setRoomName = useCallback(
    (name: string) => dispatch({type: 'SET_ROOM_NAME', name}),
    [],
  );
  const setSink = useCallback(
    (sink: string) => dispatch({type: 'SET_SINK', sink}),
    [],
  );
  const setContributeToDatabase = useCallback((val: boolean) => {
    _storage.set(MIC_DB_CONSENT_KEY, val);
    dispatch({type: 'SET_CONTRIBUTE', contribute: val});
  }, []);

  const setMicSource = useCallback(
    (src: 'phone' | 'usb') => dispatch({type: 'SET_MIC_SOURCE', src}),
    [],
  );

  // USB mic autocomplete — debounced 200ms fetch
  const _usbSearchTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const searchUsbMics = useCallback(
    (q: string) => {
      dispatch({type: 'SET_USB_MIC_QUERY', query: q});
      if (_usbSearchTimer.current) {
        clearTimeout(_usbSearchTimer.current);
      }
      if (!q.trim()) {
        dispatch({type: 'SET_USB_MIC_RESULTS', results: []});
        return;
      }
      _usbSearchTimer.current = setTimeout(async () => {
        try {
          const resp = await fetch(
            `${apiBase}/api/v1/audio/room-correction/usb-mics?q=${encodeURIComponent(q)}`,
            {headers: headers()},
          );
          const data = await resp.json();
          dispatch({type: 'SET_USB_MIC_RESULTS', results: data.mics ?? []});
        } catch (_) {
          // Non-fatal — autocomplete failures are silent
        }
      }, 200);
    },
    [apiBase, headers],
  );

  const selectUsbMic = useCallback(
    (name: string, key: string) => dispatch({type: 'SET_USB_MIC', name, key}),
    [],
  );

  // ── Contribute to mic DB via controller proxy ─────────────────────────

  const contributeMeasurement = useCallback(
    async (
      freqResponse: [number, number][],
      correctionApplied: [number, number][],
      phoneModel: string,
      targetCurve: string,
      snrEstimate: number,
      micType: 'phone' | 'usb' = 'phone',
      micModel: string = '',
    ) => {
      try {
        await fetch(
          `${apiBase}/api/v1/audio/room-correction/contribute`,
          {
            method: 'POST',
            headers: headers(),
            body: JSON.stringify({
              phone_model: phoneModel,
              mic_type: micType,
              mic_model: micModel,
              raw_response: freqResponse,
              correction_applied: correctionApplied,
              target_curve: targetCurve,
              snr_estimate: snrEstimate,
            }),
          },
        );
      } catch (_) {
        // Fire-and-forget — never surface errors to the user
      }
    },
    [apiBase, headers],
  );

  return {
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
    setMicSource,
    searchUsbMics,
    selectUsbMic,
  };
}
