import {useCallback, useEffect} from 'react';
import {ozmaClient} from '../api/client';
import {useStore} from '../store/useStore';
import {OzmaApiError} from '../api/types';

/**
 * Fetches the camera list from the controller and keeps the store updated.
 * Returns reload function for pull-to-refresh.
 */
export function useCameras(): {
  reload: () => Promise<void>;
} {
  const setCameras = useStore((s) => s.setCameras);
  const setCamerasLoading = useStore((s) => s.setCamerasLoading);
  const setCamerasError = useStore((s) => s.setCamerasError);

  const fetch = useCallback(async () => {
    setCamerasLoading(true);
    setCamerasError(null);
    try {
      const response = await ozmaClient.listCameras();
      setCameras(response.cameras);
    } catch (err) {
      const message =
        err instanceof OzmaApiError
          ? err.detail
          : err instanceof Error
          ? err.message
          : 'Failed to load cameras';
      setCamerasError(message);
    } finally {
      setCamerasLoading(false);
    }
  }, [setCameras, setCamerasLoading, setCamerasError]);

  useEffect(() => {
    fetch().catch(() => undefined);
  }, [fetch]);

  return {reload: fetch};
}
