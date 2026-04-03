/**
 * SettingsScreen — controller URL, account info, push test, logout.
 */

import React, {useCallback, useState} from 'react';
import {
  ActivityIndicator,
  Alert,
  ScrollView,
  StyleSheet,
  Switch,
  Text,
  TextInput,
  TouchableOpacity,
  View,
} from 'react-native';
import {useAuth} from '../auth/useAuth';
import {useStore} from '../store/useStore';
import {ozmaClient} from '../api/client';

export function SettingsScreen() {
  const {controllerUrl, setControllerUrl, logout, tokens} = useAuth();
  const [urlInput, setUrlInput] = useState(controllerUrl ?? '');
  const [urlSaved, setUrlSaved] = useState(false);
  const [testing, setTesting] = useState(false);
  const [pingResult, setPingResult] = useState<string | null>(null);

  const notificationPrefs = useStore((s) => s.notificationPrefs);
  const setNotificationPrefs = useStore((s) => s.setNotificationPrefs);
  const push = useStore((s) => s.push);

  const handleSaveUrl = useCallback(() => {
    const cleaned = urlInput.trim().replace(/\/$/, '');
    if (!cleaned) {
      Alert.alert('Invalid URL', 'Enter the controller URL including http(s)://');
      return;
    }
    setControllerUrl(cleaned);
    setUrlSaved(true);
    setTimeout(() => setUrlSaved(false), 2000);
  }, [urlInput, setControllerUrl]);

  const handlePingController = useCallback(async () => {
    setTesting(true);
    setPingResult(null);
    const ok = await ozmaClient.ping();
    setPingResult(ok ? 'Controller reachable' : 'Cannot reach controller');
    setTesting(false);
  }, []);

  const handleTestPush = useCallback(async () => {
    setTesting(true);
    try {
      const result = await ozmaClient.sendTestPush();
      Alert.alert('Push Test', result.message);
    } catch {
      Alert.alert('Push Test', 'Failed to send test notification. Is the push endpoint configured?');
    } finally {
      setTesting(false);
    }
  }, []);

  const handleLogout = useCallback(() => {
    Alert.alert(
      'Sign Out',
      'You will need to sign in again to use the app.',
      [
        {text: 'Cancel', style: 'cancel'},
        {text: 'Sign Out', style: 'destructive', onPress: () => logout()},
      ],
    );
  }, [logout]);

  // Decode display name from id_token if available
  const displayName = getDisplayName(tokens?.id_token ?? null);

  return (
    <ScrollView
      style={styles.container}
      contentContainerStyle={styles.content}
      keyboardShouldPersistTaps="handled">
      {/* Account */}
      <Section title="Account">
        {displayName && (
          <SettingRow label="Signed in as" value={displayName} />
        )}
        <TouchableOpacity style={styles.signOutButton} onPress={handleLogout}>
          <Text style={styles.signOutText}>Sign Out</Text>
        </TouchableOpacity>
      </Section>

      {/* Controller URL */}
      <Section title="Controller">
        <Text style={styles.fieldLabel}>Connect Relay URL</Text>
        <TextInput
          style={styles.textInput}
          value={urlInput}
          onChangeText={setUrlInput}
          placeholder="https://my-controller.connect.ozma.io"
          placeholderTextColor="#6B7280"
          autoCapitalize="none"
          autoCorrect={false}
          keyboardType="url"
          returnKeyType="done"
          onSubmitEditing={handleSaveUrl}
        />
        <View style={styles.buttonRow}>
          <TouchableOpacity style={styles.saveButton} onPress={handleSaveUrl}>
            <Text style={styles.saveButtonText}>
              {urlSaved ? 'Saved!' : 'Save'}
            </Text>
          </TouchableOpacity>
          <TouchableOpacity
            style={styles.testButton}
            onPress={handlePingController}
            disabled={testing}>
            {testing ? (
              <ActivityIndicator size="small" color="#FFFFFF" />
            ) : (
              <Text style={styles.testButtonText}>Test Connection</Text>
            )}
          </TouchableOpacity>
        </View>
        {pingResult && (
          <Text
            style={[
              styles.pingResult,
              pingResult.includes('reachable')
                ? styles.pingOk
                : styles.pingFail,
            ]}>
            {pingResult}
          </Text>
        )}
      </Section>

      {/* Notifications */}
      <Section title="Notifications">
        <ToggleRow
          label="Motion alerts"
          value={notificationPrefs.motionAlerts}
          onChange={(v) => setNotificationPrefs({motionAlerts: v})}
        />
        <ToggleRow
          label="Machine went offline"
          value={notificationPrefs.nodeOfflineAlerts}
          onChange={(v) => setNotificationPrefs({nodeOfflineAlerts: v})}
        />
        <ToggleRow
          label="Machine came online"
          value={notificationPrefs.nodeOnlineAlerts}
          onChange={(v) => setNotificationPrefs({nodeOnlineAlerts: v})}
        />
        <ToggleRow
          label="Snapshot in notification"
          value={notificationPrefs.snapshotInNotification}
          onChange={(v) => setNotificationPrefs({snapshotInNotification: v})}
        />

        {push.isRegistered && (
          <TouchableOpacity
            style={styles.testButton}
            onPress={handleTestPush}
            disabled={testing}>
            <Text style={styles.testButtonText}>Send Test Notification</Text>
          </TouchableOpacity>
        )}
        {!push.isRegistered && (
          <Text style={styles.pushWarning}>
            Push notifications not registered. Restart the app to retry.
          </Text>
        )}
      </Section>

      {/* About */}
      <Section title="About">
        <SettingRow label="App version" value="0.1.0" />
        <SettingRow label="Push token" value={push.deviceToken ? 'Registered' : 'Not registered'} />
      </Section>
    </ScrollView>
  );
}

