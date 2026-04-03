/**
 * GuestInviteScreen — generate and manage guest invite links.
 *
 * - POST /api/v1/guests/invite → invite_url + QR code
 * - GET /api/v1/guests → list active invites
 * - DELETE revoke
 */

import React, {useCallback, useEffect, useState} from 'react';
import {
  ActivityIndicator,
  Alert,
  FlatList,
  RefreshControl,
  ScrollView,
  Share,
  StyleSheet,
  Switch,
  Text,
  TextInput,
  TouchableOpacity,
  View,
} from 'react-native';
import {ozmaClient} from '../api/client';
import {useStore} from '../store/useStore';
import {GuestInvite, OzmaApiError} from '../api/types';

type Screen = 'list' | 'create';

export function GuestInviteScreen() {
  const [screen, setScreen] = useState<Screen>('list');
  const [invites, setInvites] = useState<GuestInvite[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const cameras = useStore((s) => s.cameras);

  const fetchInvites = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const res = await ozmaClient.listGuests();
      setInvites(res.invites.filter((i) => !i.revoked));
    } catch (err) {
      const message =
        err instanceof OzmaApiError
          ? err.detail
          : err instanceof Error
          ? err.message
          : 'Failed to load invites';
      setError(message);
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    fetchInvites().catch(() => undefined);
  }, [fetchInvites]);

  const handleRevoke = useCallback(
    (invite: GuestInvite) => {
      Alert.alert(
        'Revoke Invite',
        `Revoke invite "${invite.label ?? invite.id}"? The link will stop working immediately.`,
        [
          {text: 'Cancel', style: 'cancel'},
          {
            text: 'Revoke',
            style: 'destructive',
            onPress: async () => {
              try {
                await ozmaClient.revokeGuestInvite(invite.id);
                setInvites((prev) => prev.filter((i) => i.id !== invite.id));
              } catch {
                Alert.alert('Error', 'Failed to revoke invite.');
              }
            },
          },
        ],
      );
    },
    [],
  );

  const handleShare = useCallback(async (invite: GuestInvite) => {
    try {
      await Share.share({
        title: 'Ozma Camera Access',
        message: `View cameras via Ozma: ${invite.invite_url}`,
        url: invite.invite_url,
      });
    } catch {
      // User cancelled
    }
  }, []);

  const handleInviteCreated = useCallback(
    (invite: GuestInvite) => {
      setInvites((prev) => [invite, ...prev]);
      setScreen('list');
    },
    [],
  );

  if (screen === 'create') {
    return (
      <CreateInviteForm
        cameras={cameras.map((c) => ({id: c.id, name: c.name}))}
        onCreated={handleInviteCreated}
        onCancel={() => setScreen('list')}
      />
    );
  }

  return (
    <View style={styles.container}>
      <View style={styles.header}>
        <Text style={styles.headerTitle}>Guest Access</Text>
        <TouchableOpacity
          style={styles.createButton}
          onPress={() => setScreen('create')}>
          <Text style={styles.createButtonText}>+ Invite</Text>
        </TouchableOpacity>
      </View>

      {loading && invites.length === 0 ? (
        <View style={styles.centered}>
          <ActivityIndicator size="large" color="#2563EB" />
        </View>
      ) : error && invites.length === 0 ? (
        <View style={styles.centered}>
          <Text style={styles.errorText}>{error}</Text>
          <TouchableOpacity style={styles.retryButton} onPress={fetchInvites}>
            <Text style={styles.retryButtonText}>Retry</Text>
          </TouchableOpacity>
        </View>
      ) : invites.length === 0 ? (
        <View style={styles.centered}>
          <Text style={styles.emptyTitle}>No active invites</Text>
          <Text style={styles.emptySubtitle}>
            Create an invite link to give someone camera-only access.
          </Text>
        </View>
      ) : (
        <FlatList
          data={invites}
          keyExtractor={(item) => item.id}
          contentContainerStyle={styles.list}
          refreshControl={
            <RefreshControl
              refreshing={loading}
              onRefresh={fetchInvites}
              tintColor="#2563EB"
            />
          }
          renderItem={({item}) => (
            <InviteCard
              invite={item}
              onRevoke={() => handleRevoke(item)}
              onShare={() => handleShare(item)}
            />
          )}
        />
      )}
    </View>
  );
}

