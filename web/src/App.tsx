import { Button, Result } from 'antd';
import { Navigate, Route, Routes, useNavigate } from 'react-router-dom';

import { AppLayout } from './components/AppLayout';
import { AssetsPage } from './pages/AssetsPage';
import { JobsPage } from './pages/JobsPage';
import { SearchPage } from './pages/SearchPage';
import { StatsPage } from './pages/StatsPage';

function NotFoundPage() {
  const navigate = useNavigate();
  return (
    <Result
      status="404"
      title="页面不存在"
      subTitle="该路由没有对应页面，可能地址输入有误。"
      extra={
        <Button type="primary" onClick={() => navigate('/assets')}>
          回到资产库
        </Button>
      }
    />
  );
}

/** 前端路由：/assets /search /jobs /stats（SPA，未知路径由后端回落 index.html） */
export default function App() {
  return (
    <Routes>
      <Route element={<AppLayout />}>
        <Route path="/" element={<Navigate to="/assets" replace />} />
        <Route path="/assets" element={<AssetsPage />} />
        <Route path="/search" element={<SearchPage />} />
        <Route path="/jobs" element={<JobsPage />} />
        <Route path="/stats" element={<StatsPage />} />
        <Route path="*" element={<NotFoundPage />} />
      </Route>
    </Routes>
  );
}
