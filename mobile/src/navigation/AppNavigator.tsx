/**
 * AppNavigator — root navigation structure.
 *
 * Tree:
 *   RootStack
 *   ├── Onboarding (shown when no controller URL configured)
 *   ├── Login (shown when controller URL set but no valid tokens)
 *   ├── MainTabs (bottom tab bar — shown when authenticated)
 *   │   ├── Cameras tab → CameraGridScreen
 *   │   ├── Machines tab → MachineListScreen
 *   │   ├── Notifications tab → NotificationsScreen
 *   │   ├── Guests tab → GuestInviteScreen
 *   │   └── Settings tab → SettingsScreen
 *   └── CameraDetail (modal/stack, pushed from cameras or notifications)
 */

import React, {useEffect} from 'react';
import {StyleSheet, Text, View} from 'react-native';
import {NavigationContainer} from '@react-navigation/native';
import {createNativeStackNavigator} from '@react-navigation/native-stack';
import {createBottomTabNavigator} from '@react-navigation/bottom-tabs';
import {useAuth} from '../auth/useAuth';
import {useStore} from '../store/useStore';
import {PushManager} from '../push/PushManager';

// Screens
import {CameraGridScreen} from '../screens/CameraGridScreen';
import {CameraDetailScreen} from '../screens/CameraDetailScreen';
import {MachineListScreen} from '../screens/MachineListScreen';
import {NotificationsScreen} from '../screens/NotificationsScreen';
import {GuestInviteScreen} from '../screens/GuestInviteScreen';
import {SettingsScreen} from '../screens/SettingsScreen';
import {LoginScreen} from '../screens/LoginScreen';
import {OnboardingScreen} from '../screens/OnboardingScreen';

// ── Route parameter types ─────────────────────────────────────────────────────

export type RootStackParamList = {
  Onboarding: undefined;
  Login: undefined;
  MainTabs: undefined;
  CameraDetail: {cameraId: string; cameraName: string};
};

export type TabParamList = {
  Cameras: undefined;
  Machines: undefined;
  Notifications: undefined;
  Guests: undefined;
  Settings: undefined;
};

const Stack = createNativeStackNavigator<RootStackParamList>();
const Tab = createBottomTabNavigator<TabParamList>();

// ── Bottom tab navigator ──────────────────────────────────────────────────────

function MainTabs() {
  const unreadCount = useStore((s) => s.unreadCount);

  return (
    <Tab.Navigator
      screenOptions={{
        tabBarStyle: tabStyles.bar,
        tabBarActiveTintColor: '#3B82F6',
        tabBarInactiveTintColor: '#6B7280',
        tabBarLabelStyle: tabStyles.label,
        headerStyle: tabStyles.header,
        headerTintColor: '#F9FAFB',
        headerTitleStyle: tabStyles.headerTitle,
      }}>
      <Tab.Screen
        name="Cameras"
        component={CameraGridScreen}
        options={{
          title: 'Cameras',
          tabBarIcon: ({color}) => <TabIcon emoji="📷" color={color} />,
        }}
      />
      <Tab.Screen
        name="Machines"
        component={MachineListScreen}
        options={{
          title: 'Machines',
          tabBarIcon: ({color}) => <TabIcon emoji="🖥" color={color} />,
        }}
      />
      <Tab.Screen
        name="Notifications"
        component={NotificationsScreen}
        options={{
          title: 'Alerts',
          tabBarBadge: unreadCount > 0 ? unreadCount : undefined,
          tabBarIcon: ({color}) => <TabIcon emoji="🔔" color={color} />,
        }}
      />
      <Tab.Screen
        name="Guests"
        component={GuestInviteScreen}
        options={{
          title: 'Guests',
          tabBarIcon: ({color}) => <TabIcon emoji="🔗" color={color} />,
        }}
      />
      <Tab.Screen
        name="Settings"
        component={SettingsScreen}
        options={{
          title: 'Settings',
          tabBarIcon: ({color}) => <TabIcon emoji="⚙️" color={color} />,
        }}
      />
    </Tab.Navigator>
  );
}

function TabIcon({emoji, color}: {emoji: string; color: string}) {
  return (
    <Text style={{fontSize: 18, opacity: color === '#3B82F6' ? 1 : 0.6}}>
      {emoji}
    </Text>
  );
}

// ── Root stack ────────────────────────────────────────────────────────────────

function RootNavigator() {
  const {isAuthenticated, controllerUrl} = useAuth();

  if (!controllerUrl) {
    return (
      <Stack.Navigator screenOptions={stackScreenOptions}>
        <Stack.Screen
          name="Onboarding"
          component={OnboardingScreen}
          options={{headerShown: false}}
        />
      </Stack.Navigator>
    );
  }

  if (!isAuthenticated) {
    return (
      <Stack.Navigator screenOptions={stackScreenOptions}>
        <Stack.Screen
          name="Login"
          component={LoginScreen}
          options={{headerShown: false}}
        />
      </Stack.Navigator>
    );
  }

  return (
    <Stack.Navigator screenOptions={stackScreenOptions}>
      <Stack.Screen
        name="MainTabs"
        component={MainTabs}
        options={{headerShown: false}}
      />
      <Stack.Screen
        name="CameraDetail"
        component={CameraDetailScreen}
        options={({route}) => ({
          title: route.params.cameraName,
          presentation: 'card',
        })}
      />
    </Stack.Navigator>
  );
}

// ── Push initialisation ───────────────────────────────────────────────────────

function PushInitializer() {
  const {isAuthenticated} = useAuth();

  useEffect(() => {
    if (!isAuthenticated) {
      return;
    }
    let cleanup: (() => void) | undefined;
    PushManager.initialize()
      .then((fn) => {
        cleanup = fn;
      })
      .catch(() => undefined);
    return () => cleanup?.();
  }, [isAuthenticated]);

  return null;
}

// ── App navigator (exported) ──────────────────────────────────────────────────

export function AppNavigator() {
  return (
    <NavigationContainer>
      <PushInitializer />
      <RootNavigator />
    </NavigationContainer>
  );
}

// ── Styles ────────────────────────────────────────────────────────────────────

const tabStyles = StyleSheet.create({
  bar: {
    backgroundColor: '#1F2937',
    borderTopColor: '#374151',
    borderTopWidth: 1,
    paddingBottom: 4,
  },
  label: {
    fontSize: 11,
  },
  header: {
    backgroundColor: '#1F2937',
    shadowColor: 'transparent',
    elevation: 0,
  },
  headerTitle: {
    color: '#F9FAFB',
    fontSize: 17,
    fontWeight: '600',
  },
});

const stackScreenOptions = {
  contentStyle: {backgroundColor: '#111827'},
  headerStyle: {backgroundColor: '#1F2937'},
  headerTintColor: '#F9FAFB',
  headerTitleStyle: {
    fontSize: 17,
    fontWeight: '600' as const,
    color: '#F9FAFB',
  },
};
