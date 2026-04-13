export interface Device {
  id: string;
  name: string;
  ip: string;
  model: string;
  type: string;
  subtype: string;
  status: 'not_configured' | 'configured' | 'unsupported';
  discovery_method: string;
  last_seen: number;
}

export interface DiscoveryEvent {
  type: 'device_found' | 'scan_started' | 'scan_completed';
  device?: Device;
  timestamp: number;
}

export type SetupWizardType = 
  | 'proxmox'
  | 'hue'
  | 'jellyfin'
  | 'home-assistant'
  | 'generic';
