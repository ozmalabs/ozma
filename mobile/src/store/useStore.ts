/**
 * Zustand store for global application state.
 *
 * Keeps cameras, machines, notification prefs, and push registration state.
 * Does not persist to disk — fetched fresh on mount. MMKV is used only for
 * auth tokens and settings.
 */

import {create} from 'zustand';
import {Camera, NodeInfo, NotificationRecord} from '../api/types';

export type GridLayout = 1 | 4 | 9;

// ── Export Types for Reuse ────────────────────────────────────────────────────

export interface NodeDetails {
  id: string;
  name: string;
  host: string;
  port: number;
  role: string;
  hw: string;
  fwVersion: string;
  protoVersion: number;
  capabilities: string[];
  machineClass: string;
  lastSeen: string | null;
  displayOutputs: DisplayOutput[];
  vncHost: string | null;
  vncPort: number | null;
  streamPort: number | null;
  streamPath: string | null;
  audioType: string | null;
  audioSink: string | null;
  audioVBANPort: number | null;
  micVBANPort: number | null;
  captureDevice: string | null;
  cameraStreams: CameraStream[];
  frigateHost: string | null;
  frigatePort: number | null;
  ownerUserId: string | null;
  owner: string | null;
  sharedWith: string[];
  sharePermissions: string[];
  parentId: string | null;
  sunshinePort: number | null;
  // Legacy fields for compatibility
  online: boolean;
  mac_address: string | null;
  direct_registered: boolean;
  agent_connected: boolean;
  ip_address: string | null;
  platform: string | null;
  os_version: string | null;
}

export interface DisplayOutput {
  id: string;
  name: string;
  resolution: string;
  connected: boolean;
}

export interface CameraStream {
  url: string;
  name: string;
  type: 'hls' | 'mjpeg' | 'rtsp';
}

export interface Scenario {
  id: string;
  name: string;
  nodeId: string | null;
  color: string;
  transitionIn: TransitionConfig;
  motion: MotionPreset[];
  bluetooth: BluetoothConfig[];
  captureSource: string | null;
  captureSources: string[];
  wallpaper: WallpaperConfig | null;
}

export interface TransitionConfig {
  style: string;
  durationMs: number;
}

export interface MotionPreset {
  deviceId: string;
  axis: string;
  position: number;
}

export interface BluetoothConfig {
  connect: string[];
  disconnect: string[];
}

export interface WallpaperConfig {
  mode: string;
  color?: string;
  image?: string;
  url?: string;
}

interface NotificationPrefs {
  motionAlerts: boolean;
  nodeOfflineAlerts: boolean;
  nodeOnlineAlerts: boolean;
  snapshotInNotification: boolean;
}

interface PushState {
  deviceToken: string | null;
  registrationId: string | null;
  isRegistered: boolean;
}

interface DisplayOutput {
  id: string;
  name: string;
  resolution: string;
  connected: boolean;
}

interface CameraStream {
  url: string;
  name: string;
  type: 'hls' | 'mjpeg' | 'rtsp';
}

interface TransitionConfig {
  style: string;
  durationMs: number;
}

interface MotionPreset {
  deviceId: string;
  axis: string;
  position: number;
}

interface BluetoothConfig {
  connect: string[];
  disconnect: string[];
}

interface WallpaperConfig {
  mode: string;
  color?: string;
  image?: string;
  url?: string;
}

interface Scenario {
  id: string;
  name: string;
  nodeId: string | null;
  color: string;
  transitionIn: TransitionConfig;
  motion: MotionPreset[];
  bluetooth: BluetoothConfig[];
  captureSource: string | null;
  captureSources: string[];
  wallpaper: WallpaperConfig | null;
}

interface NodeDetails {
  id: string;
  name: string;
  host: string;
  port: number;
  role: string;
  hw: string;
  fwVersion: string;
  protoVersion: number;
  capabilities: string[];
  machineClass: string;
  lastSeen: string | null;
  displayOutputs: DisplayOutput[];
  vncHost: string | null;
  vncPort: number | null;
  streamPort: number | null;
  streamPath: string | null;
  audioType: string | null;
  audioSink: string | null;
  audioVBANPort: number | null;
  micVBANPort: number | null;
  captureDevice: string | null;
  cameraStreams: CameraStream[];
  frigateHost: string | null;
  frigatePort: number | null;
  ownerUserId: string | null;
  owner: string | null;
  sharedWith: string[];
  sharePermissions: string[];
  parentId: string | null;
  sunshinePort: number | null;
  // Legacy fields for compatibility
  online: boolean;
  mac_address: string | null;
  direct_registered: boolean;
  agent_connected: boolean;
  ip_address: string | null;
  platform: string | null;
  os_version: string | null;
}

