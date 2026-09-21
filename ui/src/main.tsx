import React from 'react'
import ReactDOM from 'react-dom/client'
import { BrowserRouter } from 'react-router'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import App from './App'
import { prefetchBaseUi } from '@/components/ui'
import { initTheme } from '@/lib/theme'
import './index.css'

initTheme()
// 提示气泡和浮层面板那一坨不在首屏包里，等首屏画完趁空闲取回来
prefetchBaseUi()

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 10_000,
      refetchOnWindowFocus: false,
      retry: (count, error) => {
        // 4xx 重试没意义；超时 / 太重的查询重试只会更堵
        const status = (error as { status?: number }).status ?? 0
        return status >= 500 && status !== 504 && count < 1
      },
    },
  },
})

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <BrowserRouter>
        <App />
      </BrowserRouter>
    </QueryClientProvider>
  </React.StrictMode>,
)
