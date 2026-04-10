import { useParams } from 'react-router-dom'
import Layout from '../components/layout/Layout'

export default function NodeDetailPage() {
  const { id } = useParams<{ id: string }>()

  return (
    <Layout>
      <div className="container mx-auto p-6">
        <h1 className="text-2xl font-bold mb-6">Node Details</h1>
        <p className="text-lg">Node {id} - Detail view coming soon</p>
      </div>
    </Layout>
  )
}
