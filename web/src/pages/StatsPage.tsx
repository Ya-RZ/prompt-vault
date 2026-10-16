import { ReloadOutlined } from '@ant-design/icons';
import {
  Alert,
  Button,
  Card,
  Col,
  Empty,
  Progress,
  Row,
  Segmented,
  Space,
  Spin,
  Statistic,
  Tag,
  Tooltip,
  Typography,
} from 'antd';
import { useCallback, useState } from 'react';

import {
  fetchFacets,
  fetchSize,
  fetchStats,
  type FacetBucket,
  type FacetKey,
  type FacetsResponse,
  type SizeResponse,
  type StatsResponse,
} from '../api';
import { DistributionBars } from '../components/DistributionBars';
import { ErrorState } from '../components/ErrorState';
import { useAsync } from '../hooks/useAsync';
import { formatBytes, formatNumber, safePercent } from '../utils/format';

const { Text } = Typography;

const FACET_OPTIONS: Array<{ value: FacetKey; label: string }> = [
  { value: 'tag', label: 'tag' },
  { value: 'kind', label: 'kind' },
  { value: 'nsfw', label: 'nsfw' },
  { value: 'month', label: 'month' },
  { value: 'source', label: 'source' },
];

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

/** /stats —— 总量、分布、编码进度、作业状态、体积 */
export function StatsPage() {
  const [facetKey, setFacetKey] = useState<FacetKey>('tag');
  const [facetData, setFacetData] = useState<FacetsResponse | null>(null);
  const [facetLoading, setFacetLoading] = useState(false);
  const [facetError, setFacetError] = useState<unknown>(null);

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

  const runFacetQuery = async (key: FacetKey) => {
    setFacetKey(key);
    setFacetLoading(true);
    setFacetError(null);
    try {
      const response = await fetchFacets(key);
      setFacetData(response);
    } catch (cause) {
      setFacetError(cause);
      setFacetData(null);
    } finally {
      setFacetLoading(false);
    }
  };

  const reloadAll = () => {
    reloadStats();
    reloadSize();
  };

  const statsBuckets = (items: FacetBucket[] | null | undefined): FacetBucket[] => items ?? [];

  return (
    <Space orientation="vertical" size={12} style={{ width: '100%' }}>
      <Card size="small" variant="outlined" styles={{ body: { padding: 12 } }}>
        <Space size={12} wrap>
          <Button icon={<ReloadOutlined />} onClick={reloadAll} loading={statsLoading || sizeLoading}>
            刷新
          </Button>
          <Text type="secondary">
            数据来自 GET /api/stats · GET /api/stats/facets · GET /api/stats/size
          </Text>
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

          {/* 编码进度 + 作业状态 + 体积 */}
          <Row gutter={[12, 12]}>
            <Col xs={24} lg={12}>
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
            <Col xs={24} lg={6}>
              <Card size="small" variant="outlined" title="作业状态">
                <Space orientation="vertical" size={8} style={{ width: '100%' }}>
                  <Space size={8}>
                    <Tag color="processing">running</Tag>
                    <Text strong>{formatNumber(stats.jobs?.running)}</Text>
                  </Space>
                  <Space size={8}>
                    <Tag color="success">succeeded</Tag>
                    <Text strong>{formatNumber(stats.jobs?.succeeded)}</Text>
                  </Space>
                  <Space size={8}>
                    <Tag color="error">failed</Tag>
                    <Text strong>{formatNumber(stats.jobs?.failed)}</Text>
                  </Space>
                  <Space size={8}>
                    <Tag>cancelled</Tag>
                    <Text strong>{formatNumber(stats.jobs?.cancelled)}</Text>
                  </Space>
                </Space>
              </Card>
            </Col>
            <Col xs={24} lg={6}>
              <Card size="small" variant="outlined" title="磁盘占用（/api/stats/size）">
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

          {/* 四组分布 */}
          <Row gutter={[12, 12]}>
            <Col xs={24} lg={12}>
              <Card size="small" variant="outlined" title={`按标签（by_tag，共 ${statsBuckets(stats.by_tag).length} 项）`}>
                <DistributionBars items={statsBuckets(stats.by_tag)} limit={12} emptyText="还没有标签" />
              </Card>
            </Col>
            <Col xs={24} lg={12}>
              <Card size="small" variant="outlined" title="按月份（by_month）">
                <DistributionBars items={statsBuckets(stats.by_month)} limit={12} emptyText="还没有数据" />
              </Card>
            </Col>
            <Col xs={24} lg={12}>
              <Card size="small" variant="outlined" title="按来源目录（by_source）">
                <DistributionBars items={statsBuckets(stats.by_source)} limit={12} emptyText="还没有数据" />
              </Card>
            </Col>
            <Col xs={24} lg={12}>
              <Card size="small" variant="outlined" title="按类型（by_kind）">
                <DistributionBars items={statsBuckets(stats.by_kind)} emptyText="还没有数据" />
              </Card>
            </Col>
          </Row>

          {/* facets 查询 */}
          <Card
            size="small"
            variant="outlined"
            title="Facet 查询（GET /api/stats/facets）"
            extra={
              <Space size={8}>
                <Segmented
                  value={facetKey}
                  onChange={(value) => void runFacetQuery(value as FacetKey)}
                  options={FACET_OPTIONS}
                />
                <Button size="small" loading={facetLoading} onClick={() => void runFacetQuery(facetKey)}>
                  查询
                </Button>
              </Space>
            }
          >
            {facetError ? (
              <ErrorState error={facetError} context="Facet 查询失败" />
            ) : facetData ? (
              <Space orientation="vertical" size={8} style={{ width: '100%' }}>
                <Alert
                  type="info"
                  showIcon
                  title={`key=${facetData.key} · 数据来源：${facetData.source}`}
                  description="source 为 qdrant-facet 表示由 Qdrant facet() 统计，其它值表示退回 SQL 聚合。"
                />
                <DistributionBars
                  items={(facetData.items ?? []).map((item) => ({ key: item.value, count: item.count }))}
                  limit={20}
                />
              </Space>
            ) : (
              <Text type="secondary">选择 key 后点击「查询」，或直接切换上方选项。</Text>
            )}
          </Card>
        </>
      ) : (
        <Card variant="outlined">
          <Empty description="没有统计数据" />
        </Card>
      )}
    </Space>
  );
}
