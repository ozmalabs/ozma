import { BrowserRouter, Routes, Route } from 'react-router-dom'
import Layout from './layouts/Layout'
import NodesPage from './pages/NodesPage'
import ControlsPage from './pages/ControlsPage'

function App() {
  return (
    <BrowserRouter>
      <Layout>
        <Routes>
          <Route path="/" element={<NodesPage />} />
          <Route path="/nodes" element={<NodesPage />} />
          <Route path="/controls" element={<ControlsPage />} />
        </Routes>
      </Layout>
    </BrowserRouter>
  )
}

export default App
