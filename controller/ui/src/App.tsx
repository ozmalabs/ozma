import { BrowserRouter, Routes, Route } from 'react-router-dom'
import Layout from './layouts/Layout'
import Dashboard from './pages/Dashboard'
import NodesPage from './pages/NodesPage'
import ScenariosPage from './pages/ScenariosPage'
import AudioPage from './pages/AudioPage'
import SettingsPage from './pages/SettingsPage'

function App() {
  return (
    <BrowserRouter>
      <Layout>
        <Routes>
          <Route path="/" element={<Dashboard />} />
          <Route path="/dashboard" element={<Dashboard />} />
          <Route path="/nodes" element={<NodesPage />} />
          <Route path="/scenarios" element={<ScenariosPage />} />
          <Route path="/audio" element={<AudioPage />} />
          <Route path="/settings" element={<SettingsPage />} />
        </Routes>
      </Layout>
    </BrowserRouter>
  )
}

export default App
