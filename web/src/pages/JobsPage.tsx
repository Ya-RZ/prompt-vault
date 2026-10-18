import { PlusOutlined, ReloadOutlined, WarningOutlined } from '@ant-design/icons';
import {
  Alert,
  App as AntdApp,
  Button,
  Card,
  Drawer,
  Empty,
  Form,
  Input,
  InputNumber,
  Modal,
  Pagination,
  Popconfirm,
  Progress,
  Select,
  Space,
  Spin,
  Switch,
  Table,
  Tag,
  Tooltip,
  Typography,
  type TableProps,
} from 'antd';
import { useCallback, useEffect, useState } from 'react';

import {
  cancelJob,
  createEncodeJob,
  createImportJob,
  deleteJob,
  describeError,
  listJobErrors,
  listJobs,
  pauseJob,
  resumeJob,
  type EncodeJobRequest,
  type ImportJobRequest,
  type Job,
  type JobErrorItem,
  type JobErrorListResponse,
  type JobStatus,
  type Paged,
} from '../api';
import { ErrorState } from '../components/ErrorState';
import { useAsync } from '../hooks/useAsync';
import {
  formatDateTime,
  formatDuration,
  formatNumber,
  formatRate,
  safePercent,
} from '../utils/format';

const { Text } = Typography;

const DEFAULT_EXTENSIONS = ['jpg', 'jpeg', 'png', 'webp', 'gif', 'mp4'];

interface ImportFormValues {
  root: string;
  recursive: boolean;
  extensions: string[];
  default_prompt_role?: string;
  dedupe_by?: string;
}

interface EncodeFormValues {
  limited: boolean;
  limit?: number | null;
  only_missing: boolean;
  batch_size: number;
}

function statusColor(status: JobStatus): string {
  switch (status) {
    case 'running':
      return 'processing';
    case 'paused':
      return 'warning';
    case 'succeeded':
      return 'success';
    case 'failed':
      return 'error';
    case 'cancelled':
    default:
      return 'default';
  }
}

/**
 * 进度百分比。
 * 关键：total 为 0 时不能算出 NaN —— 优先用后端给的 percent，取不到就按 done/total 兜底，
 * 两者都不可用时返回 0。
 */
function jobPercent(job: Job): number {
  const fromApi = job.progress?.percent;
  if (typeof fromApi === 'number' && Number.isFinite(fromApi)) {
    return Math.min(100, Math.max(0, fromApi));
  }
  return safePercent(job.done, job.total);
}