interface NodeStore {
  // ── Selected Node Detail ─────────────────────────────────────────────────
  selectedNodeId: string | null;
  selectedNode: NodeDetails | null;
  selectedNodeLoading: boolean;
  selectedNodeError: string | null;

  setSelectedNodeId(id: string | null): void;
  setSelectedNode(node: NodeDetails | null): void;
  setSelectedNodeLoading(loading: boolean): void;
  setSelectedNodeError(error: string | null): void;
  updateSelectedNode(node: Partial<NodeDetails>): void;
  clearSelectedNode(): void;

  // ── Scenarios ─────────────────────────────────────────────────────────────
  scenarios: Scenario[];
  scenariosLoading: boolean;
  scenariosError: string | null;
  activeScenarioId: string | null;

  setScenarios(scenarios: Scenario[]): void;
  setScenariosLoading(loading: boolean): void;
  setScenariosError(error: string | null): void;
  setActiveScenarioId(id: string | null): void;
  updateScenario(scenario: Partial<Scenario>): void;
}

interface AppStore {
  // ── Cameras ──────────────────────────────────────────────────────────────
  cameras: Camera[];
  camerasLoading: boolean;
  camerasError: string | null;
  selectedCameraId: string | null;
  gridLayout: GridLayout;

  setCameras(cameras: Camera[]): void;
  setCamerasLoading(loading: boolean): void;
  setCamerasError(error: string | null): void;
  setSelectedCamera(id: string | null): void;
  setGridLayout(layout: GridLayout): void;

  // ── Machines ─────────────────────────────────────────────────────────────
  nodes: NodeInfo[];
  nodesLoading: boolean;
  nodesError: string | null;
  activeNodeId: string | null;

  setNodes(nodes: NodeInfo[]): void;
  setNodesLoading(loading: boolean): void;
  setNodesError(error: string | null): void;
  setActiveNodeId(id: string | null): void;
  updateNodeOnlineStatus(nodeId: string, online: boolean): void;

  // ── Notifications ─────────────────────────────────────────────────────────
  notifications: NotificationRecord[];
  notificationsLoading: boolean;
  notificationsError: string | null;
  unreadCount: number;

  setNotifications(records: NotificationRecord[]): void;
  prependNotification(record: NotificationRecord): void;
  markNotificationRead(id: string): void;
  setNotificationsLoading(loading: boolean): void;
  setNotificationsError(error: string | null): void;
  setUnreadCount(count: number): void;

  // ── Notification preferences ──────────────────────────────────────────────
  notificationPrefs: NotificationPrefs;
  setNotificationPrefs(prefs: Partial<NotificationPrefs>): void;

  // ── Push ──────────────────────────────────────────────────────────────────
  push: PushState;
  setPushToken(token: string | null): void;
  setPushRegistration(registrationId: string): void;
  clearPushRegistration(): void;

  // ── Node Store (Zustand) ─────────────────────────────────────────────────
  nodeStore: NodeStore;
}