// ── InviteCard ────────────────────────────────────────────────────────────────

function InviteCard({
  invite,
  onRevoke,
  onShare,
}: {
  invite: GuestInvite;
  onRevoke: () => void;
  onShare: () => void;
}) {
  const isExpired = new Date(invite.expires_at) < new Date();
  const expiresLabel = isExpired
    ? 'Expired'
    : `Expires ${formatRelativeTime(invite.expires_at)}`;

  return (
    <View style={[styles.card, isExpired && styles.cardExpired]}>
      <View style={styles.cardRow}>
        <Text style={styles.cardLabel} numberOfLines={1}>
          {invite.label ?? invite.id}
        </Text>
        <Text style={[styles.cardExpiry, isExpired && styles.cardExpiryExpired]}>
          {expiresLabel}
        </Text>
      </View>

      {invite.accepted_by_email && (
        <Text style={styles.cardAccepted}>
          Accepted by {invite.accepted_by_email}
        </Text>
      )}

      {invite.camera_ids.length > 0 && (
        <Text style={styles.cardMeta}>
          {invite.camera_ids.length} camera{invite.camera_ids.length !== 1 ? 's' : ''}
        </Text>
      )}

      <Text style={styles.cardUrl} numberOfLines={1}>
        {invite.invite_url}
      </Text>

      <View style={styles.cardActions}>
        <TouchableOpacity style={styles.shareButton} onPress={onShare}>
          <Text style={styles.shareButtonText}>Share Link</Text>
        </TouchableOpacity>
        <TouchableOpacity style={styles.revokeButton} onPress={onRevoke}>
          <Text style={styles.revokeButtonText}>Revoke</Text>
        </TouchableOpacity>
      </View>
    </View>
  );
}

// ── CreateInviteForm ──────────────────────────────────────────────────────────

interface CameraOption {
  id: string;
  name: string;
}

