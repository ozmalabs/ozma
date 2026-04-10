import { BrowserRouter, Routes, Route } from 'react-router-dom'
import Layout from './layouts/Layout'
import NodesPage from './pages/NodesPage'

function App() {
  return (
    <BrowserRouter>
      <Layout>
        <Routes>
          <Route path="/" element={<NodesPage />} />
          <Route path="/nodes" element={<NodesPage />} />
        </Routes>
      </Layout>
    </BrowserRouter>
  )
}

export default App
