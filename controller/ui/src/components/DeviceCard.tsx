import { Device } from '../types/discovery';

interface DeviceCardProps {
  device: Device;
  onSetup: () => void;
}

export default function DeviceCard(props: DeviceCardProps) {
  const getDeviceIcon = (type: string) => {
    switch (type) {
      case 'router': return '📡';
      case 'switch': return '⇄';
      case 'ap': return '📶';
      case 'nas': return '💾';
      case 'storage': return '🗄️';
      case 'media-server': return '📺';
      case 'streaming': return '🎬';
      case 'smart-home': return '🏠';
      case 'hue': return '💡';
      case 'home-assistant': return 'HomeAs';
      case 'camera': return '📹';
      case 'ip-camera': return '📷';
      case 'vm': return '🖥️';
      case 'proxmox': return '🔧';
      default: return '🔌';
    }
  };

  const getStatusColor = (status: string) => {
    switch (status) {
      case 'configured': return 'bg-green-100 text-green-800';
      case 'not_configured': return 'bg-orange-100 text-orange-800';
      case 'unsupported': return 'bg-gray-100 text-gray-800';
      default: return 'bg-gray-100 text-gray-800';
    }
  };

  return (
    <div class="bg-white rounded-lg shadow border border-gray-200 overflow-hidden">
      <div class="p-4">
        <div class="flex items-start justify-between">
          <div class="flex items-center">
            <div class="text-2xl mr-3">
              {getDeviceIcon(props.device.type)}
            </div>
            <div>
              <h3 class="font-medium text-gray-900">{props.device.name || props.device.ip}</h3>
              <p class="text-sm text-gray-500">{props.device.model}</p>
            </div>
          </div>
          <span class={`inline-flex items-center px-2.5 py-0.5 rounded-full text-xs font-medium ${getStatusColor(props.device.status)}`}>
            {props.device.status.replace('_', ' ')}
          </span>
        </div>
        
        <div class="mt-3 flex items-center text-xs text-gray-500">
          <span class="bg-blue-100 text-blue-800 px-2 py-1 rounded">
            {props.device.discovery_method}
          </span>
          <span class="ml-2">{props.device.ip}</span>
        </div>
        
        {props.device.status === 'not_configured' && (
          <button
            onClick={props.onSetup}
            class="mt-4 w-full bg-blue-600 hover:bg-blue-700 text-white text-sm font-medium py-2 px-4 rounded-lg"
          >
            Set up
          </button>
        )}
      </div>
    </div>
  );
}
