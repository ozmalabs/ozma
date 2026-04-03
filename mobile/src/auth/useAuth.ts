import {useContext} from 'react';
import {AuthContext, AuthContextValue} from './AuthContext';

/**
 * Hook to access auth state and actions.
 *
 * Must be used within <AuthProvider>.
 */
export function useAuth(): AuthContextValue {
  const ctx = useContext(AuthContext);
  if (!ctx) {
    throw new Error('useAuth must be used within an AuthProvider');
  }
  return ctx;
}
