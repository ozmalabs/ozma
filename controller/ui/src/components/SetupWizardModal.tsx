import { createSignal, createEffect } from 'solid-js';
import { Device } from '../types/discovery';

interface SetupWizardModalProps {
  device: Device;
  onClose: () => void;
  onSuccess: () => void;
}

export default function SetupWizardModal(props: SetupWizardModalProps) {
  const [credentials, setCredentials] = createSignal<Record<string, string>>({});
  const [isSubmitting, setIsSubmitting] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);

  const getWizardInstructions = (device: Device) => {
    switch (device.subtype) {
      case 'proxmox':
        return {
          title: 'Proxmox Setup',
          instructions: 'Create API token in Datacenter > Permissions > API Tokens',
          fields: [
            { name: 'username', label: 'Username', type: 'text' },
            { name: 'token_id', label: 'Token ID', type: 'text' },
            { name: 'token_secret', label: 'Token Secret', type: 'password' }
          ]
        };
      case 'hue':
        return {
          title: 'Hue Bridge Setup',
          instructions: 'Press the button on top of your Hue Bridge, then click Pair',
          fields: [
            { name: 'bridge_ip', label: 'Bridge IP Address', type: 'text' }
          ]
        };
      case 'jellyfin':
        return {
          title: 'Jellyfin Setup',
          instructions: 'Go to Dashboard > API Keys > Add API Key',
          fields: [
            { name: 'api_key', label: 'API Key', type: 'password' },
            { name: 'server_url', label: 'Server URL', type: 'text' }
          ]
        };
      case 'home-assistant':
        return {
          title: 'Home Assistant Setup',
          instructions: 'Profile > Long-Lived Access Tokens > Create',
          fields: [
            { name: 'access_token', label: 'Access Token', type: 'password' },
            { name: 'server_url', label: 'Server URL', type: 'text' }
          ]
        };
      default:
        return {
          title: 'Device Setup',
          instructions: 'Enter the required credentials to configure this device',
          fields: [
            { name: 'username', label: 'Username', type: 'text' },
            { name: 'password', label: 'Password', type: 'password' }
          ]
        };
    }
  };

  const wizard = getWizardInstructions(props.device);

  const handleSubmit = async (e: Event) => {
    e.preventDefault();
    setIsSubmitting(true);
    setError(null);

    try {
      const response = await fetch(`/api/v1/discover/${props.device.id}/configure`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json'
        },
        body: JSON.stringify(credentials())
      });

      if (!response.ok) {
        const data = await response.json();
        throw new Error(data.error || 'Failed to configure device');
      }

      props.onSuccess();
    } catch (err) {
      setError(err instanceof Error ? err.message : 'An unknown error occurred');
    } finally {
      setIsSubmitting(false);
    }
  };

  return (
    <div class="fixed inset-0 bg-black bg-opacity-50 flex items-center justify-center z-50">
      <div class="bg-white rounded-lg shadow-xl w-full max-w-md">
        <div class="p-6">
          <div class="flex justify-between items-center mb-4">
            <h2 class="text-xl font-bold text-gray-900">{wizard.title}</h2>
            <button 
              onClick={props.onClose}
              class="text-gray-400 hover:text-gray-500"
            >
              <svg class="h-6 w-6" fill="none" viewBox="0 0 24 24" stroke="currentColor">
                <path stroke-linecap="round" stroke-linejoin="round" stroke-width="2" d="M6 18L18 6M6 6l12 12" />
              </svg>
            </button>
          </div>

          <div class="mb-6">
            <p class="text-gray-600 mb-4">{wizard.instructions}</p>
            
            {error() && (
              <div class="mb-4 p-3 bg-red-50 text-red-700 rounded">
                {error()}
              </div>
            )}

            <form onSubmit={handleSubmit}>
              {wizard.fields.map(field => (
                <div class="mb-4">
                  <label class="block text-sm font-medium text-gray-700 mb-1">
                    {field.label}
                  </label>
                  <input
                    type={field.type}
                    class="w-full px-3 py-2 border border-gray-300 rounded-md shadow-sm focus:outline-none focus:ring-blue-500 focus:border-blue-500"
                    value={credentials()[field.name] || ''}
                    onInput={(e) => setCredentials(prev => ({
                      ...prev,
                      [field.name]: (e.target as HTMLInputElement).value
                    }))}
                  />
                </div>
              ))}

              <div class="flex justify-end space-x-3 mt-6">
                <button
                  type="button"
                  onClick={props.onClose}
                  class="px-4 py-2 border border-gray-300 rounded-md text-sm font-medium text-gray-700 hover:bg-gray-50"
                >
                  Cancel
                </button>
                <button
                  type="submit"
                  disabled={isSubmitting()}
                  class="px-4 py-2 bg-blue-600 text-white rounded-md text-sm font-medium hover:bg-blue-700 disabled:opacity-50"
                >
                  {isSubmitting() ? 'Configuring...' : 'Configure'}
                </button>
              </div>
            </form>
          </div>
        </div>
      </div>
    </div>
  );
}
