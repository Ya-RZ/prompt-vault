import { AppstoreOutlined, ReloadOutlined, TableOutlined } from '@ant-design/icons';
import {
  Button,
  Card,
  Empty,
  Input,
  Pagination,
  Segmented,
  Select,
  Space,
  Spin,
  Table,
  Tag,
  Tooltip,
  Typography,
  type TableProps,
} from 'antd';
import { useCallback, useState } from 'react';

import {
  listAssets,
  type Asset,
  type AssetKind,
  type AssetListResponse,
  type AssetSort,
  type TriState,
} from '../api';
import { AssetCard } from '../components/AssetCard';
import { AssetDetailDrawer } from '../components/AssetDetailDrawer';
import { CoverImage } from '../components/CoverImage';
import { ErrorState } from '../components/ErrorState';
import { useAsync } from '../hooks/useAsync';
import { formatBytes, formatDateTime, formatNumber } from '../utils/format';

const { Text } = Typography;

const SORT_OPTIONS: Array<{ value: AssetSort; label: string }> = [
  { value: '-added_at', label: '最新加入' },
  { value: 'added_at', label: '最早加入' },
  { value: '-bytes', label: '体积从大到小' },
  { value: 'bytes', label: '体积从小到大' },
  { value: '-id', label: 'ID 倒序' },
  { value: 'id', label: 'ID 正序' },
];

const TRI_OPTIONS: Array<{ value: TriState; label: string }> = [
  { value: 'all', label: '全部' },
  { value: 'true', label: '仅 true' },
  { value: 'false', label: '仅 false' },
];

/** 三态筛选的 options 生成器（保留 TRI_OPTIONS 供筛选器复用） */
function triOptions(prefix: string) {
  return TRI_OPTIONS.map((option) => ({
    value: option.value,
    label: `${prefix}：${option.label}`,
  }));
}

