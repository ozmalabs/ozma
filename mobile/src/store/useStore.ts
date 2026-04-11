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

interface NodeDetails {
  id: string;
  name: string;
  host: string;
  port: number;
  machine_class: string;
  last_seen: string | null;
  online: boolean;
  mac_address: string | null;
  camera_streams: CameraStream[];
  frigate_host: string | null;
  frigate_port: number | null;
  direct_registered: boolean;
  agent_connected: boolean;
  ip_address: string | null;
  platform: string | null;
  os_version: string | null;
}

interface CameraStream {
  url: string;
  name: string;
  type: 'hls' | 'mjpeg' | 'rtsp';
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

    setSelectedNodeId: (id) => set((state) => ({nodeStore: {...state.nodeStore, selectedNodeId: id}})),
    setSelectedNode: (node) => set((state) => ({nodeStore: {...state.nodeStore, selectedNode: node}})),
    setSelectedNodeLoading: (loading) => set((state) => ({nodeStore: {...state.nodeStore, selectedNodeLoading: loading}})),
    setSelectedNodeError: (error) => set((state) => ({nodeStore: {...state.nodeStore, selectedNodeError: error}})),
    updateSelectedNode: (partial) => set((state) => ({
      nodeStore: {
        ...state.nodeStore,
        selectedNode: state.nodeStore.selectedNode
          ? {...state.nodeStore.selectedNode, ...partial}
          : null,
      },
    })),
  },
}));
