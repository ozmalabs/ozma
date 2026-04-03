/**
 * LoginScreen — OIDC login via system browser.
 *
 * Shown when no valid tokens exist. The controller URL must already be set
 * (via OnboardingScreen); if not, we redirect there first.
 */

import React, {useCallback} from 'react';
import {
  ActivityIndicator,
  StyleSheet,
  Text,
  TouchableOpacity,
  View,
} from 'react-native';
import {useAuth} from '../auth/useAuth';

export function LoginScreen() {
  const {login, isLoading, error, controllerUrl} = useAuth();

  const handleLogin = useCallback(() => {
    login().catch(() => undefined);
  }, [login]);

  return (
    <View style={styles.container}>
      <View style={styles.logoArea}>
        <View style={styles.logoMark}>
          <Text style={styles.logoText}>oz</Text>
        </View>
        <Text style={styles.appName}>Ozma</Text>
        <Text style={styles.tagline}>KVMA router — home & small business IT</Text>
      </View>

      <View style={styles.body}>
        {controllerUrl ? (
          <>
            <Text style={styles.controllerLabel}>Connecting to</Text>
            <Text style={styles.controllerUrl} numberOfLines={1}>
              {controllerUrl}
            </Text>
          </>
        ) : (
          <Text style={styles.noControllerHint}>
            Controller URL not set. Go back to set it up.
          </Text>
        )}

        {error && <Text style={styles.errorText}>{error}</Text>}

        <TouchableOpacity
          style={[
            styles.loginButton,
            (!controllerUrl || isLoading) && styles.loginButtonDisabled,
          ]}
          onPress={handleLogin}
          disabled={!controllerUrl || isLoading}>
          {isLoading ? (
            <ActivityIndicator color="#FFFFFF" />
          ) : (
            <Text style={styles.loginButtonText}>Sign in with Ozma</Text>
          )}
        </TouchableOpacity>

        <Text style={styles.hint}>
          Opens your browser for secure sign-in via Authentik OIDC.
        </Text>
      </View>
    </View>
  );
}

const styles = StyleSheet.create({
  container: {
    flex: 1,
    backgroundColor: '#0F172A',
    justifyContent: 'center',
    padding: 32,
  },
  logoArea: {
    alignItems: 'center',
    marginBottom: 48,
  },
  logoMark: {
    width: 72,
    height: 72,
    borderRadius: 20,
    backgroundColor: '#2563EB',
    justifyContent: 'center',
    alignItems: 'center',
    marginBottom: 16,
  },
  logoText: {
    color: '#FFFFFF',
    fontSize: 28,
    fontWeight: '800',
    letterSpacing: -1,
  },
  appName: {
    color: '#F9FAFB',
    fontSize: 32,
    fontWeight: '800',
    letterSpacing: -1,
  },
  tagline: {
    color: '#6B7280',
    fontSize: 14,
    marginTop: 6,
    textAlign: 'center',
  },
  body: {
    gap: 16,
  },
  controllerLabel: {
    color: '#6B7280',
    fontSize: 13,
    textAlign: 'center',
  },
  controllerUrl: {
    color: '#93C5FD',
    fontSize: 14,
    textAlign: 'center',
    fontFamily: 'monospace',
  },
  noControllerHint: {
    color: '#FBBF24',
    fontSize: 14,
    textAlign: 'center',
  },
  errorText: {
    color: '#F87171',
    fontSize: 14,
    textAlign: 'center',
    backgroundColor: '#450A0A',
    padding: 12,
    borderRadius: 8,
  },
  loginButton: {
    backgroundColor: '#2563EB',
    paddingVertical: 15,
    borderRadius: 12,
    alignItems: 'center',
    marginTop: 8,
  },
  loginButtonDisabled: {
    backgroundColor: '#1E3A5F',
    opacity: 0.6,
  },
  loginButtonText: {
    color: '#FFFFFF',
    fontSize: 16,
    fontWeight: '700',
  },
  hint: {
    color: '#4B5563',
    fontSize: 12,
    textAlign: 'center',
  },
});
