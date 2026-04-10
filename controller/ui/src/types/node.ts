export interface NodeInfo {
  id: string;
  name: string;
  hostname: string;
  ip_address?: string | null;
  port: number;
  status?: 'online' | 'offline' | 'connecting' | 'error' | null;
  machine_class?: 'workstation' | 'server' | 'kiosk' | null;
  active?: boolean;
  last_seen?: string;
  capabilities?: {
    usb_hid?: boolean;
    video_capture?: boolean;
    audio?: boolean;
    rgb_leds?: boolean;
  };
  metadata?: {
    manufacturer?: string;
    model?: string;
    firmware_version?: string;
    os?: string;
  };
}

export interface NodesState {
  nodes: NodeInfo[];
  loading: boolean;
  error: string | null;
  selectedNodeId: string | null;
  WebSocketStatus: 'connected' | 'disconnected' | 'error';
  fetchNodes: () => Promise<void>;
  selectNode: (id: string) => void;
  updateNodeStatus: (id: string, status: NodeInfo['status']) => void;
}
