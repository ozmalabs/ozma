import { useParams } from 'react-router-dom'

const NodeDetailPage = () => {
  const { id } = useParams<{ id: string }>()
  
  return (
    <div className="p-6">
      <h1 className="text-2xl font-bold text-emerald-400">Node {id}</h1>
      <p className="mt-4 text-gray-300">Node detail page coming soon...</p>
    </div>
  )
}

export default NodeDetailPage