/** 作业错误明细抽屉 */
function JobErrorsDrawer({ job, onClose }: { job: Job | null; onClose: () => void }) {
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(20);
  const [data, setData] = useState<JobErrorListResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const jobId = job?.id ?? null;

  useEffect(() => {
    setPage(1);
  }, [jobId]);

  useEffect(() => {
    if (jobId === null) {
      setData(null);
      setError(null);
      return;
    }
    let active = true;
    setLoading(true);
    setError(null);
    listJobErrors(jobId, { page, page_size: pageSize })
      .then((response) => {
        if (active) setData(response);
      })
      .catch((cause: unknown) => {
        if (active) setError(cause);
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [jobId, page, pageSize]);

  const columns: TableProps<JobErrorItem>['columns'] = [
    { title: '相对路径', dataIndex: 'rel_path', ellipsis: true },
    {
      title: '错误码',
      dataIndex: 'code',
      width: 180,
      render: (value: string) => <Tag color="red">{value}</Tag>,
    },
    { title: '说明', dataIndex: 'message', ellipsis: true },
    {
      title: '时间',
      dataIndex: 'at',
      width: 180,
      render: (value: string) => formatDateTime(value),
    },
  ];

  return (
    <Drawer open={job !== null} size={820} onClose={onClose} title={job ? `作业错误：#${job.id}` : '作业错误'}>
      {error ? (
        <ErrorState error={error} context="错误明细加载失败" />
      ) : loading && !data ? (
        <Spin />
      ) : (data?.items?.length ?? 0) === 0 ? (
        <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="该作业没有错误记录" />
      ) : (
        <Space orientation="vertical" size={12} style={{ width: '100%' }}>
          <Table<JobErrorItem>
            size="small"
            rowKey={(record) => `${record.rel_path}-${record.at}-${record.code}`}
            columns={columns}
            dataSource={data?.items ?? []}
            pagination={false}
            loading={loading}
            scroll={{ x: 760 }}
          />
          <Pagination
            current={page}
            pageSize={pageSize}
            total={data?.total ?? 0}
            showSizeChanger
            pageSizeOptions={[20, 50, 100, 200]}
            showTotal={(total) => `共 ${formatNumber(total)} 条错误`}
            onChange={(nextPage, nextSize) => {
              setPage(nextSize === pageSize ? nextPage : 1);
              setPageSize(nextSize);
            }}
          />
        </Space>
      )}
    </Drawer>
  );
}

/** /jobs —— 作业：列表、进度、速率/ETA、暂停恢复取消、新建导入与编码 */
export function JobsPage() {
  const { message } = AntdApp.useApp();
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(20);

  const [importOpen, setImportOpen] = useState(false);
  const [encodeOpen, setEncodeOpen] = useState(false);
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<unknown>(null);
  const [errorJob, setErrorJob] = useState<Job | null>(null);

  const [importForm] = Form.useForm<ImportFormValues>();
  const [encodeForm] = Form.useForm<EncodeFormValues>();

  const loader = useCallback(
    (signal: AbortSignal) => listJobs({ page, page_size: pageSize }, signal),
    [page, pageSize],
  );
  const { data, error, loading, reload } = useAsync<Paged<Job>>(loader, [page, pageSize]);

  const items = data?.items ?? [];
  const hasRunning = items.some((job) => job.status === 'running');

  // 有运行中的作业时轮询刷新（3s），否则不打扰后端
  useEffect(() => {
    if (!hasRunning) return undefined;
    const timer = window.setInterval(() => reload(), 3000);
    return () => window.clearInterval(timer);
  }, [hasRunning, reload]);

  const runAction = async (job: Job, action: 'pause' | 'resume' | 'cancel') => {
    try {
      if (action === 'pause') await pauseJob(job.id);
      else if (action === 'resume') await resumeJob(job.id);
      else await cancelJob(job.id);
      message.success(`已${action === 'pause' ? '暂停' : action === 'resume' ? '恢复' : '取消'}作业 #${job.id}`);
      reload();
    } catch (cause) {
      const presentation = describeError(cause);
      message.error(`操作失败：${presentation.title}`);
    }
  };

  /** 删除作业：仅 failed/succeeded/cancelled 可删，运行中后端返回 409 CONFLICT */
  const handleDeleteJob = async (job: Job) => {
    try {
      await deleteJob(job.id);
      message.success(`已删除作业 #${job.id}`);
      reload();
    } catch (cause) {
      const presentation = describeError(cause);
      if (presentation.code === 'CONFLICT') {
        message.warning('该作业仍在运行或排队，请先取消后再删除');
      } else {
        message.error(`删除失败：${presentation.title}`);
      }
    }
  };

  const handleCreateImport = async (values: ImportFormValues) => {
    setSubmitting(true);
    setSubmitError(null);
    try {
      const payload: ImportJobRequest = {
        root: values.root.trim(),
        recursive: values.recursive ?? true,
        extensions: values.extensions && values.extensions.length > 0 ? values.extensions : DEFAULT_EXTENSIONS,
        default_prompt_role: values.default_prompt_role?.trim() || 'caption',
        dedupe_by: values.dedupe_by?.trim() || 'sha256',
      };
      const job = await createImportJob(payload);
      message.success(`导入作业已创建：#${job.id}`);
      setImportOpen(false);
      importForm.resetFields();
      reload();
    } catch (cause) {
      setSubmitError(cause);
    } finally {
      setSubmitting(false);
    }
  };

  const handleCreateEncode = async (values: EncodeFormValues) => {
    setSubmitting(true);
    setSubmitError(null);
    try {
      const payload: EncodeJobRequest = {
        limit: values.limited ? (values.limit ?? null) : null,
        only_missing: values.only_missing ?? true,
        batch_size: values.batch_size ?? 8,
        asset_ids: null,
        prompt_ids: null,
      };
      const job = await createEncodeJob(payload);
      message.success(`编码作业已创建：#${job.id}`);
      setEncodeOpen(false);
      encodeForm.resetFields();
      reload();
    } catch (cause) {
      setSubmitError(cause);
    } finally {
      setSubmitting(false);
    }
  };

  const columns: TableProps<Job>['columns'] = [
    {
      title: '作业',
      dataIndex: 'id',
      width: 190,
      render: (value: string, record) => (
        <Space orientation="vertical" size={0}>
          <Text className="pv-header-fingerprint">{value}</Text>
          <Text type="secondary" style={{ fontSize: 12 }}>
            {record.kind}
            {record.root ? ` · ${record.root}` : ''}
          </Text>
        </Space>
      ),
    },
    {
      title: '状态',
      dataIndex: 'status',
      width: 100,
      render: (value: JobStatus) => <Tag color={statusColor(value)}>{value}</Tag>,
    },
    {
      title: '进度',
      key: 'progress',
      width: 280,
      render: (_value, record) => {
        const percent = jobPercent(record);
        const finished = record.status === 'succeeded';
        const failed = record.status === 'failed';
        return (
          <Space orientation="vertical" size={2} style={{ width: '100%' }}>
            <Progress
              percent={percent}
              size="small"
              status={failed ? 'exception' : finished ? 'success' : 'normal'}
              format={(value) => `${(value ?? 0).toFixed(1)}%`}
            />
            <Text type="secondary" style={{ fontSize: 12 }}>
              done {formatNumber(record.done)} / total {formatNumber(record.total)} · 失败{' '}
              {formatNumber(record.failed)} · 跳过 {formatNumber(record.skipped)}
            </Text>
          </Space>
        );
      },
    },
    {
      title: '速率 / ETA',
      key: 'rate',
      width: 150,
      render: (_value, record) => (
        <Space orientation="vertical" size={0}>
          <Text style={{ fontSize: 12 }}>{formatRate(record.progress?.rate_per_minute)}</Text>
          <Text type="secondary" style={{ fontSize: 12 }}>
            ETA {formatDuration(record.progress?.eta_seconds)}
          </Text>
        </Space>
      ),
    },
    {
      title: '游标（断点）',
      dataIndex: 'cursor',
      ellipsis: true,
      render: (value: string | null) =>
        value ? (
          <Tooltip title={value}>
            <Text style={{ fontSize: 12 }}>{value}</Text>
          </Tooltip>
        ) : (
          <Text type="secondary">—</Text>
        ),
    },
    {
      title: '更新时间',
      dataIndex: 'updated_at',
      width: 170,
      render: (value: string, record) => (
        <Space orientation="vertical" size={0}>
          <Text style={{ fontSize: 12 }}>{formatDateTime(value)}</Text>
          {record.message ? (
            <Tooltip title={record.message}>
              <Text type="secondary" style={{ fontSize: 12 }}>
                {record.message.slice(0, 18)}
                {record.message.length > 18 ? '…' : ''}
              </Text>
            </Tooltip>
          ) : null}
        </Space>
      ),
    },
    {
      title: '操作',
      key: 'action',
      width: 270,
      render: (_value, record) => (
        <Space size={4} wrap>
          {record.status === 'running' ? (
            <Button size="small" onClick={() => void runAction(record, 'pause')}>
              暂停
            </Button>
          ) : null}
          {record.status === 'paused' ? (
            <Button size="small" type="primary" onClick={() => void runAction(record, 'resume')}>
              恢复
            </Button>
          ) : null}
          {record.status === 'running' || record.status === 'paused' ? (
            <Popconfirm
              title="取消该作业？"
              description="已完成的项不会被回滚，游标会保留。"
              okText="取消作业"
              cancelText="再想想"
              onConfirm={() => void runAction(record, 'cancel')}
            >
              <Button size="small" danger>
                取消
              </Button>
            </Popconfirm>
          ) : null}
          <Button
            size="small"
            type="link"
            icon={record.failed > 0 ? <WarningOutlined /> : undefined}
            onClick={() => setErrorJob(record)}
          >
            错误{record.failed > 0 ? `（${record.failed}）` : ''}
          </Button>
          {record.status === 'failed' ||
          record.status === 'succeeded' ||
          record.status === 'cancelled' ? (
            <Popconfirm
              title="删除该作业记录？"
              description="只删除作业与其错误明细，资产与文件不受影响。"
              okText="删除"
              okButtonProps={{ danger: true }}
              cancelText="取消"
              onConfirm={() => void handleDeleteJob(record)}
            >
              <Button size="small" danger type="link">
                删除
              </Button>
            </Popconfirm>
          ) : null}
        </Space>
      ),
    },
  ];

  return (
    <Space orientation="vertical" size={12} style={{ width: '100%' }}>
      <Card size="small" variant="outlined" styles={{ body: { padding: 12 } }}>
        <Space size={8} wrap>
          <Button
            type="primary"
            icon={<PlusOutlined />}
            onClick={() => {
              setSubmitError(null);
              setImportOpen(true);
            }}
          >
            新建导入作业
          </Button>
          <Button
            icon={<PlusOutlined />}
            onClick={() => {
              setSubmitError(null);
              setEncodeOpen(true);
            }}
          >
            新建编码作业
          </Button>
          <Button icon={<ReloadOutlined />} onClick={reload} loading={loading}>
            刷新
          </Button>
          {hasRunning ? (
            <Text type="secondary">有作业运行中，页面每 3 秒自动刷新。</Text>
          ) : (
            <Text type="secondary">共 {formatNumber(data?.total ?? 0)} 个作业。</Text>
          )}
        </Space>
      </Card>

      {error ? (
        <ErrorState error={error} context="作业列表加载失败" onRetry={reload} />
      ) : loading && items.length === 0 ? (
        <div style={{ padding: 48, textAlign: 'center' }}>
          <Spin />
        </div>
      ) : items.length === 0 ? (
        <Card variant="outlined">
          <Empty description="还没有作业。先「新建导入作业」把媒体目录扫进库，再「新建编码作业」补向量。" />
        </Card>
      ) : (
        <Table<Job>
          size="small"
          rowKey="id"
          columns={columns}
          dataSource={items}
          pagination={false}
          loading={loading}
          scroll={{ x: 1460 }}
        />
      )}

      <Pagination
        style={{ textAlign: 'right' }}
        current={page}
        pageSize={pageSize}
        total={data?.total ?? 0}
        showSizeChanger
        pageSizeOptions={[20, 50, 100, 200]}
        showTotal={(total) => `共 ${formatNumber(total)} 个作业`}
        onChange={(nextPage, nextSize) => {
          setPage(nextSize === pageSize ? nextPage : 1);
          setPageSize(nextSize);
        }}
      />

      {/* 新建导入作业 */}
      <Modal
        open={importOpen}
        title="新建导入作业"
        onCancel={() => setImportOpen(false)}
        onOk={() => importForm.submit()}
        confirmLoading={submitting}
        okText="创建"
        cancelText="取消"
        destroyOnHidden
      >
        <Space orientation="vertical" size={12} style={{ width: '100%' }}>
          <Alert
            type="info"
            showIcon
            title="导入前主服务会先做金标校验"
            description="未通过会返回 409 GOLDEN_FAILED 并拒绝创建作业；侧车不支持 /v1/verify 时按 unsupported 记录后继续。"
          />
          {submitError ? <ErrorState error={submitError} context="创建导入作业失败" /> : null}
          <Form<ImportFormValues>
            form={importForm}
            layout="vertical"
            initialValues={{
              recursive: true,
              extensions: DEFAULT_EXTENSIONS,
              default_prompt_role: 'caption',
              dedupe_by: 'sha256',
            }}
            onFinish={handleCreateImport}
          >
            <Form.Item name="root" label="根目录 root" rules={[{ required: true, message: '请输入根目录' }]}>
              <Input placeholder="例如 media 或 D:/pics" />
            </Form.Item>
            <Form.Item name="extensions" label="扩展名 extensions">
              <Select mode="tags" placeholder="回车添加，例如 jpg" options={DEFAULT_EXTENSIONS.map((ext) => ({ value: ext, label: ext }))} />
            </Form.Item>
            <Form.Item name="recursive" label="递归扫描子目录" valuePropName="checked">
              <Switch />
            </Form.Item>
            <Form.Item name="default_prompt_role" label="默认提示词角色 default_prompt_role">
              <Input placeholder="caption" />
            </Form.Item>
            <Form.Item name="dedupe_by" label="去重字段 dedupe_by">
              <Input placeholder="sha256" />
            </Form.Item>
          </Form>
        </Space>
      </Modal>

      {/* 新建编码作业 */}
      <Modal
        open={encodeOpen}
        title="新建编码作业"
        onCancel={() => setEncodeOpen(false)}
        onOk={() => encodeForm.submit()}
        confirmLoading={submitting}
        okText="创建"
        cancelText="取消"
        destroyOnHidden
      >
        <Space orientation="vertical" size={12} style={{ width: '100%' }}>
          {submitError ? <ErrorState error={submitError} context="创建编码作业失败" /> : null}
          <Form<EncodeFormValues>
            form={encodeForm}
            layout="vertical"
            initialValues={{ limited: false, limit: 100, only_missing: true, batch_size: 8 }}
            onFinish={handleCreateEncode}
          >
            <Form.Item name="only_missing" label="只编码缺失项 only_missing" valuePropName="checked">
              <Switch />
            </Form.Item>
            <Form.Item name="batch_size" label="批大小 batch_size" rules={[{ required: true }]}>
              <InputNumber min={1} max={512} style={{ width: 160 }} />
            </Form.Item>
            <Form.Item name="limited" label="限制本次编码数量" valuePropName="checked">
              <Switch />
            </Form.Item>
            <Form.Item noStyle shouldUpdate={(prev, next) => prev.limited !== next.limited}>
              {({ getFieldValue }) =>
                getFieldValue('limited') ? (
                  <Form.Item name="limit" label="limit（留空表示不限）">
                    <InputNumber min={1} style={{ width: 160 }} />
                  </Form.Item>
                ) : null
              }
            </Form.Item>
          </Form>
          <Text type="secondary" style={{ fontSize: 12 }}>
            契约 §6 还支持 asset_ids / prompt_ids 精确指定，本轮前端未提供入口（按 null 提交）。
          </Text>
        </Space>
      </Modal>

      <JobErrorsDrawer job={errorJob} onClose={() => setErrorJob(null)} />
    </Space>
  );
}
