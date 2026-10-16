import { Alert, Layout, Menu, Space, Typography } from 'antd';
import { useCallback } from 'react';
import { Outlet, useLocation, useNavigate } from 'react-router-dom';

import { fetchHealth } from '../api';
import { useAsync } from '../hooks/useAsync';
import { HealthIndicator } from './HealthIndicator';

const { Text, Title } = Typography;

const NAV_ITEMS = [
  { key: '/assets', label: '资产库' },
  { key: '/search', label: '搜索' },
  { key: '/jobs', label: '作业' },
  { key: '/stats', label: '统计' },
];

/** 全局页头 + 内容容器；健康状态在这里统一拉取并展示 */
export function AppLayout() {
  const navigate = useNavigate();
  const location = useLocation();

  const loader = useCallback((signal: AbortSignal) => fetchHealth(signal), []);
  const { data: health, error, loading, reload } = useAsync(loader, []);

  const selectedKey =
    NAV_ITEMS.find((item) => location.pathname.startsWith(item.key))?.key ?? '/assets';

  const isMock = health?.encoder?.mode === 'mock';

  return (
    <Layout style={{ minHeight: '100vh', background: '#f5f5f5' }}>
      <div
        style={{
          background: '#fff',
          borderBottom: '1px solid #f0f0f0',
          padding: '10px 20px',
          position: 'sticky',
          top: 0,
          zIndex: 20,
        }}
      >
        {/* 第一行：标题 + 健康徽标（健康区放右，避免与导航争宽度） */}
        <div
          style={{
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'space-between',
            gap: 16,
            flexWrap: 'wrap',
          }}
        >
          <Space orientation="vertical" size={0}>
            <Title level={5} style={{ margin: 0 }}>
              Prompt Vault
            </Title>
            <Text type="secondary" style={{ fontSize: 12 }}>
              提示词资产库 · {health ? `v${health.version}` : '—'}
            </Text>
          </Space>

          <HealthIndicator
            health={health}
            error={error}
            loading={loading}
            onRefresh={reload}
          />
        </div>

        {/* 第二行：导航独占整行，窄视口下也不会被 antd 的溢出折叠吃掉 */}
        <Menu
          mode="horizontal"
          selectedKeys={[selectedKey]}
          items={NAV_ITEMS}
          onClick={({ key }) => navigate(key)}
          style={{ borderBottom: 'none', marginTop: 4 }}
        />
      </div>

      {isMock ? (
        <Alert
          type="warning"
          showIcon
          banner
          title="MOCK 编码器（未运行模型）"
          description="主服务正使用内置确定性伪向量（fingerprint = mock|dim=2048|v1），并未调用编码侧车或真实模型。此模式仅用于验证「导入 → 编码 → 入库 → 检索 → 前端」全链路，检索结果不具备语义含义。"
        />
      ) : null}

      <Layout.Content style={{ padding: 16, maxWidth: 1720, width: '100%', margin: '0 auto' }}>
        <Outlet />
      </Layout.Content>
    </Layout>
  );
}
