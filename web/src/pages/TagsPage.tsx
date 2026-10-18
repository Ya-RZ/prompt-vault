import { ClearOutlined, ReloadOutlined } from '@ant-design/icons';
import {
  App as AntdApp,
  Button,
  Card,
  Empty,
  Input,
  Pagination,
  Popconfirm,
  Space,
  Spin,
  Tag,
  Typography,
} from 'antd';
import { useCallback, useMemo, useState } from 'react';

import {
  cleanupTags,
  deleteTag,
  describeError,
  listTags,
  type TagItem,
  type TagListResponse,
} from '../api';
import { ErrorState } from '../components/ErrorState';
import { tagColor } from '../utils/tagColor';
import { useAsync } from '../hooks/useAsync';
import { formatNumber } from '../utils/format';

const { Text } = Typography;

/** /tags —— 标签管理：Tag 墙 + 单个删除 + 清理未使用标签 */
export function TagsPage() {
  const { message } = AntdApp.useApp();

  const [qInput, setQInput] = useState('');
  const [q, setQ] = useState('');
  const [page, setPage] = useState(1);
  // 后端 /api/tags 的 page_size 上限是 500，标签页默认一次看全
  const [pageSize, setPageSize] = useState(500);
  const [busyId, setBusyId] = useState<number | null>(null);
  const [cleaning, setCleaning] = useState(false);

  const loader = useCallback(
    (signal: AbortSignal) =>
      listTags({ page, page_size: pageSize, q: q || undefined }, signal),
    [page, pageSize, q],
  );

  const { data, error, loading, reload, setData } = useAsync<TagListResponse>(loader, [
    page,
    pageSize,
    q,
  ]);

  const items = useMemo(() => data?.items ?? [], [data]);
  const total = data?.total ?? 0;
  const unusedCount = useMemo(() => items.filter((item) => item.asset_count === 0).length, [items]);

  const handleSearch = (value: string) => {
    setQ(value.trim());
    setPage(1);
  };

  const handleDelete = async (item: TagItem) => {
    // 有引用的标签后端会返回 409，这里先给出明确提示，避免无谓请求
    if (item.asset_count > 0) {
      message.warning(`标签「${item.name}」仍被 ${item.asset_count} 个资产使用，无法删除`);
      return;
    }
    setBusyId(item.id);
    try {
      await deleteTag(item.id);
      message.success(`已删除标签「${item.name}」`);
      // 先从本地移除，再以服务端结果为准刷新
      setData((prev) =>
        prev
          ? {
              ...prev,
              items: prev.items.filter((tag) => tag.id !== item.id),
              total: Math.max(0, prev.total - 1),
            }
          : prev,
      );
      reload();
    } catch (cause) {
      const presentation = describeError(cause);
      if (presentation.code === 'CONFLICT') {
        message.warning(`标签「${item.name}」仍被资产引用，无法删除`);
      } else {
        message.error(`删除失败：${presentation.title}`);
      }
    } finally {
      setBusyId(null);
    }
  };

  const handleCleanup = async () => {
    setCleaning(true);
    try {
      const result = await cleanupTags();
      message.success(
        result.deleted > 0 ? `已清理 ${formatNumber(result.deleted)} 个未使用标签` : '没有未使用的标签',
      );
      reload();
    } catch (cause) {
      const presentation = describeError(cause);
      message.error(`清理失败：${presentation.title}`);
    } finally {
      setCleaning(false);
    }
  };

  return (
    <Space orientation="vertical" size={12} style={{ width: '100%' }}>
      <Card size="small" variant="outlined" styles={{ body: { padding: 12 } }}>
        <Space size={8} wrap>
          <Input.Search
            allowClear
            style={{ width: 240 }}
            placeholder="按名称过滤标签（q）"
            value={qInput}
            onChange={(event) => setQInput(event.target.value)}
            onSearch={handleSearch}
          />
          <Popconfirm
            title="清理未使用标签？"
            description="将删除所有没有被任何资产引用的标签，不可撤销。"
            okText="清理"
            cancelText="取消"
            onConfirm={handleCleanup}
          >
            <Button icon={<ClearOutlined />} loading={cleaning}>
              清理未使用标签
            </Button>
          </Popconfirm>
          <Button icon={<ReloadOutlined />} onClick={reload} loading={loading}>
            刷新
          </Button>
          <Text type="secondary">
            共 {formatNumber(total)} 个标签
            {items.length > 0 ? ` · 本页 ${unusedCount} 个未使用` : ''}
          </Text>
        </Space>
      </Card>

      {error ? (
        <ErrorState error={error} context="标签列表加载失败" onRetry={reload} />
      ) : loading && items.length === 0 ? (
        <div style={{ padding: 48, textAlign: 'center' }}>
          <Spin />
        </div>
      ) : items.length === 0 ? (
        <Card variant="outlined">
          <Empty description="还没有任何标签。可在资产详情或上传表单里添加标签。" />
        </Card>
      ) : (
        <Card size="small" variant="outlined" styles={{ body: { padding: 16 } }}>
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}>
            {items.map((item) => (
              <Tag
                key={item.id}
                style={{
                  fontSize: 13,
                  padding: '2px 8px',
                  marginInlineEnd: 0,
                  // 0 引用的标签用透明度区分，但仍然是彩色
                  opacity: item.asset_count > 0 ? 1 : 0.55,
                  ...tagColor(item.name),
                }}
                closable={busyId !== item.id}
                onClose={(event) => {
                  event.preventDefault();
                  void handleDelete(item);
                }}
              >
                {item.name}
                <Text type="secondary" style={{ marginLeft: 6, fontSize: 12 }}>
                  {formatNumber(item.asset_count)}
                </Text>
              </Tag>
            ))}
            {busyId !== null ? <Spin size="small" /> : null}
          </div>

          <Text type="secondary" style={{ display: 'block', marginTop: 12, fontSize: 12 }}>
            数字是该标签被引用的资产数；0 的标签可直接删除，点标签右侧 × 即可。
          </Text>
        </Card>
      )}

      <Pagination
        showSizeChanger
        pageSizeOptions={[100, 200, 500]}
        current={page}
        pageSize={pageSize}
        total={total}
        showTotal={(value) => `共 ${formatNumber(value)} 个标签`}
        onChange={(nextPage, nextPageSize) => {
          setPage(nextPageSize === pageSize ? nextPage : 1);
          setPageSize(nextPageSize);
        }}
        style={{ textAlign: 'right' }}
      />
    </Space>
  );
}
