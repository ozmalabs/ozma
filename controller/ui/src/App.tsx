import { BrowserRouter, Routes, Route } from 'react-router-dom'
import Layout from './layouts/Layout'
import NodesPage from './pages/NodesPage'
import NodeDetailPage from './pages/NodeDetailPage'

function App() {
  return (
    <BrowserRouter>
      <Layout>
        <Routes>
          <Route path="/" element={<NodesPage />} />
          <Route path="/nodes" element={<NodesPage />} />
          <Route path="/nodes/:id" element={<NodeDetailPage />} />
        </Routes>
      </Layout>
    </BrowserRouter>
  )
}

export default App
