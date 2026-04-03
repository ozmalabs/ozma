/**
 * Ozma Mobile — root component.
 *
 * Wraps the entire app in:
 *   - SafeAreaProvider (insets for notched devices)
 *   - AuthProvider (OIDC tokens, controller URL)
 *   - AppNavigator (react-navigation tree)
 */

import React from 'react';
import {StatusBar} from 'react-native';
import {SafeAreaProvider} from 'react-native-safe-area-context';
import {AuthProvider} from './src/auth/AuthContext';
import {AppNavigator} from './src/navigation/AppNavigator';

export default function App() {
  return (
    <SafeAreaProvider>
      <StatusBar barStyle="light-content" backgroundColor="#0F172A" />
      <AuthProvider>
        <AppNavigator />
      </AuthProvider>
    </SafeAreaProvider>
  );
}
