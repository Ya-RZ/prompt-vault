import { ReloadOutlined } from '@ant-design/icons';
import {
  App as AntdApp,
  Button,
  Card,
  Empty,
  Image,
  Pagination,
  Popconfirm,
  Space,
  Spin,
  Table,
  Tag,
  Typography,
  type TableProps,
} from 'antd';
import { useCallback, useState } from 'react';

import {
  clearTrash,
  describeError,
  listTrash,
  purgeTrash,
  restoreTrash,
  type TrashItem,
  type TrashListResponse,
} from '../api';
import { ErrorState } from '../components/ErrorState';
import { useAsync } from '../hooks/useAsync';
import { formatBytes, formatDateTime } from '../utils/format';

const { Text } = Typography;

/** /trash —— 回收站：恢复或彻底删除 */
export function TrashPage() {
  const { message } = AntdApp.useApp();
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(20);
  const [busyId, setBusyId] = useState<number | null>(null);
  const [clearing, setClearing] = useState(false);

  const loader = useCallback(
    (signal: AbortSignal) => listTrash(page, pageSize, signal),
    [page, pageSize],
  );
  const { data, error, loading, reload } = useAsync<TrashListResponse>(loader, [page, pageSize]);

  const items = data?.items ?? [];
  const total = data?.total ?? 0;

  const handleRestore = async (item: TrashItem) => {
    setBusyId(item.id);
    try {
      const result = await restoreTrash(item.id);
      message.success(`已恢复为资产 #${result.asset_id}（待编码）`);
      reload();
    } catch (cause) {
      message.error(describeError(cause).title);
    } finally {
      setBusyId(null);
    }
  };

  const handlePurge = async (item: TrashItem) => {
    setBusyId(item.id);
    try {
      const result = await purgeTrash(item.id);
      message.success(result.file_deleted ? '已彻底删除（含文件）' : '已彻底删除（文件已不存在）');
      reload();
    } catch (cause) {
      message.error(describeError(cause).title);
    } finally {
      setBusyId(null);
    }
  };

  const handleClear = async () => {
    setClearing(true);
    try {
      const result = await clearTrash();
      message.success(`已清空：${result.cleared} 条记录，${result.files_deleted} 个文件`);
      reload();
    } catch (cause) {
      message.error(describeError(cause).title);
    } finally {
      setClearing(false);
    }
  };

  const columns: TableProps<TrashItem>['columns'] = [
    {
      title: '预览',
      dataIndex: 'file_url',
      width: 88,
      render: (url: string) => (
        <Image
          src={url}
          width={64}
          height={64}
          style={{ objectFit: 'cover', borderRadius: 6 }}
          preview={false}
        />
      ),
    },
    {
      title: '原路径',
      dataIndex: 'rel_path',
      render: (value: string, record) => (
        <Space orientation="vertical" size={0}>
          <Text>{value}</Text>
          <Text type="secondary" style={{ fontSize: 12 }}>
            #{record.asset_id} · {record.hash?.slice(0, 16) ?? '无 hash'}
          </Text>
        </Space>
      ),
    },
    {
      title: '提示词',
      dataIndex: 'prompts',
      width: 90,
      render: (prompts: TrashItem['prompts']) => <Tag>{prompts.length}</Tag>,
    },
    {
      title: '大小',
      dataIndex: 'bytes',
      width: 100,
      render: (value: number) => formatBytes(value),
    },
    {
      title: '删除时间',
      dataIndex: 'deleted_at',
      width: 180,
      render: (value: string) => formatDateTime(value),
    },
    {
      title: '操作',
      key: 'actions',
      width: 180,
      render: (_value, record) => (
        <Space>
          <Button size="small" loading={busyId === record.id} onClick={() => handleRestore(record)}>
            恢复
          </Button>
          <Popconfirm
            title="彻底删除？"
            description="记录与文件都会被永久删除，不可恢复。"
            okText="删除"
            okButtonProps={{ danger: true }}
            cancelText="取消"
            onConfirm={() => handlePurge(record)}
          >
            <Button size="small" danger loading={busyId === record.id}>
              彻底删除
            </Button>
          </Popconfirm>
        </Space>
      ),
    },
  ];

  return (
    <Space orientation="vertical" size={12} style={{ width: '100%' }}>
      <Card size="small" variant="outlined" styles={{ body: { padding: 12 } }}>
        <Space size={12} wrap>
          <Button icon={<ReloadOutlined />} onClick={reload} loading={loading}>
            刷新
          </Button>
          <Text type="secondary">
            删除的资产先进入这里；导入/上传遇到回收站里的内容会自动跳过。目录：{data?.trash_dir ?? 'data/trash'}
          </Text>
          <Popconfirm
            title="清空回收站？"
            description="所有记录与文件都会被永久删除。"
            okText="清空"
            okButtonProps={{ danger: true }}
            cancelText="取消"
            onConfirm={handleClear}
          >
            <Button danger loading={clearing}>
              清空回收站
            </Button>
          </Popconfirm>
        </Space>
      </Card>

      {error ? (
        <ErrorState error={error} context="回收站加载失败" onRetry={reload} />
      ) : loading && !data ? (
        <Spin />
      ) : items.length === 0 ? (
        <Card size="small" variant="outlined">
          <Empty description="回收站为空" />
        </Card>
      ) : (
        <Card size="small" variant="outlined" styles={{ body: { padding: 0 } }}>
          <Table
            rowKey="id"
            size="small"
            dataSource={items}
            columns={columns}
            pagination={false}
          />
        </Card>
      )}

      <Space style={{ justifyContent: 'flex-end', width: '100%' }}>
        <Text type="secondary">共 {total} 条</Text>
        <Pagination
          current={page}
          pageSize={pageSize}
          total={total}
          showSizeChanger
          pageSizeOptions={[10, 20, 50]}
          onChange={(nextPage, nextSize) => {
            setPage(nextPage);
            setPageSize(nextSize ?? 20);
          }}
        />
      </Space>
    </Space>
  );
}