/** /assets —— 资产库：筛选 + 网格/表格双视图 + 详情抽屉 */
export function AssetsPage() {
  const [qInput, setQInput] = useState('');
  const [q, setQ] = useState('');
  const [tags, setTags] = useState<string[]>([]);
  const [kind, setKind] = useState<AssetKind | undefined>(undefined);
  const [nsfw, setNsfw] = useState<TriState>('all');
  const [encoded, setEncoded] = useState<TriState>('all');
  const [sort, setSort] = useState<AssetSort>('-added_at');
  const [view, setView] = useState<'grid' | 'table'>('grid');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);
  const [revealedIds, setRevealedIds] = useState<number[]>([]);
  const [openAssetId, setOpenAssetId] = useState<number | null>(null);

  const loader = useCallback(
    (signal: AbortSignal) =>
      listAssets(
        {
          page,
          page_size: pageSize,
          q: q || undefined,
          tag: tags.length > 0 ? tags : undefined,
          kind,
          nsfw,
          encoded,
          sort,
        },
        signal,
      ),
    [page, pageSize, q, tags, kind, nsfw, encoded, sort],
  );

  const { data, error, loading, reload } = useAsync<AssetListResponse>(loader, [
    page,
    pageSize,
    q,
    tags,
    kind,
    nsfw,
    encoded,
    sort,
  ]);

  const items = data?.items ?? [];
  const total = data?.total ?? 0;

  const handleReveal = (id: number) => setRevealedIds((prev) => [...prev, id]);

  const handleSearch = (value: string) => {
    setQ(value.trim());
    setPage(1);
  };

  const columns: TableProps<Asset>['columns'] = [
    {
      title: '预览',
      dataIndex: 'thumb_url',
      width: 82,
      render: (_value, record) => (
        <div style={{ width: 64 }}>
          <CoverImage
            src={record.thumb_url}
            alt={record.rel_path}
            blurred={record.nsfw && !revealedIds.includes(record.id)}
            onReveal={() => handleReveal(record.id)}
          />
        </div>
      ),
    },
    { title: 'ID', dataIndex: 'id', width: 76 },
    {
      title: '路径',
      dataIndex: 'rel_path',
      ellipsis: true,
      render: (value: string) => <Tooltip title={value}>{value}</Tooltip>,
    },
    {
      title: '类型',
      dataIndex: 'kind',
      width: 90,
      render: (value: AssetKind) => <Tag color={value === 'video' ? 'blue' : 'default'}>{value}</Tag>,
    },
    {
      title: '尺寸',
      key: 'dimension',
      width: 110,
      render: (_value, record) =>
        record.width && record.height ? `${record.width}×${record.height}` : '—',
    },
    {
      title: '体积',
      dataIndex: 'bytes',
      width: 100,
      render: (value: number) => formatBytes(value),
    },
    {
      title: 'NSFW',
      dataIndex: 'nsfw',
      width: 80,
      render: (value: boolean) => (value ? <Tag color="red">是</Tag> : <Tag>否</Tag>),
    },
    {
      title: '编码',
      key: 'encoded',
      width: 90,
      render: (_value, record) =>
        record.vector?.encoded ? <Tag color="green">已编码</Tag> : <Tag>未编码</Tag>,
    },
    {
      title: '标签',
      dataIndex: 'tags',
      width: 200,
      render: (value: string[] | undefined) =>
        (value ?? []).length === 0 ? (
          <Text type="secondary">—</Text>
        ) : (
          <Space size={4} wrap>
            {(value ?? []).slice(0, 3).map((tag) => (
              <Tag key={tag} color="processing">
                {tag}
              </Tag>
            ))}
            {(value ?? []).length > 3 ? <Text type="secondary">+{(value ?? []).length - 3}</Text> : null}
          </Space>
        ),
    },
    {
      title: '加入时间',
      dataIndex: 'added_at',
      width: 170,
      render: (value: string) => formatDateTime(value),
    },
    {
      title: '操作',
      key: 'action',
      width: 90,
      render: (_value, record) => (
        <Button size="small" type="link" onClick={() => setOpenAssetId(record.id)}>
          详情
        </Button>
      ),
    },
  ];

  return (
    <Space orientation="vertical" size={12} style={{ width: '100%' }}>
      <Card size="small" variant="outlined" styles={{ body: { padding: 12 } }}>
        <Space size={8} wrap>
          <Input.Search
            allowClear
            style={{ width: 260 }}
            placeholder="模糊搜索路径或提示词文本（q）"
            value={qInput}
            onChange={(event) => setQInput(event.target.value)}
            onSearch={handleSearch}
          />
          <Select
            mode="tags"
            style={{ minWidth: 190 }}
            placeholder="tag（可多个）"
            value={tags}
            onChange={setTags}
            options={tags.map((tag) => ({ value: tag, label: tag }))}
          />
          <Select
            allowClear
            style={{ width: 130 }}
            placeholder="kind"
            value={kind}
            onChange={setKind}
            options={[
              { value: 'image', label: 'image' },
              { value: 'video', label: 'video' },
            ]}
          />
          <Select
            style={{ width: 130 }}
            value={nsfw}
            onChange={setNsfw}
            options={triOptions('nsfw')}
          />
          <Select
            style={{ width: 150 }}
            value={encoded}
            onChange={setEncoded}
            options={triOptions('encoded')}
          />
          <Select
            style={{ width: 170 }}
            value={sort}
            onChange={setSort}
            options={SORT_OPTIONS}
          />
          <Segmented
            value={view}
            onChange={(value) => setView(value as 'grid' | 'table')}
            options={[
              { value: 'grid', icon: <AppstoreOutlined />, label: '网格' },
              { value: 'table', icon: <TableOutlined />, label: '表格' },
            ]}
          />
          <Button icon={<ReloadOutlined />} onClick={reload} loading={loading}>
            刷新
          </Button>
          <Text type="secondary">
            共 {formatNumber(total)} 条 · 第 {data?.page ?? page} 页
          </Text>
        </Space>
      </Card>

      {error ? (
        <ErrorState error={error} context="资产列表加载失败" onRetry={reload} />
      ) : loading && items.length === 0 ? (
        <div style={{ padding: 48, textAlign: 'center' }}>
          <Spin />
        </div>
      ) : items.length === 0 ? (
        <Card variant="outlined">
          <Empty description="没有符合条件的资产。若后端还没有数据，可到「作业」页先执行一次导入。" />
        </Card>
      ) : view === 'grid' ? (
        <div className="pv-grid">
          {items.map((asset) => (
            <AssetCard
              key={asset.id}
              asset={asset}
              revealed={revealedIds.includes(asset.id)}
              onReveal={handleReveal}
              onOpen={(opened) => setOpenAssetId(opened.id)}
            />
          ))}
        </div>
      ) : (
        <Table<Asset>
          size="small"
          rowKey="id"
          columns={columns}
          dataSource={items}
          pagination={false}
          loading={loading}
          scroll={{ x: 1180 }}
        />
      )}

      <Pagination
        showSizeChanger
        pageSizeOptions={[24, 50, 100, 200]}
        current={page}
        pageSize={pageSize}
        total={total}
        showTotal={(value) => `共 ${formatNumber(value)} 条`}
        onChange={(nextPage, nextPageSize) => {
          setPage(nextPageSize === pageSize ? nextPage : 1);
          setPageSize(nextPageSize);
        }}
        style={{ textAlign: 'right' }}
      />

      <AssetDetailDrawer
        assetId={openAssetId}
        open={openAssetId !== null}
        onClose={() => setOpenAssetId(null)}
        onMutated={reload}
      />
    </Space>
  );
}