export const useStore = create<AppStore>((set) => ({
  // ── Cameras ───────────────────────────────────────────────────────────────
  cameras: [],
  camerasLoading: false,
  camerasError: null,
  selectedCameraId: null,
  gridLayout: 1,

  setCameras: (cameras) => set({cameras}),
  setCamerasLoading: (camerasLoading) => set({camerasLoading}),
  setCamerasError: (camerasError) => set({camerasError}),
  setSelectedCamera: (selectedCameraId) => set({selectedCameraId}),
  setGridLayout: (gridLayout) => set({gridLayout}),

  // ── Machines ──────────────────────────────────────────────────────────────
  nodes: [],
  nodesLoading: false,
  nodesError: null,
  activeNodeId: null,

  setNodes: (nodes) => set({nodes}),
  setNodesLoading: (nodesLoading) => set({nodesLoading}),
  setNodesError: (nodesError) => set({nodesError}),
  setActiveNodeId: (activeNodeId) => set({activeNodeId}),
  updateNodeOnlineStatus: (nodeId, online) =>
    set((state) => ({
      nodes: state.nodes.map((n) =>
        n.id === nodeId ? {...n, online} : n,
      ),
    })),

  // ── Notifications ─────────────────────────────────────────────────────────
  notifications: [],
  notificationsLoading: false,
  notificationsError: null,
  unreadCount: 0,

  setNotifications: (notifications) => set({notifications}),
  prependNotification: (record) =>
    set((state) => ({
      notifications: [record, ...state.notifications],
      unreadCount: state.unreadCount + (record.read ? 0 : 1),
    })),
  markNotificationRead: (id) =>
    set((state) => {
      const notifications = state.notifications.map((n) =>
        n.id === id ? {...n, read: true} : n,
      );
      const unreadCount = notifications.filter((n) => !n.read).length;
      return {notifications, unreadCount};
    }),
  setNotificationsLoading: (notificationsLoading) =>
    set({notificationsLoading}),
  setNotificationsError: (notificationsError) => set({notificationsError}),
  setUnreadCount: (unreadCount) => set({unreadCount}),

  // ── Notification preferences ──────────────────────────────────────────────
  notificationPrefs: {
    motionAlerts: true,
    nodeOfflineAlerts: true,
    nodeOnlineAlerts: false,
    snapshotInNotification: true,
  },
  setNotificationPrefs: (prefs) =>
    set((state) => ({
      notificationPrefs: {...state.notificationPrefs, ...prefs},
    })),

  // ── Push ──────────────────────────────────────────────────────────────────
  push: {
    deviceToken: null,
    registrationId: null,
    isRegistered: false,
  },
  setPushToken: (token) =>
    set((state) => ({push: {...state.push, deviceToken: token}})),
  setPushRegistration: (registrationId) =>
    set((state) => ({
      push: {...state.push, registrationId, isRegistered: true},
    })),
  clearPushRegistration: () =>
    set((state) => ({
      push: {...state.push, registrationId: null, isRegistered: false},
    })),

  // ── Node Store (Zustand) ─────────────────────────────────────────────────
  nodeStore: {
    selectedNodeId: null,
    selectedNode: null,
    selectedNodeLoading: false,
    selectedNodeError: null,

    setSelectedNodeId: (id) => set((state) => {
      // Validate ID format
      if (id && typeof id !== 'string') {
        console.warn('NodeStore: Invalid node ID type, expected string');
        return state;
      }
      if (id && state.nodeStore.selectedNodeId === id) return state;
      return {
        nodeStore: {
          ...state.nodeStore,
          selectedNodeId: id,
          selectedNodeError: null,
          selectedNodeLoading: false,
        },
      };
    }),
    setSelectedNode: (node) => set((state) => {
      // Validate node
      if (node && typeof node !== 'object') {
        console.warn('NodeStore: Invalid node data type');
        return state;
      }
      // Ensure node ID matches selected node ID
      if (node?.id !== state.nodeStore.selectedNodeId) return state;
      return {
        nodeStore: {
          ...state.nodeStore,
          selectedNode: node,
          selectedNodeError: null,
        },
      };
    }),
    setSelectedNodeLoading: (loading) => set((state) => ({
      nodeStore: {
        ...state.nodeStore,
        selectedNodeLoading: typeof loading === 'boolean' ? loading : false,
      },
    })),
    setSelectedNodeError: (error) => set((state) => ({
      nodeStore: {
        ...state.nodeStore,
        selectedNodeError: error || null,
      },
    })),
    updateSelectedNode: (partial) => set((state) => {
      if (!partial || typeof partial !== 'object') {
        console.warn('NodeStore: Invalid update data type');
        return state;
      }
      const node = state.nodeStore.selectedNode;
      if (!node) return state;
      return {
        nodeStore: {
          ...state.nodeStore,
          selectedNode: {...node, ...partial},
        },
      };
    }),
    clearSelectedNode: () => set((state) => ({
      nodeStore: {
        ...state.nodeStore,
        selectedNodeId: null,
        selectedNode: null,
        selectedNodeLoading: false,
        selectedNodeError: null,
      },
    })),
  },

  // ── Scenarios ─────────────────────────────────────────────────────────────
  scenarios: [],
  scenariosLoading: false,
  scenariosError: null,
  activeScenarioId: null,

  setScenarios: (scenarios) => set({scenarios}),
  setScenariosLoading: (loading) => set({scenariosLoading}),
  setScenariosError: (error) => set({scenariosError}),
  setActiveScenarioId: (id) => set({activeScenarioId: id}),
  updateScenario: (scenario) =>
    set((state) => ({
      scenarios: state.scenarios.map((s) =>
        s.id === scenario.id ? {...s, ...scenario} : s,
      ),
    })),
}));
