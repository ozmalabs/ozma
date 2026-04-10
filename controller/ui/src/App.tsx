import { Routes, Route } from 'react-router-dom'
import Layout from './components/Layout'
import NodesPage from './routes/NodesPage'
import ScenariosPage from './routes/ScenariosPage'
import StreamsPage from './routes/StreamsPage'
import SettingsPage from './routes/SettingsPage'

export default function App() {
  return (
    <Layout>
      <Routes>
        <Route path="/" element={<NodesPage />} />
        <Route path="/nodes" element={<NodesPage />} />
        <Route path="/nodes/:nodeId" element={<NodesPage />} />
        <Route path="/scenarios" element={<ScenariosPage />} />
        <Route path="/streams" element={<StreamsPage />} />
        <Route path="/settings" element={<SettingsPage />} />
      </Routes>
    </Layout>
  )
}
