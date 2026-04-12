import { lazy, Suspense } from 'react'
import { BrowserRouter, Routes, Route } from 'react-router-dom'

// Lazy-loaded route stubs
const Dashboard  = lazy(() => import('./pages/Dashboard'))
const Nodes      = lazy(() => import('./pages/Nodes'))
const NodeDetail = lazy(() => import('./pages/NodeDetail'))
const Scenarios  = lazy(() => import('./pages/Scenarios'))
const Audio      = lazy(() => import('./pages/Audio'))
const Streaming  = lazy(() => import('./pages/Streaming'))
const Controls   = lazy(() => import('./pages/Controls'))
const RGB        = lazy(() => import('./pages/RGB'))
const Settings   = lazy(() => import('./pages/Settings'))
const Login      = lazy(() => import('./pages/Login'))

function PageFallback() {
  return (
    <div className="flex items-center justify-center h-screen bg-oz-bg text-oz-muted font-mono text-sm">
      Loading…
    </div>
  )
}

export default function App() {
  return (
    <BrowserRouter basename="/ui">
      <Suspense fallback={<PageFallback />}>
        <Routes>
          <Route path="/"          element={<Dashboard />} />
          <Route path="/nodes"     element={<Nodes />} />
          <Route path="/nodes/:id" element={<NodeDetail />} />
          <Route path="/scenarios" element={<Scenarios />} />
          <Route path="/audio"     element={<Audio />} />
          <Route path="/streaming" element={<Streaming />} />
          <Route path="/controls"  element={<Controls />} />
          <Route path="/rgb"       element={<RGB />} />
          <Route path="/settings"  element={<Settings />} />
          <Route path="/login"     element={<Login />} />
        </Routes>
      </Suspense>
    </BrowserRouter>
  )
}
