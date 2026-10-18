import {
  AppstoreOutlined,
  ReloadOutlined,
  TableOutlined,
  UploadOutlined,
} from '@ant-design/icons';
import {
  App as AntdApp,
  Button,
  Card,
  Empty,
  Input,
  Masonry,
  Modal,
  Pagination,
  Segmented,
  Select,
  Space,
  Spin,
  Table,
  Tag,
  Typography,
  Upload,
  type TableProps,
  type UploadProps,
} from 'antd';
import { useCallback, useMemo, useState } from 'react';

import {
  describeError,
  fetchFacets,
  listAssets,
  uploadAsset,
  type Asset,
  type AssetKind,
  type AssetListResponse,
  type AssetSort,
  type FacetsResponse,
  type TriState,
} from '../api';
import { AssetCard } from '../components/AssetCard';
import { AssetDetailDrawer } from '../components/AssetDetailDrawer';
import { AssetUploadModal } from '../components/AssetUploadModal';
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

const KIND_OPTIONS: Array<{ value: AssetKind; label: string }> = [
  { value: 'image', label: 'image' },
  { value: 'video', label: 'video' },
  { value: 'other', label: 'other' },
];

/**
 * 瀑布流列数（需求 3：默认 4 列，随宽度变化）。
 * AntD 的断点为 xs<576 / sm≥576 / md≥768 / lg≥992 / xl≥1200 / xxl≥1600，
 * 对应到目标断点即：<600 约 1 列、<900 约 2 列、<1200 约 3 列、≥1200 4 列。
 */
const MASONRY_COLUMNS = { xs: 1, sm: 1, md: 2, lg: 3, xl: 4, xxl: 4, xxxl: 4 };

/** 三态筛选的 options 生成器（保留 TRI_OPTIONS 供筛选器复用） */
function triOptions(prefix: string) {
  return TRI_OPTIONS.map((option) => ({
    value: option.value,
    label: `${prefix}：${option.label}`,
  }));
}

