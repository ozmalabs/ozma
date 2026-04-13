import { Device } from '../types/discovery';

interface OnboardingTasksProps {
  devices: Device[];
  onSetup: (device: Device) => void;
}

export default function OnboardingTasks(props: OnboardingTasksProps) {
  const pendingDevices = () => 
    props.devices.filter(d => d.status === 'not_configured');

  return (
    <div class="bg-white rounded-lg shadow border border-gray-200 p-4">
      <h3 class="font-medium text-gray-900 mb-2">Setup Tasks</h3>
      {pendingDevices().length === 0 ? (
        <p class="text-gray-500 text-sm">No pending setup tasks</p>
      ) : (
        <div class="space-y-2">
          {pendingDevices().slice(0, 3).map(device => (
            <div class="flex items-center justify-between">
              <div class="flex items-center">
                <span class="text-sm font-medium text-gray-900">
                  {device.name || device.ip}
                </span>
              </div>
              <button
                onClick={() => props.onSetup(device)}
                class="text-blue-600 hover:text-blue-800 text-sm font-medium"
              >
                Set up
              </button>
            </div>
          ))}
          {pendingDevices().length > 3 && (
            <p class="text-xs text-gray-500">
              +{pendingDevices().length - 3} more tasks
            </p>
          )}
        </div>
      )}
    </div>
  );
}
