/**
 * OnboardingScreen — first-run setup.
 *
 * Step 1: Enter the controller / Connect relay URL.
 * Step 2: Trigger OIDC login.
 *
 * Once the user has a valid token, this screen is never shown again.
 */

import React, {useCallback, useState} from 'react';
import {
  ActivityIndicator,
  KeyboardAvoidingView,
  Platform,
  ScrollView,
  StyleSheet,
  Text,
  TextInput,
  TouchableOpacity,
  View,
} from 'react-native';
import {useAuth} from '../auth/useAuth';
import {ozmaClient} from '../api/client';

type Step = 'url' | 'login';

export function OnboardingScreen() {
  const {setControllerUrl, login, isLoading, error} = useAuth();
  const [step, setStep] = useState<Step>('url');
  const [urlInput, setUrlInput] = useState('');
  const [validating, setValidating] = useState(false);
  const [validationError, setValidationError] = useState<string | null>(null);

  const handleValidateUrl = useCallback(async () => {
    const cleaned = urlInput.trim().replace(/\/$/, '');
    if (!cleaned) {
      setValidationError('Please enter the controller URL.');
      return;
    }
    if (!cleaned.startsWith('http://') && !cleaned.startsWith('https://')) {
      setValidationError('URL must start with http:// or https://');
      return;
    }

    setValidating(true);
    setValidationError(null);

    try {
      // Test connectivity before saving.
      ozmaClient.setControllerUrl(cleaned);
      const ok = await ozmaClient.ping();
      if (!ok) {
        setValidationError(
          'Could not reach the controller. Check the URL and your network connection.',
        );
        return;
      }
      setControllerUrl(cleaned);
      setStep('login');
    } catch {
      setValidationError('Connection failed. Check the URL and try again.');
    } finally {
      setValidating(false);
    }
  }, [urlInput, setControllerUrl]);

  const handleLogin = useCallback(() => {
    login().catch(() => undefined);
  }, [login]);

  return (
    <KeyboardAvoidingView
      style={styles.flex}
      behavior={Platform.OS === 'ios' ? 'padding' : undefined}>
      <ScrollView
        style={styles.flex}
        contentContainerStyle={styles.container}
        keyboardShouldPersistTaps="handled">
        {/* Header */}
        <View style={styles.logoArea}>
          <View style={styles.logoMark}>
            <Text style={styles.logoText}>oz</Text>
          </View>
          <Text style={styles.appName}>Welcome to Ozma</Text>
        </View>

        {/* Step indicator */}
        <View style={styles.steps}>
          <StepDot active={step === 'url'} done={step === 'login'} label="1" />
          <View style={[styles.stepLine, step === 'login' && styles.stepLineDone]} />
          <StepDot active={step === 'login'} done={false} label="2" />
        </View>

        {step === 'url' ? (
          <UrlStep
            urlInput={urlInput}
            onChangeUrl={setUrlInput}
            onNext={handleValidateUrl}
            validating={validating}
            error={validationError}
          />
        ) : (
          <LoginStep
            onLogin={handleLogin}
            isLoading={isLoading}
            error={error}
          />
        )}
      </ScrollView>
    </KeyboardAvoidingView>
  );
}

// ── Step components ───────────────────────────────────────────────────────────

function UrlStep({
  urlInput,
  onChangeUrl,
  onNext,
  validating,
  error,
}: {
  urlInput: string;
  onChangeUrl: (v: string) => void;
  onNext: () => void;
  validating: boolean;
  error: string | null;
}) {
  return (
    <View style={styles.stepContent}>
      <Text style={styles.stepTitle}>Connect to your controller</Text>
      <Text style={styles.stepBody}>
        Enter your Ozma controller address. This is the Connect relay URL shown
        in your controller dashboard, e.g.{'\n'}
        <Text style={styles.mono}>https://myhome.connect.ozma.io</Text>
      </Text>

      <TextInput
        style={styles.textInput}
        value={urlInput}
        onChangeText={onChangeUrl}
        placeholder="https://myhome.connect.ozma.io"
        placeholderTextColor="#4B5563"
        autoCapitalize="none"
        autoCorrect={false}
        keyboardType="url"
        returnKeyType="next"
        onSubmitEditing={onNext}
      />

      {error && <Text style={styles.errorText}>{error}</Text>}

      <TouchableOpacity
        style={[styles.primaryButton, validating && styles.primaryButtonLoading]}
        onPress={onNext}
        disabled={validating}>
        {validating ? (
          <ActivityIndicator color="#FFFFFF" />
        ) : (
          <Text style={styles.primaryButtonText}>Continue</Text>
        )}
      </TouchableOpacity>

      <Text style={styles.hint}>
        Self-hosted? Use your local IP or hostname:{'\n'}
        <Text style={styles.mono}>http://192.168.1.50:7380</Text>
      </Text>
    </View>
  );
}