function CreateInviteForm({
  cameras,
  onCreated,
  onCancel,
}: {
  cameras: CameraOption[];
  onCreated: (invite: GuestInvite) => void;
  onCancel: () => void;
}) {
  const [label, setLabel] = useState('');
  const [ttlDays, setTtlDays] = useState('7');
  const [selectedCameras, setSelectedCameras] = useState<Set<string>>(new Set());
  const [allCameras, setAllCameras] = useState(true);
  const [creating, setCreating] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const toggleCamera = useCallback((id: string) => {
    setSelectedCameras((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }, []);

  const handleCreate = useCallback(async () => {
    setCreating(true);
    setError(null);
    try {
      const ttl = parseInt(ttlDays, 10);
      const res = await ozmaClient.createGuestInvite({
        label: label.trim() || undefined,
        camera_ids: allCameras ? [] : Array.from(selectedCameras),
        ttl: isNaN(ttl) ? 604800 : ttl * 86400,
      });
      onCreated(res.invite);
    } catch (err) {
      const message =
        err instanceof OzmaApiError
          ? err.detail
          : err instanceof Error
          ? err.message
          : 'Failed to create invite';
      setError(message);
    } finally {
      setCreating(false);
    }
  }, [label, ttlDays, allCameras, selectedCameras, onCreated]);

  return (
    <ScrollView
      style={styles.formContainer}
      contentContainerStyle={styles.formContent}
      keyboardShouldPersistTaps="handled">
      <Text style={styles.formTitle}>New Guest Invite</Text>
      <Text style={styles.formSubtitle}>
        The recipient will get camera-only (read) access. They do not need an
        Ozma account.
      </Text>

      <View style={styles.fieldGroup}>
        <Text style={styles.fieldLabel}>Label (optional)</Text>
        <TextInput
          style={styles.textInput}
          value={label}
          onChangeText={setLabel}
          placeholder="e.g. Alice — front door"
          placeholderTextColor="#6B7280"
          autoCapitalize="none"
        />
      </View>

      <View style={styles.fieldGroup}>
        <Text style={styles.fieldLabel}>Expires after (days)</Text>
        <TextInput
          style={styles.textInput}
          value={ttlDays}
          onChangeText={setTtlDays}
          keyboardType="number-pad"
          placeholder="7"
          placeholderTextColor="#6B7280"
        />
      </View>

      <View style={styles.fieldGroup}>
        <View style={styles.toggleRow}>
          <Text style={styles.fieldLabel}>All cameras</Text>
          <Switch
            value={allCameras}
            onValueChange={setAllCameras}
            trackColor={{true: '#2563EB'}}
          />
        </View>
        {!allCameras && cameras.length > 0 && (
          <View style={styles.cameraList}>
            {cameras.map((cam) => (
              <TouchableOpacity
                key={cam.id}
                style={[
                  styles.cameraChip,
                  selectedCameras.has(cam.id) && styles.cameraChipSelected,
                ]}
                onPress={() => toggleCamera(cam.id)}>
                <Text
                  style={[
                    styles.cameraChipText,
                    selectedCameras.has(cam.id) && styles.cameraChipTextSelected,
                  ]}>
                  {cam.name}
                </Text>
              </TouchableOpacity>
            ))}
          </View>
        )}
      </View>

      {error && <Text style={styles.formError}>{error}</Text>}

      <View style={styles.formActions}>
        <TouchableOpacity style={styles.cancelButton} onPress={onCancel}>
          <Text style={styles.cancelButtonText}>Cancel</Text>
        </TouchableOpacity>
        <TouchableOpacity
          style={[styles.submitButton, creating && styles.submitButtonLoading]}
          onPress={handleCreate}
          disabled={creating}>
          {creating ? (
            <ActivityIndicator size="small" color="#FFFFFF" />
          ) : (
            <Text style={styles.submitButtonText}>Create Invite</Text>
          )}
        </TouchableOpacity>
      </View>
    </ScrollView>
  );
}

// ── Helpers ───────────────────────────────────────────────────────────────────

function formatRelativeTime(iso: string): string {
  const diff = new Date(iso).getTime() - Date.now();
  if (diff < 0) {
    return 'expired';
  }
  const hours = Math.floor(diff / 3600000);
  if (hours < 24) {
    return `in ${hours}h`;
  }
  return `in ${Math.floor(hours / 24)}d`;
}

// ── Styles ────────────────────────────────────────────────────────────────────

const styles = StyleSheet.create({
  container: {
    flex: 1,
    backgroundColor: '#111827',
  },
  header: {
    flexDirection: 'row',
    justifyContent: 'space-between',
    alignItems: 'center',
    padding: 16,
    borderBottomWidth: 1,
    borderBottomColor: '#374151',
    backgroundColor: '#1F2937',
  },
  headerTitle: {
    color: '#F9FAFB',
    fontSize: 17,
    fontWeight: '600',
  },
  createButton: {
    backgroundColor: '#2563EB',
    paddingHorizontal: 14,
    paddingVertical: 7,
    borderRadius: 8,
  },
  createButtonText: {
    color: '#FFFFFF',
    fontWeight: '600',
    fontSize: 14,
  },
  centered: {
    flex: 1,
    justifyContent: 'center',
    alignItems: 'center',
    padding: 24,
  },
  list: {
    padding: 12,
    gap: 10,
  },
  card: {
    backgroundColor: '#1F2937',
    borderRadius: 10,
    padding: 14,
    gap: 8,
    borderWidth: 1,
    borderColor: '#374151',
  },
  cardExpired: {
    opacity: 0.5,
  },
  cardRow: {
    flexDirection: 'row',
    justifyContent: 'space-between',
    alignItems: 'flex-start',
  },
  cardLabel: {
    color: '#F9FAFB',
    fontSize: 15,
    fontWeight: '600',
    flex: 1,
  },
  cardExpiry: {
    color: '#10B981',
    fontSize: 12,
    marginLeft: 8,
  },
  cardExpiryExpired: {
    color: '#EF4444',
  },
  cardAccepted: {
    color: '#34D399',
    fontSize: 12,
  },
  cardMeta: {
    color: '#9CA3AF',
    fontSize: 12,
  },
  cardUrl: {
    color: '#6B7280',
    fontSize: 11,
    fontFamily: 'monospace',
  },
  cardActions: {
    flexDirection: 'row',
    gap: 8,
    marginTop: 4,
  },
  shareButton: {
    flex: 1,
    backgroundColor: '#1D4ED8',
    paddingVertical: 8,
    borderRadius: 6,
    alignItems: 'center',
  },
  shareButtonText: {
    color: '#FFFFFF',
    fontSize: 13,
    fontWeight: '600',
  },
  revokeButton: {
    flex: 1,
    backgroundColor: '#7F1D1D',
    paddingVertical: 8,
    borderRadius: 6,
    alignItems: 'center',
  },
  revokeButtonText: {
    color: '#FCA5A5',
    fontSize: 13,
    fontWeight: '600',
  },
  formContainer: {
    flex: 1,
    backgroundColor: '#111827',
  },
  formContent: {
    padding: 20,
    gap: 20,
  },
  formTitle: {
    color: '#F9FAFB',
    fontSize: 20,
    fontWeight: '700',
  },
  formSubtitle: {
    color: '#9CA3AF',
    fontSize: 14,
    lineHeight: 20,
  },
  fieldGroup: {
    gap: 8,
  },
  fieldLabel: {
    color: '#D1D5DB',
    fontSize: 14,
    fontWeight: '500',
  },
  textInput: {
    backgroundColor: '#1F2937',
    borderWidth: 1,
    borderColor: '#374151',
    borderRadius: 8,
    paddingHorizontal: 12,
    paddingVertical: 10,
    color: '#F9FAFB',
    fontSize: 15,
  },
  toggleRow: {
    flexDirection: 'row',
    justifyContent: 'space-between',
    alignItems: 'center',
  },
  cameraList: {
    flexDirection: 'row',
    flexWrap: 'wrap',
    gap: 8,
    marginTop: 8,
  },
  cameraChip: {
    borderWidth: 1,
    borderColor: '#374151',
    borderRadius: 20,
    paddingHorizontal: 12,
    paddingVertical: 6,
  },
  cameraChipSelected: {
    backgroundColor: '#1D4ED8',
    borderColor: '#2563EB',
  },
  cameraChipText: {
    color: '#9CA3AF',
    fontSize: 13,
  },
  cameraChipTextSelected: {
    color: '#FFFFFF',
  },
  formError: {
    color: '#F87171',
    fontSize: 14,
    textAlign: 'center',
  },
  formActions: {
    flexDirection: 'row',
    gap: 12,
  },
  cancelButton: {
    flex: 1,
    backgroundColor: '#374151',
    paddingVertical: 12,
    borderRadius: 8,
    alignItems: 'center',
  },
  cancelButtonText: {
    color: '#D1D5DB',
    fontWeight: '600',
  },
  submitButton: {
    flex: 2,
    backgroundColor: '#2563EB',
    paddingVertical: 12,
    borderRadius: 8,
    alignItems: 'center',
  },
  submitButtonLoading: {
    opacity: 0.7,
  },
  submitButtonText: {
    color: '#FFFFFF',
    fontWeight: '600',
    fontSize: 15,
  },
  errorText: {
    color: '#F87171',
    fontSize: 15,
    textAlign: 'center',
    marginBottom: 16,
  },
  retryButton: {
    backgroundColor: '#2563EB',
    paddingHorizontal: 24,
    paddingVertical: 10,
    borderRadius: 8,
  },
  retryButtonText: {
    color: '#FFFFFF',
    fontWeight: '600',
  },
  emptyTitle: {
    color: '#E5E7EB',
    fontSize: 18,
    fontWeight: '600',
    marginBottom: 8,
  },
  emptySubtitle: {
    color: '#6B7280',
    fontSize: 14,
    textAlign: 'center',
  },
});
