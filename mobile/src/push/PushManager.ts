/**
 * Push notification manager.
 *
 * Handles:
 * - FCM token acquisition (Android + iOS via react-native-firebase/messaging)
 * - APNs permission request on iOS
 * - Token registration with the Ozma controller
 * - Foreground notification display via @notifee/react-native
 * - Background message handler (must be called before AppRegistry)
 * - Incoming notification → store update
 */

import {Platform} from 'react-native';
import messaging, {
  FirebaseMessagingTypes,
} from '@react-native-firebase/messaging';
import notifee, {
  AndroidImportance,
  AndroidVisibility,
  EventType,
} from '@notifee/react-native';
import {ozmaClient} from '../api/client';
import {useStore} from '../store/useStore';
import {NotificationRecord} from '../api/types';

const CHANNEL_ID = 'ozma-alerts';
const CHANNEL_NAME = 'Ozma Alerts';

// ── Background handler (called from index.js BEFORE AppRegistry) ─────────────

function registerBackgroundHandler(): void {
  messaging().setBackgroundMessageHandler(async (remoteMessage) => {
    // Background messages are handled silently; notifee will display them
    // if data-only (no notification object).
    if (!remoteMessage.notification) {
      await displayForegroundNotification(remoteMessage);
    }
  });
}

// ── Channel setup ─────────────────────────────────────────────────────────────

async function ensureChannel(): Promise<void> {
  if (Platform.OS !== 'android') {
    return;
  }
  await notifee.createChannel({
    id: CHANNEL_ID,
    name: CHANNEL_NAME,
    importance: AndroidImportance.HIGH,
    visibility: AndroidVisibility.PUBLIC,
    sound: 'default',
    vibration: true,
  });
}

// ── Display notification via notifee ─────────────────────────────────────────

async function displayForegroundNotification(
  remoteMessage: FirebaseMessagingTypes.RemoteMessage,
): Promise<void> {
  await ensureChannel();

  const title = remoteMessage.notification?.title ??
    remoteMessage.data?.title as string | undefined ??
    'Ozma Alert';
  const body = remoteMessage.notification?.body ??
    remoteMessage.data?.body as string | undefined ??
    '';
  const snapshotUrl = remoteMessage.data?.snapshot_url as string | undefined;

  const iosAttachments: {url: string}[] = snapshotUrl
    ? [{url: snapshotUrl}]
    : [];

  await notifee.displayNotification({
    title,
    body,
    android: {
      channelId: CHANNEL_ID,
      importance: AndroidImportance.HIGH,
      pressAction: {id: 'default'},
      largeIcon: snapshotUrl ?? undefined,
    },
    ios: {
      attachments: iosAttachments,
    },
    data: remoteMessage.data as Record<string, string> | undefined,
  });
}

// ── Token registration ────────────────────────────────────────────────────────

async function registerToken(token: string): Promise<void> {
  const platform = Platform.OS === 'ios' ? 'ios' : 'android';
  try {
    const response = await ozmaClient.registerPushToken({
      device_token: token,
      platform,
    });
    useStore.getState().setPushRegistration(response.registration_id);
    useStore.getState().setPushToken(token);
  } catch {
    // Non-fatal; will retry on next launch.
  }
}

// ── Notification → store update ───────────────────────────────────────────────

function remoteMessageToRecord(
  msg: FirebaseMessagingTypes.RemoteMessage,
): NotificationRecord {
  const data = msg.data ?? {};
  return {
    id: (data.notification_id as string | undefined) ?? msg.messageId ?? String(Date.now()),
    title: msg.notification?.title ?? (data.title as string | undefined) ?? 'Ozma Alert',
    body: msg.notification?.body ?? (data.body as string | undefined) ?? '',
    created_at: new Date().toISOString(),
    snapshot_url: (data.snapshot_url as string | undefined) ?? null,
    camera_id: (data.camera_id as string | undefined) ?? null,
    node_id: (data.node_id as string | undefined) ?? null,
    event_type: (data.event_type as string | undefined) ?? 'push',
    read: false,
  };
}

// ── Main initialisation ───────────────────────────────────────────────────────

async function initialize(): Promise<() => void> {
  await ensureChannel();

  // Request permission (iOS requires explicit prompt; Android 13+ also does).
  const authStatus = await messaging().requestPermission();
  const enabled =
    authStatus === messaging.AuthorizationStatus.AUTHORIZED ||
    authStatus === messaging.AuthorizationStatus.PROVISIONAL;

  if (!enabled) {
    return () => undefined;
  }

  // Get and register the FCM token.
  const token = await messaging().getToken();
  await registerToken(token);

  // Re-register if the token rotates.
  const unsubTokenRefresh = messaging().onTokenRefresh((newToken) => {
    registerToken(newToken).catch(() => undefined);
  });

  // Foreground message handler.
  const unsubForeground = messaging().onMessage(async (remoteMessage) => {
    // Update in-app notification list.
    useStore.getState().prependNotification(remoteMessageToRecord(remoteMessage));

    // Display via notifee so foreground messages are visible.
    await displayForegroundNotification(remoteMessage);
  });

  // notifee foreground event handler (user taps notification action).
  const unsubNotifee = notifee.onForegroundEvent(({type}) => {
    if (type === EventType.PRESS) {
      // Navigation to notification screen is handled by AppNavigator
      // listening to notifee's initial notification.
    }
  });

  // Handle app opened from notification (terminated state).
  const initial = await notifee.getInitialNotification();
  if (initial) {
    // Signal the store that the app was opened from a notification.
    // AppNavigator checks this on mount to deep-link appropriately.
    const cameraId = initial.notification.data?.camera_id as string | undefined;
    if (cameraId) {
      useStore.getState().setSelectedCamera(cameraId);
    }
  }

  return () => {
    unsubTokenRefresh();
    unsubForeground();
    unsubNotifee();
  };
}

// ── Exports ───────────────────────────────────────────────────────────────────

export const PushManager = {
  registerBackgroundHandler,
  initialize,
  registerToken,
};
