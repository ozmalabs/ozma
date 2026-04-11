export default function ScenariosPage() {
  return (
    <div className="h-full">
      <div className="mb-6">
        <h1 className="text-3xl font-bold text-foreground">Scenarios</h1>
        <p className="text-muted-foreground mt-1">
          Manage your automation scenarios
        </p>
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-4">
        {/* Scenario Card */}
        <div className="bg-card rounded-xl border p-6 hover:border-emerald-500/50 transition-all group cursor-pointer">
          <div className="flex items-center gap-3 mb-4">
            <div className="h-10 w-10 rounded-lg bg-violet-500/10 flex items-center justify-center">
              <svg
                xmlns="http://www.w3.org/2000/svg"
                width="20"
                height="20"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                className="text-violet-500"
              >
                <path d="m12 3-1.912 5.813a2 2 0 0 1-1.275 1.275L3 12l5.813 1.912a2 2 0 0 1 1.275 1.275L12 21l1.912-5.813a2 2 0 0 1 1.275-1.275L21 12l-5.813-1.912a2 2 0 0 1-1.275-1.275L12 3z" />
              </svg>
            </div>
            <div>
              <h3 className="font-semibold text-lg group-hover:text-violet-500 transition-colors">Default Scenario</h3>
              <p className="text-sm text-muted-foreground">Auto-assigns nodes to optimal streams</p>
            </div>
          </div>
          <div className="flex items-center gap-2 text-xs text-violet-500/80">
            <span className="px-2 py-0.5 bg-violet-500/10 rounded">Active</span>
            <span className="mx-1">•</span>
            <span>3 nodes assigned</span>
          </div>
        </div>

        {/* Scenario Card */}
        <div className="bg-card rounded-xl border p-6 hover:border-emerald-500/50 transition-all group cursor-pointer">
          <div className="flex items-center gap-3 mb-4">
            <div className="h-10 w-10 rounded-lg bg-blue-500/10 flex items-center justify-center">
              <svg
                xmlns="http://www.w3.org/2000/svg"
                width="20"
                height="20"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                className="text-blue-500"
              >
                <path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10z" />
              </svg>
            </div>
            <div>
              <h3 className="font-semibold text-lg group-hover:text-blue-500 transition-colors">Broadcast Mode</h3>
              <p className="text-sm text-muted-foreground">High-priority streaming for events</p>
            </div>
          </div>
          <div className="flex items-center gap-2 text-xs text-blue-500/80">
            <span className="px-2 py-0.5 bg-blue-500/10 rounded">Ready</span>
            <span className="mx-1">•</span>
            <span>2 nodes assigned</span>
          </div>
        </div>

        {/* Add Scenario Card */}
        <div className="bg-card rounded-xl border border-dashed p-6 flex flex-col items-center justify-center hover:border-emerald-500/50 transition-all group cursor-pointer min-h-[180px]">
          <div className="h-10 w-10 rounded-full bg-border flex items-center justify-center mb-3 group-hover:bg-emerald-500/20 transition-colors">
            <svg
              xmlns="http://www.w3.org/2000/svg"
              width="20"
              height="20"
              viewBox="0 0 24 24"
              fill="none"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
              className="text-muted-foreground group-hover:text-emerald-500 transition-colors"
            >
              <circle cx="12" cy="12" r="10" />
              <line x1="12" x2="12" y1="8" y2="16" />
              <line x1="8" x2="16" y1="12" y2="12" />
            </svg>
          </div>
          <h3 className="font-semibold text-sm group-hover:text-emerald-500 transition-colors">Create New Scenario</h3>
          <p className="text-xs text-muted-foreground mt-1">Define custom node grouping rules</p>
        </div>
      </div>
    </div>
  )
}
