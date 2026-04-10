import { Routes, Route } from 'react-router-dom'
import { ThemeProvider } from './contexts/ThemeContext'
import Layout from './components/Layout'
import NodesPage from './pages/NodesPage'

export default function App() {
  return (
    <ThemeProvider>
      <Layout>
        <Routes>
          <Route path="/" element={<NodesPage />} />
          <Route path="/nodes" element={<NodesPage />} />
        </Routes>
      </Layout>
    </ThemeProvider>
  )
}
