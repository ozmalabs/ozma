import {useCallback, useEffect, useState} from 'react';
import {ozmaClient} from '../api/client';
import {useStore} from '../store/useStore';
import {OzmaApiError} from '../api/types';

/**
 * Fetches nodes from the controller and exposes a Wake-on-LAN action.
 */
export function useMachines(): {
  reload: () => Promise<void>;
  sendWoL: (nodeId: string) => Promise<{ok: boolean; message: string}>;
  wolLoading: Record<string, boolean>;
} {
  const setNodes = useStore((s) => s.setNodes);
  const setNodesLoading = useStore((s) => s.setNodesLoading);
  const setNodesError = useStore((s) => s.setNodesError);
  const setActiveNodeId = useStore((s) => s.setActiveNodeId);

  const [wolLoading, setWolLoading] = useState<Record<string, boolean>>({});

  const fetch = useCallback(async () => {
    setNodesLoading(true);
    setNodesError(null);
    try {
      const response = await ozmaClient.listNodes();
      setNodes(response.nodes);
      setActiveNodeId(response.active_node_id);
    } catch (err) {
      const message =
        err instanceof OzmaApiError
          ? err.detail
          : err instanceof Error
          ? err.message
          : 'Failed to load machines';
      setNodesError(message);
    } finally {
      setNodesLoading(false);
    }
  }, [setNodes, setNodesLoading, setNodesError, setActiveNodeId]);

  useEffect(() => {
    fetch().catch(() => undefined);
  }, [fetch]);

  const sendWoL = useCallback(
    async (nodeId: string): Promise<{ok: boolean; message: string}> => {
      setWolLoading((prev) => ({...prev, [nodeId]: true}));
      try {
        const result = await ozmaClient.sendWoL(nodeId);
        return {ok: result.ok, message: result.message};
      } catch (err) {
        const message =
          err instanceof OzmaApiError
            ? err.detail
            : err instanceof Error
            ? err.message
            : 'WoL failed';
        return {ok: false, message};
      } finally {
        setWolLoading((prev) => ({...prev, [nodeId]: false}));
      }
    },
    [],
  );

  return {reload: fetch, sendWoL, wolLoading};
}
