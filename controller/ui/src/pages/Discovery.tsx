import { createEffect, createSignal, onCleanup, For } from 'solid-js';
import { Device, DiscoveryEvent, SetupWizardType } from '../types/discovery';
import DeviceCard from '../components/DeviceCard';
import SetupWizardModal from '../components/SetupWizardModal';
import OnboardingTasks from '../components/OnboardingTasks';

export default function Discovery() {
  const [devices, setDevices] = createSignal<Device[]>([]);
  const [isScanning, setIsScanning] = createSignal(false);
  const [selectedDevice, setSelectedDevice] = createSignal<Device | null>(null);
  const [showWizard, setShowWizard] = createSignal(false);
  const [events, setEvents] = createSignal<DiscoveryEvent[]>([]);

  // Group devices by type
  const groupedDevices = () => {
    const groups: Record<string, Device[]> = {
      'Network Infrastructure': [],
      'NAS & Storage': [],
      'Media': [],
      'Smart Home': [],
      'Cameras': [],
      'Virtualisation': [],
      'Other': []
    };

    devices().forEach(device => {
      switch (device.type) {
        case 'router':
        case 'switch':
        case 'ap':
          groups['Network Infrastructure'].push(device);
          break;
        case 'nas':
        case 'storage':
          groups['NAS & Storage'].push(device);
          break;
        case 'media-server':
        case 'streaming':
          groups['Media'].push(device);
          break;
        case 'smart-home':
        case 'hue':
        case 'home-assistant':
          groups['Smart Home'].push(device);
          break;
        case 'camera':
        case 'ip-camera':
          groups['Cameras'].push(device);
          break;
        case 'vm':
        case 'proxmox':
          groups['Virtualisation'].push(device);
          break;
        default:
          groups['Other'].push(device);
      }
    });

    return groups;
  };

  // Start scanning
  const startScan = async () => {
    setIsScanning(true);
    try {
      await fetch('/api/v1/discover/scan', { method: 'POST' });
    } catch (error) {
      console.error('Failed to start scan:', error);
      setIsScanning(false);
    }
  };

  // Setup SSE for discovery events
  createEffect(() => {
    const eventSource = new EventSource('/api/v1/discover/stream');
    
    eventSource.onmessage = (event) => {
      const data: DiscoveryEvent = JSON.parse(event.data);
      setEvents(prev => [...prev, data]);
      
      if (data.type === 'device_found') {
        setDevices(prev => {
          const exists = prev.some(d => d.id === data.device.id);
          if (!exists) {
            return [...prev, data.device];
          }
          return prev;
        });
      }
    };

    onCleanup(() => {
      eventSource.close();
    });
  });

  // Open setup wizard
  const openWizard = (device: Device) => {
    setSelectedDevice(device);
    setShowWizard(true);
  };

  return (
    <div class="p-6">
      <div class="mb-6">
        <h1 class="text-2xl font-bold text-gray-900">Device Discovery</h1>
        <p class="text-gray-600">Find and configure devices on your network</p>
      </div>

      <div class="mb-6 flex justify-between items-center">
        <button
          onClick={startScan}
          disabled={isScanning()}
          class="bg-blue-600 hover:bg-blue-700 text-white font-medium py-2 px-4 rounded-lg disabled:opacity-50"
        >
          {isScanning() ? 'Scanning...' : 'Scan Network'}
        </button>
        
        <OnboardingTasks devices={devices()} onSetup={openWizard} />
      </div>

      <div class="space-y-8">
        <For each={Object.entries(groupedDevices())}>
          {([groupName, groupDevices]) => (
            groupDevices.length > 0 && (
              <div>
                <h2 class="text-lg font-semibold text-gray-800 mb-4">{groupName}</h2>
                <div class="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
                  <For each={groupDevices}>
                    {device => (
                      <DeviceCard 
                        device={device} 
                        onSetup={() => openWizard(device)} 
                      />
                    )}
                  </For>
                </div>
              </div>
            )
          )}
        </For>
      </div>

      {showWizard() && selectedDevice() && (
        <SetupWizardModal
          device={selectedDevice()!}
          onClose={() => setShowWizard(false)}
          onSuccess={() => {
            // Update device status
            setDevices(prev => prev.map(d => 
              d.id === selectedDevice()!.id 
                ? { ...d, status: 'configured' } 
                : d
            ));
            setShowWizard(false);
          }}
        />
      )}
    </div>
  );
}
