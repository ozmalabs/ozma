import { Routes, Route } from 'react-router-dom'
import { Layout } from './components/layout'
import NodesPage from './pages/nodes'

export default function App() {
  return (
    <Layout>
      <Routes>
        <Route path="/" element={<NodesPage />} />
        <Route path="/nodes" element={<NodesPage />} />
      </Routes>
    </Layout>
  )
}