/** /assets —— 资产库：筛选 + 瀑布流/表格双视图 + 上传 + 详情抽屉 */
export function AssetsPage() {
  const { message } = AntdApp.useApp();

  const [qInput, setQInput] = useState('');
  const [q, setQ] = useState('');
  /** 需求 7：tag 过滤器改为单选（原为多选 tags 模式） */
  const [tag, setTag] = useState<string | undefined>(undefined);
  const [kind, setKind] = useState<AssetKind | undefined>(undefined);
  const [nsfw, setNsfw] = useState<TriState>('all');
  const [encoded, setEncoded] = useState<TriState>('all');
  const [sort, setSort] = useState<AssetSort>('-added_at');
  const [view, setView] = useState<'grid' | 'table'>('grid');
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);
  const [revealedIds, setRevealedIds] = useState<number[]>([]);
  const [openAssetId, setOpenAssetId] = useState<number | null>(null);

  // 上传：uploadOpen 是选择文件的弹窗，metaOpen 是上传成功后填写信息的弹窗
  const [uploadOpen, setUploadOpen] = useState(false);
  const [uploading, setUploading] = useState(false);
  const [uploadedAsset, setUploadedAsset] = useState<Asset | null>(null);
  const [metaOpen, setMetaOpen] = useState(false);

  const loader = useCallback(
    (signal: AbortSignal) =>
      listAssets(
        {
          page,
          page_size: pageSize,
          q: q || undefined,
          tag,
          kind,
          nsfw,
          encoded,
          sort,
        },
        signal,
      ),
    [page, pageSize, q, tag, kind, nsfw, encoded, sort],
  );

  const { data, error, loading, reload } = useAsync<AssetListResponse>(loader, [
    page,
    pageSize,
    q,
    tag,
    kind,
    nsfw,
    encoded,
    sort,
  ]);

  // tag 选项来自 facets：详情页新增标签后重新拉一次即可出现（配合后端 facets 修复）
  // limit=200：后端默认只返回前 20 个分桶，放大到上限避免新标签排不进过滤器
  const facetsLoader = useCallback(
    (signal: AbortSignal) => fetchFacets('tag', signal, 200),
    [],
  );
  const { data: facets, reload: reloadFacets } = useAsync<FacetsResponse>(facetsLoader, []);

  const tagOptions = useMemo(() => {
    const options = (facets?.items ?? []).map((item) => ({
      value: item.value,
      label: `${item.value}（${item.count}）`,
    }));
    // 当前选中的标签可能已不在 facets 结果里（刚被删/刚新增），补一个占位避免显示空值
    if (tag && !options.some((option) => option.value === tag)) {
      options.unshift({ value: tag, label: tag });
    }
    return options;
  }, [facets, tag]);

  const items = useMemo(() => data?.items ?? [], [data]);
  const total = data?.total ?? 0;

  const handleReveal = useCallback((id: number) => {
    setRevealedIds((prev) => (prev.includes(id) ? prev : [...prev, id]));
  }, []);

  const handleOpen = useCallback((asset: Asset) => setOpenAssetId(asset.id), []);

  /** 列表数据变更（详情编辑 / 上传保存 / 删除）后，列表与 facet 一起刷新 */
  const handleMutated = useCallback(() => {
    reload();
    reloadFacets();
  }, [reload, reloadFacets]);

  const handleSearch = (value: string) => {
    setQ(value.trim());
    setPage(1);
  };

  /** 上传（multipart 字段 file）→ 202/201 拿到 AssetDto 后弹出填写信息表单 */
  const handleUpload: UploadProps['customRequest'] = ({ file, onSuccess, onError }) => {
    const selected = file as File;
    setUploading(true);
    uploadAsset(selected)
      .then((created) => {
        onSuccess?.(created);
        setUploadedAsset(created);
        setUploadOpen(false);
        setMetaOpen(true);
        // 资产已入库，先刷一次列表：用户即使跳过「填写信息」也能看到新资产
        reload();
      })
      .catch((cause: unknown) => {
        const presentation = describeError(cause);
        message.error(`上传失败：${presentation.title}`);
        onError?.(cause instanceof Error ? cause : new Error(presentation.title));
      })
      .finally(() => setUploading(false));
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
            alt="资产缩略图"
            blurred={record.nsfw && !revealedIds.includes(record.id)}
            onReveal={() => handleReveal(record.id)}
          />
        </div>
      ),
    },
    {
      title: '类型',
      dataIndex: 'kind',
      width: 90,
      render: (value: AssetKind) => (
        <Tag color={value === 'video' ? 'blue' : 'default'}>{value}</Tag>
      ),
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
            {(value ?? []).slice(0, 3).map((name) => (
              <Tag key={name} color="processing">
                {name}
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

  const masonryItems = useMemo(
    () =>
      items.map((asset) => ({
        key: asset.id,
        data: asset,
        children: (
          <AssetCard
            asset={asset}
            revealed={revealedIds.includes(asset.id)}
            onReveal={handleReveal}
            onOpen={handleOpen}
          />
        ),
      })),
    [items, revealedIds, handleReveal, handleOpen],
  );

  return (
    <Space orientation="vertical" size={12} style={{ width: '100%' }}>
      <Card size="small" variant="outlined" styles={{ body: { padding: 12 } }}>
        <Space size={8} wrap>
          <Button type="primary" icon={<UploadOutlined />} onClick={() => setUploadOpen(true)}>
            上传
          </Button>
          <Input.Search
            allowClear
            style={{ width: 260 }}
            placeholder="模糊搜索路径或提示词文本（q）"
            value={qInput}
            onChange={(event) => setQInput(event.target.value)}
            onSearch={handleSearch}
          />
          {/* 需求 7：单选 tag 过滤器，选项来自 /api/stats/facets?key=tag */}
          <Select
            allowClear
            showSearch
            optionFilterProp="label"
            style={{ minWidth: 200 }}
            placeholder="tag（单选）"
            value={tag}
            onChange={(value: string | undefined) => {
              setTag(value);
              setPage(1);
            }}
            options={tagOptions}
          />
          <Select
            allowClear
            style={{ width: 130 }}
            placeholder="kind"
            value={kind}
            onChange={setKind}
            options={KIND_OPTIONS}
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
              { value: 'grid', icon: <AppstoreOutlined />, label: '瀑布流' },
              { value: 'table', icon: <TableOutlined />, label: '表格' },
            ]}
          />
          <Button icon={<ReloadOutlined />} onClick={handleMutated} loading={loading}>
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
          <Empty description="没有符合条件的资产。可点「上传」直接添加，或到「作业」页执行一次导入。" />
        </Card>
      ) : view === 'grid' ? (
        // 需求 3：AntD Masonry 瀑布流（antd 6.6.6 提供，无需 CSS columns 兜底）
        <Masonry<Asset>
          items={masonryItems}
          columns={MASONRY_COLUMNS}
          gutter={16}
        />
      ) : (
        <Table<Asset>
          size="small"
          rowKey="id"
          columns={columns}
          dataSource={items}
          pagination={false}
          loading={loading}
          scroll={{ x: 1100 }}
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

      {/* 上传入口：拖拽到区域或点击选择文件（multipart 字段 file） */}
      <Modal
        open={uploadOpen}
        title="上传资产"
        footer={null}
        onCancel={() => setUploadOpen(false)}
        destroyOnHidden
      >
        <Upload.Dragger
          name="file"
          multiple={false}
          maxCount={1}
          showUploadList={false}
          customRequest={handleUpload}
          disabled={uploading}
        >
          <p className="ant-upload-drag-icon">
            <UploadOutlined style={{ fontSize: 28 }} />
          </p>
          <p className="ant-upload-text">把文件拖到这里，或点击选择文件</p>
          <p className="ant-upload-hint">上传成功后会弹出表单，填写标题、标签与提示词。</p>
        </Upload.Dragger>
        {uploading ? (
          <div style={{ marginTop: 12, textAlign: 'center' }}>
            <Space size={8}>
              <Spin size="small" />
              <Text type="secondary">上传中…</Text>
            </Space>
          </div>
        ) : null}
      </Modal>

      <AssetUploadModal
        asset={uploadedAsset}
        open={metaOpen}
        onClose={() => setMetaOpen(false)}
        onSaved={handleMutated}
      />

      <AssetDetailDrawer
        assetId={openAssetId}
        open={openAssetId !== null}
        onClose={() => setOpenAssetId(null)}
        onMutated={handleMutated}
      />
    </Space>
  );
}
