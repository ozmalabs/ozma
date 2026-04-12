import { useCallback, useEffect, useState } from 'react';
import { getToken } from '../auth/tokenStorage';

export interface ControllerSettings {
  node_name: string;
  machine_class: 'workstation' | 'server' | 'kiosk' | 'camera';
  display_width: number;
  display_height: number;
  display_refresh: number;
  theme: 'dark' | 'light' | 'system';
}

const DEFAULTS: ControllerSettings = {
  node_name: '',
  machine_class: 'workstation',
  display_width: 1920,
  display_height: 1080,
  display_refresh: 60,
  theme: 'dark',
};

function authHeaders(): HeadersInit {
  const token = getToken();
  return token
    ? { 'Content-Type': 'application/json', Authorization: `Bearer ${token}` }
    : { 'Content-Type': 'application/json' };
}

export function useSettings() {
  const [settings, setSettings] = useState<ControllerSettings>(DEFAULTS);
  const [loading, setLoading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await fetch('/api/v1/settings', { headers: authHeaders() });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data = await res.json();
      setSettings({ ...DEFAULTS, ...data });
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to load settings');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { load(); }, [load]);

  const save = useCallback(async (patch: Partial<ControllerSettings>) => {
    setSaving(true);
    setError(null);
    setSaved(false);
    try {
      const next = { ...settings, ...patch };
      const res = await fetch('/api/v1/settings', {
        method: 'PATCH',
        headers: authHeaders(),
        body: JSON.stringify(patch),
      });
      if (!res.ok) {
        const body = await res.json().catch(() => ({}));
        throw new Error(body?.detail ?? body?.error ?? `HTTP ${res.status}`);
      }
      setSettings(next);
      setSaved(true);
      setTimeout(() => setSaved(false), 2500);
    } catch (e) {
      setError(e instanceof Error ? e.message : 'Failed to save settings');
    } finally {
      setSaving(false);
    }
  }, [settings]);

  return { settings, setSettings, loading, saving, saved, error, save, reload: load };
}
