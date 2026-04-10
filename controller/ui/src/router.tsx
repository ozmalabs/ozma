import { BrowserRouter, Routes, Route, Navigate } from 'react-router-dom'
import Layout from './components/Layout'
import NodesPage from './pages/NodesPage'

export const Router = () => {
  return (
    <BrowserRouter>
      <Routes>
        <Route path="/" element={<Layout />}>
          <Route index path="/nodes" element={<NodesPage />} />
          <Route path="/scenarios" element={<div>Scenarios page (coming soon)</div>} />
          <Route path="/streams" element={<div>Streams page (coming soon)</div>} />
          <Route path="/settings" element={<div>Settings page (coming soon)</div>} />
          <Route path="*" element={<Navigate to="/nodes" replace />} />
        </Route>
      </Routes>
    </BrowserRouter>
  )
}

export default Router
