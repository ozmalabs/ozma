import { Routes, Route } from 'react-router-dom'
import Layout from './components/Layout'
import NodesPage from './pages/NodesPage'

export default function App() {
  return (
    <Layout>
      <Routes>
        <Route path="/" element={<NodesPage />} />
      </Routes>
    </Layout>
  )
}
