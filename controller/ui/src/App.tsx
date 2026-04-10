import { Routes, Route } from 'react-router-dom'
import Layout from './components/Layout'
import NodesPage from './pages/NodesPage'
import NodeDetailPage from './pages/NodeDetailPage'

function App() {
  return (
    <Layout>
      <Routes>
        <Route path="/" element={<NodesPage />} />
        <Route path="/nodes/:nodeId" element={<NodeDetailPage />} />
      </Routes>
    </Layout>
  )
}

export default App