// ── Sub-components ────────────────────────────────────────────────────────────

function Section({title, children}: {title: string; children: React.ReactNode}) {
  return (
    <View style={styles.section}>
      <Text style={styles.sectionTitle}>{title}</Text>
      <View style={styles.sectionBody}>{children}</View>
    </View>
  );
}

function SettingRow({label, value}: {label: string; value: string}) {
  return (
    <View style={styles.settingRow}>
      <Text style={styles.settingLabel}>{label}</Text>
      <Text style={styles.settingValue}>{value}</Text>
    </View>
  );
}

function ToggleRow({
  label,
  value,
  onChange,
}: {
  label: string;
  value: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <View style={styles.settingRow}>
      <Text style={styles.settingLabel}>{label}</Text>
      <Switch
        value={value}
        onValueChange={onChange}
        trackColor={{true: '#2563EB'}}
        thumbColor="#FFFFFF"
      />
    </View>
  );
}

function getDisplayName(idToken: string | null): string | null {
  if (!idToken) {
    return null;
  }
  try {
    const parts = idToken.split('.');
    if (parts.length !== 3) {
      return null;
    }
    const payload = JSON.parse(atob(parts[1] as string)) as {
      name?: string;
      email?: string;
      preferred_username?: string;
    };
    return payload.name ?? payload.preferred_username ?? payload.email ?? null;
  } catch {
    return null;
  }
}

// ── Styles ────────────────────────────────────────────────────────────────────

const styles = StyleSheet.create({
  container: {
    flex: 1,
    backgroundColor: '#111827',
  },
  content: {
    paddingBottom: 40,
  },
  section: {
    marginTop: 24,
    marginHorizontal: 16,
  },
  sectionTitle: {
    color: '#6B7280',
    fontSize: 12,
    fontWeight: '600',
    textTransform: 'uppercase',
    letterSpacing: 0.8,
    marginBottom: 10,
  },
  sectionBody: {
    backgroundColor: '#1F2937',
    borderRadius: 10,
    overflow: 'hidden',
    borderWidth: 1,
    borderColor: '#374151',
    gap: 1,
  },
  settingRow: {
    flexDirection: 'row',
    justifyContent: 'space-between',
    alignItems: 'center',
    paddingHorizontal: 14,
    paddingVertical: 12,
    backgroundColor: '#1F2937',
  },
  settingLabel: {
    color: '#D1D5DB',
    fontSize: 15,
  },
  settingValue: {
    color: '#6B7280',
    fontSize: 14,
  },
  fieldLabel: {
    color: '#9CA3AF',
    fontSize: 13,
    paddingHorizontal: 14,
    paddingTop: 12,
    paddingBottom: 4,
  },
  textInput: {
    backgroundColor: '#111827',
    marginHorizontal: 14,
    marginBottom: 10,
    borderWidth: 1,
    borderColor: '#374151',
    borderRadius: 8,
    paddingHorizontal: 12,
    paddingVertical: 10,
    color: '#F9FAFB',
    fontSize: 14,
  },
  buttonRow: {
    flexDirection: 'row',
    gap: 10,
    paddingHorizontal: 14,
    paddingBottom: 12,
  },
  saveButton: {
    flex: 1,
    backgroundColor: '#1D4ED8',
    paddingVertical: 10,
    borderRadius: 8,
    alignItems: 'center',
  },
  saveButtonText: {
    color: '#FFFFFF',
    fontWeight: '600',
    fontSize: 14,
  },
  testButton: {
    flex: 1,
    backgroundColor: '#374151',
    paddingVertical: 10,
    borderRadius: 8,
    alignItems: 'center',
  },
  testButtonText: {
    color: '#D1D5DB',
    fontWeight: '500',
    fontSize: 14,
  },
  pingResult: {
    fontSize: 13,
    textAlign: 'center',
    paddingBottom: 12,
    paddingHorizontal: 14,
  },
  pingOk: {
    color: '#34D399',
  },
  pingFail: {
    color: '#F87171',
  },
  signOutButton: {
    paddingHorizontal: 14,
    paddingVertical: 12,
    backgroundColor: '#1F2937',
  },
  signOutText: {
    color: '#EF4444',
    fontSize: 15,
    fontWeight: '500',
  },
  pushWarning: {
    color: '#FBBF24',
    fontSize: 13,
    paddingHorizontal: 14,
    paddingBottom: 12,
  },
});
