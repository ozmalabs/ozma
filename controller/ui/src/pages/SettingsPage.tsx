import { useAuthStore } from '../store/useAuthStore'

export default function SettingsPage() {
  const { user, logout } = useAuthStore()

  return (
    <div className="p-6 max-w-2xl">
      <h1 className="text-2xl font-bold mb-6">Settings</h1>

      <div className="rounded-xl border bg-card overflow-hidden">
        {/* User info */}
        <div className="p-6 border-b">
          <h2 className="text-lg font-semibold mb-3">Account</h2>
          {user ? (
            <div className="space-y-1 text-sm">
              <p>
                <span className="text-muted-foreground">Username: </span>
                <span className="font-medium">{user.username}</span>
              </p>
              <p>
                <span className="text-muted-foreground">Email: </span>
                <span className="font-medium">{user.email}</span>
              </p>
              {user.roles.length > 0 && (
                <div className="flex gap-1 mt-2 flex-wrap">
                  {user.roles.map((role) => (
                    <span key={role} className="px-2 py-0.5 bg-secondary rounded text-xs">
                      {role}
                    </span>
                  ))}
                </div>
              )}
            </div>
          ) : (
            <p className="text-muted-foreground text-sm">Not signed in</p>
          )}
        </div>

        {/* Sign out */}
        <div className="p-6">
          <button
            onClick={() => logout()}
            className="px-4 py-2 bg-destructive text-destructive-foreground rounded-lg hover:bg-destructive/90 transition-colors text-sm font-medium"
          >
            Sign Out
          </button>
        </div>
      </div>
    </div>
  )
}
