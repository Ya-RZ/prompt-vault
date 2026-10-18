import { ReloadOutlined } from '@ant-design/icons';
import {
  Button,
  Card,
  Col,
  Empty,
  Progress,
  Row,
  Space,
  Spin,
  Statistic,
  Tooltip,
  Typography,
} from 'antd';
import { useCallback } from 'react';

import {
  fetchSize,
  fetchStats,
  type SizeResponse,
  type StatsResponse,
} from '../api';
import { ErrorState } from '../components/ErrorState';
import { useAsync } from '../hooks/useAsync';
import { formatBytes, formatNumber, safePercent } from '../utils/format';

const { Text } = Typography;

/** percent 优先用后端值；缺失/非法时按 encoded/total 兜底，永不出 NaN */
function clampPercent(
  value: number | null | undefined,
  part: number | null | undefined,
  total: number | null | undefined,
): number {
  if (typeof value === 'number' && Number.isFinite(value)) {
    return Math.min(100, Math.max(0, value));
  }
  return safePercent(part ?? 0, total ?? 0);
}

function StatCard({
  title,
  value,
  suffix,
  hint,
  valueColor,
}: {
  title: string;
  value: string;
  suffix?: string;
  hint?: string;
  valueColor?: string;
}) {
  const content = (
    <Card size="small" variant="outlined">
      <Statistic
        title={title}
        value={value}
        suffix={suffix}
        valueStyle={valueColor ? { color: valueColor } : undefined}
      />
    </Card>
  );
  return hint ? <Tooltip title={hint}>{content}</Tooltip> : content;
}

/** 资产类型 → 中文名（按类型卡片用） */
const KIND_LABEL: Record<string, string> = {
  image: '图片',
  video: '视频',
  other: '其他',
};

/** /stats —— 总量、编码进度、体积、按类型 */
export function StatsPage() {
  const statsLoader = useCallback((signal: AbortSignal) => fetchStats(signal), []);
  const sizeLoader = useCallback((signal: AbortSignal) => fetchSize(signal), []);

  const {
    data: stats,
    error: statsError,
    loading: statsLoading,
    reload: reloadStats,
  } = useAsync<StatsResponse>(statsLoader, []);

  const {
    data: size,
    error: sizeError,
    loading: sizeLoading,
    reload: reloadSize,
  } = useAsync<SizeResponse>(sizeLoader, []);

  const reloadAll = () => {
    reloadStats();
    reloadSize();
  };

  return (
    <Space orientation="vertical" size={12} style={{ width: '100%' }}>
      <Card size="small" variant="outlined" styles={{ body: { padding: 12 } }}>
        <Space size={12} wrap>
          <Button icon={<ReloadOutlined />} onClick={reloadAll} loading={statsLoading || sizeLoading}>
            刷新
          </Button>
          <Text type="secondary">数据来自主服务的统计接口</Text>
        </Space>
      </Card>

      {statsError ? (
        <ErrorState error={statsError} context="统计数据加载失败" onRetry={reloadStats} />
      ) : statsLoading && !stats ? (
        <div style={{ padding: 48, textAlign: 'center' }}>
          <Spin />
        </div>
      ) : stats ? (
        <>
          {/* 总量卡片 */}
          <Row gutter={[12, 12]}>
            <Col xs={24} sm={12} lg={6}>
              <StatCard
                title="资产总数"
                value={formatNumber(stats.assets?.total)}
                hint={`图片 ${formatNumber(stats.assets?.images)} · 视频 ${formatNumber(
                  stats.assets?.videos,
                )} · NSFW ${formatNumber(stats.assets?.nsfw)}`}
              />
            </Col>
            <Col xs={24} sm={12} lg={6}>
              <StatCard
                title="提示词总数"
                value={formatNumber(stats.prompts?.total)}
                hint={`已编码 ${formatNumber(stats.prompts?.encoded)} · 待编码 ${formatNumber(
                  stats.prompts?.pending,
                )}`}
              />
            </Col>
            <Col xs={24} sm={12} lg={6}>
              <StatCard
                title="向量点数"
                value={formatNumber(stats.vectors?.points)}
                hint={`维度 ${formatNumber(stats.vectors?.dense_size)} · 量化 ${
                  stats.vectors?.quantization ?? '—'
                } · 稀疏路 ${stats.vectors?.sparse_enabled ? '可用' : '不可用'}`}
              />
            </Col>
            <Col xs={24} sm={12} lg={6}>
              <StatCard
                title="媒体体积"
                value={formatBytes(stats.assets?.bytes)}
                hint={`向量占用 ${formatBytes(stats.vectors?.disk_bytes)}`}
              />
            </Col>
          </Row>

          {/* 编码进度 + 体积（两者都不需要占满整行，各给 1/3） */}
          <Row gutter={[12, 12]}>
            <Col xs={24} sm={12} lg={8}>
              <Card size="small" variant="outlined" title="编码进度">
                <Space orientation="vertical" size={8} style={{ width: '100%' }}>
                  <Progress
                    percent={clampPercent(
                      stats.encode_progress?.percent,
                      stats.encode_progress?.encoded,
                      stats.encode_progress?.total,
                    )}
                    status={
                      stats.encode_progress && stats.encode_progress.total > 0 &&
                      stats.encode_progress.encoded >= stats.encode_progress.total
                        ? 'success'
                        : 'normal'
                    }
                  />
                  <Text type="secondary">
                    已编码 {formatNumber(stats.encode_progress?.encoded)} / 共{' '}
                    {formatNumber(stats.encode_progress?.total)}（提示词向量）
                  </Text>
                  {stats.encode_progress?.total === 0 ? (
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      当前没有需要编码的提示词，进度按 0% 显示。
                    </Text>
                  ) : null}
                </Space>
              </Card>
            </Col>
            <Col xs={24} sm={12} lg={8}>
              <Card size="small" variant="outlined" title="磁盘占用">
                {sizeError ? (
                  <ErrorState error={sizeError} context="体积统计加载失败" onRetry={reloadSize} />
                ) : sizeLoading && !size ? (
                  <Spin />
                ) : size ? (
                  <Space orientation="vertical" size={4} style={{ width: '100%' }}>
                    <Text>媒体：{formatBytes(size.media_bytes)}</Text>
                    <Text>向量：{formatBytes(size.vector_bytes)}</Text>
                    <Text>
                      SQLite：{formatBytes(size.db_bytes)}
                      {size.wal_bytes ? `（WAL ${formatBytes(size.wal_bytes)}）` : ''}
                    </Text>
                    <Text strong>合计：{formatBytes(size.total_bytes)}</Text>
                  </Space>
                ) : (
                  <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="暂无数据" />
                )}
              </Card>
            </Col>
          </Row>

          {/* 按类型（卡片形式） */}
          <Row gutter={[12, 12]}>
            {(stats.by_kind ?? []).length === 0 ? (
              <Col span={24}>
                <Card size="small" variant="outlined" title="按类型">
                  <Text type="secondary">还没有资产</Text>
                </Card>
              </Col>
            ) : (
              (stats.by_kind ?? []).map((item) => (
                <Col xs={12} sm={8} lg={6} key={item.key}>
                  <StatCard
                    title={KIND_LABEL[item.key] ?? item.key}
                    value={formatNumber(item.count)}
                    hint={`占全部资产 ${safePercent(item.count, stats.assets?.total ?? 0).toFixed(1)}%`}
                  />
                </Col>
              ))
            )}
          </Row>

        </>
      ) : (
        <Card variant="outlined">
          <Empty description="没有统计数据" />
        </Card>
      )}
    </Space>
  );
}
