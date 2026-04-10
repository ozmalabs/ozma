import { Routes, Route } from 'react-router-dom'
import Layout from './components/Layout'
import NodesPage from './pages/NodesPage'

function App() {
  return (
    <Layout>
      <Routes>
        <Route path="/" element={<NodesPage />} />
        <Route path="/nodes" element={<NodesPage />} />
      </Routes>
    </Layout>
  )
}

export default App