function LoginStep({
  onLogin,
  isLoading,
  error,
}: {
  onLogin: () => void;
  isLoading: boolean;
  error: string | null;
}) {
  return (
    <View style={styles.stepContent}>
      <Text style={styles.stepTitle}>Sign in</Text>
      <Text style={styles.stepBody}>
        Tap below to open your browser and sign in securely via your Ozma
        controller. Your credentials never touch this app.
      </Text>

      {error && <Text style={styles.errorText}>{error}</Text>}

      <TouchableOpacity
        style={[styles.primaryButton, isLoading && styles.primaryButtonLoading]}
        onPress={onLogin}
        disabled={isLoading}>
        {isLoading ? (
          <ActivityIndicator color="#FFFFFF" />
        ) : (
          <Text style={styles.primaryButtonText}>Sign in with Ozma</Text>
        )}
      </TouchableOpacity>

      <Text style={styles.hint}>
        Uses OAuth 2.0 + PKCE via Authentik. No passwords stored in the app.
      </Text>
    </View>
  );
}

function StepDot({
  active,
  done,
  label,
}: {
  active: boolean;
  done: boolean;
  label: string;
}) {
  return (
    <View
      style={[
        styles.stepDot,
        active && styles.stepDotActive,
        done && styles.stepDotDone,
      ]}>
      <Text
        style={[
          styles.stepDotText,
          (active || done) && styles.stepDotTextActive,
        ]}>
        {done ? '✓' : label}
      </Text>
    </View>
  );
}

// ── Styles ────────────────────────────────────────────────────────────────────

const styles = StyleSheet.create({
  flex: {
    flex: 1,
    backgroundColor: '#0F172A',
  },
  container: {
    flexGrow: 1,
    padding: 28,
    justifyContent: 'center',
    gap: 32,
  },
  logoArea: {
    alignItems: 'center',
    gap: 14,
  },
  logoMark: {
    width: 64,
    height: 64,
    borderRadius: 18,
    backgroundColor: '#2563EB',
    justifyContent: 'center',
    alignItems: 'center',
  },
  logoText: {
    color: '#FFFFFF',
    fontSize: 24,
    fontWeight: '800',
  },
  appName: {
    color: '#F9FAFB',
    fontSize: 26,
    fontWeight: '800',
    letterSpacing: -0.5,
  },
  steps: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'center',
  },
  stepDot: {
    width: 32,
    height: 32,
    borderRadius: 16,
    backgroundColor: '#1F2937',
    borderWidth: 2,
    borderColor: '#374151',
    justifyContent: 'center',
    alignItems: 'center',
  },
  stepDotActive: {
    borderColor: '#2563EB',
    backgroundColor: '#1E3A5F',
  },
  stepDotDone: {
    backgroundColor: '#065F46',
    borderColor: '#10B981',
  },
  stepDotText: {
    color: '#6B7280',
    fontSize: 13,
    fontWeight: '700',
  },
  stepDotTextActive: {
    color: '#FFFFFF',
  },
  stepLine: {
    flex: 1,
    height: 2,
    backgroundColor: '#374151',
    marginHorizontal: 8,
    maxWidth: 60,
  },
  stepLineDone: {
    backgroundColor: '#10B981',
  },
  stepContent: {
    gap: 16,
  },
  stepTitle: {
    color: '#F9FAFB',
    fontSize: 22,
    fontWeight: '700',
    letterSpacing: -0.3,
  },
  stepBody: {
    color: '#9CA3AF',
    fontSize: 15,
    lineHeight: 22,
  },
  mono: {
    fontFamily: Platform.OS === 'ios' ? 'Menlo' : 'monospace',
    color: '#93C5FD',
    fontSize: 13,
  },
  textInput: {
    backgroundColor: '#1F2937',
    borderWidth: 1,
    borderColor: '#374151',
    borderRadius: 10,
    paddingHorizontal: 14,
    paddingVertical: 13,
    color: '#F9FAFB',
    fontSize: 15,
  },
  errorText: {
    color: '#F87171',
    fontSize: 14,
    backgroundColor: '#450A0A',
    padding: 12,
    borderRadius: 8,
  },
  primaryButton: {
    backgroundColor: '#2563EB',
    paddingVertical: 15,
    borderRadius: 12,
    alignItems: 'center',
  },
  primaryButtonLoading: {
    opacity: 0.7,
  },
  primaryButtonText: {
    color: '#FFFFFF',
    fontSize: 16,
    fontWeight: '700',
  },
  hint: {
    color: '#4B5563',
    fontSize: 12,
    lineHeight: 18,
    textAlign: 'center',
  },
});
