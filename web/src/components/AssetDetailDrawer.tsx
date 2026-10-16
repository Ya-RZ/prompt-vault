import {
  App as AntdApp,
  Button,
  Descriptions,
  Divider,
  Drawer,
  Empty,
  Input,
  Popconfirm,
  Select,
  Space,
  Spin,
  Switch,
  Tabs,
  Tag,
  Tooltip,
  Typography,
} from 'antd';
import { useCallback, useEffect, useState } from 'react';

import {
  createAssetPrompt,
  deleteAsset,
  describeError,
  getAsset,
  getSimilarAssets,
  updateAssetMeta,
  type Asset,
  type SearchHit,
} from '../api';
import { useAsync } from '../hooks/useAsync';
import { formatBytes, formatDateTime, formatNumber, shortFingerprint } from '../utils/format';
import { CoverImage } from './CoverImage';
import { ErrorState } from './ErrorState';

const { Text, Paragraph } = Typography;

export interface AssetDetailDrawerProps {
  assetId: number | null;
  open: boolean;
  onClose: () => void;
  /** 元数据/提示词/删除等写操作成功后通知外层刷新列表 */
  onMutated: () => void;
}

/** 相似资产结果（结构同 /api/search/image 的 items） */
function SimilarGrid({ items }: { items: SearchHit[] }) {
  const [revealed, setRevealed] = useState<number[]>([]);

  if (items.length === 0) {
    return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="没有相似资产" />;
  }

  return (
    <div className="pv-grid" style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(140px, 1fr))' }}>
      {items.map((hit) => (
        <div key={`${hit.asset.id}-${hit.matched?.prompt_id ?? 'x'}`}>
          <CoverImage
            src={hit.asset.thumb_url}
            alt={hit.asset.rel_path}
            blurred={Boolean(hit.asset.nsfw) && !revealed.includes(hit.asset.id)}
            onReveal={() => setRevealed((prev) => [...prev, hit.asset.id])}
          />
          <div className="pv-meta-line" style={{ marginTop: 4 }} title={hit.asset.rel_path}>
            #{hit.asset.id} {hit.asset.rel_path}
          </div>
          <div className="pv-meta-line">score {hit.score?.toFixed(4) ?? '—'}</div>
        </div>
      ))}
    </div>
  );
}

export function AssetDetailDrawer({ assetId, open, onClose, onMutated }: AssetDetailDrawerProps) {
  const { message } = AntdApp.useApp();
  const [saving, setSaving] = useState(false);
  const [revealed, setRevealed] = useState(false);

  // 元数据编辑的本地草稿
  const [tagDraft, setTagDraft] = useState<string[]>([]);
  const [pathDraft, setPathDraft] = useState('');

  // 新增提示词
  const [promptText, setPromptText] = useState('');
  const [promptRole, setPromptRole] = useState('caption');
  const [promptEncode, setPromptEncode] = useState(true);

  // 相似资产
  const [similar, setSimilar] = useState<SearchHit[] | null>(null);
  const [similarLoading, setSimilarLoading] = useState(false);
  const [similarError, setSimilarError] = useState<unknown>(null);

  const enabled = open && assetId !== null;
  const loader = useCallback(
    (signal: AbortSignal) => {
      if (assetId === null) return Promise.reject(new Error('未选择资产'));
      return getAsset(assetId, signal);
    },
    [assetId],
  );
  const { data: asset, error, loading, reload, setData } = useAsync<Asset>(loader, [assetId], {
    enabled,
  });

  // 打开/切换资产时重置本地状态
  useEffect(() => {
    setRevealed(false);
    setSimilar(null);
    setSimilarError(null);
    setPromptText('');
    setPromptEncode(true);
  }, [assetId]);

  useEffect(() => {
    if (!asset) return;
    setTagDraft(asset.tags ?? []);
    setPathDraft(asset.rel_path ?? '');
  }, [asset]);

  const handleError = (cause: unknown, fallback: string) => {
    const presentation = describeError(cause);
    message.error(`${fallback}：${presentation.title}`);
  };

  const applyUpdatedAsset = (updated: Asset) => {
    setData(updated);
    setTagDraft(updated.tags ?? []);
    setPathDraft(updated.rel_path ?? '');
    onMutated();
  };

  const handleToggleNsfw = async (next: boolean) => {
    if (assetId === null) return;
    setSaving(true);
    try {
      const updated = await updateAssetMeta(assetId, { nsfw: next });
      applyUpdatedAsset(updated);
      message.success(next ? '已标记为 NSFW' : '已取消 NSFW 标记');
    } catch (cause) {
      handleError(cause, '更新 NSFW 失败');
    } finally {
      setSaving(false);
    }
  };

  const handleSaveMeta = async () => {
    if (assetId === null) return;
    setSaving(true);
    try {
      const updated = await updateAssetMeta(assetId, {
        tags: tagDraft,
        rel_path: pathDraft.trim() === '' ? undefined : pathDraft.trim(),
      });
      applyUpdatedAsset(updated);
      message.success('元数据已保存');
    } catch (cause) {
      handleError(cause, '保存元数据失败');
    } finally {
      setSaving(false);
    }
  };

  const handleAddPrompt = async () => {
    if (assetId === null) return;
    const text = promptText.trim();
    if (!text) {
      message.warning('提示词内容不能为空');
      return;
    }
    setSaving(true);
    try {
      await createAssetPrompt(assetId, { text, role: promptRole.trim() || 'caption', encode: promptEncode });
      setPromptText('');
      message.success(promptEncode ? '提示词已添加并编码' : '提示词已添加');
      reload();
      onMutated();
    } catch (cause) {
      // 契约 §4：encode 失败时提示词已落库，可用 /api/jobs 补编码
      const presentation = describeError(cause);
      message.warning(`提示词可能已落库，但编码失败：${presentation.title}`);
      reload();
      onMutated();
    } finally {
      setSaving(false);
    }
  };

  const handleDelete = async () => {
    if (assetId === null) return;
    setSaving(true);
    try {
      const result = await deleteAsset(assetId);
      message.success(
        `已删除 #${result.id}（向量：${result.vector_deleted ? '已删' : '未删'}，文件：${
          result.file_deleted ? '已删' : '保留'
        }）`,
      );
      onMutated();
      onClose();
    } catch (cause) {
      handleError(cause, '删除失败');
    } finally {
      setSaving(false);
    }
  };

  const handleLoadSimilar = async () => {
    if (assetId === null) return;
    setSimilarLoading(true);
    setSimilarError(null);
    try {
      const response = await getSimilarAssets(assetId, 12, 'dense');
      setSimilar(response.items ?? []);
    } catch (cause) {
      setSimilarError(cause);
    } finally {
      setSimilarLoading(false);
    }
  };

  const blurred = Boolean(asset?.nsfw) && !revealed;

  const overview = asset ? (
    <Space orientation="vertical" size={16} style={{ width: '100%' }}>
      <div style={{ maxWidth: 360 }}>
        <CoverImage
          src={asset.media_url}
          alt={asset.rel_path}
          blurred={blurred}
          onReveal={() => setRevealed(true)}
        />
      </div>

      <Descriptions
        size="small"
        bordered
        column={2}
        items={[
          { key: 'id', label: 'ID', children: asset.id },
          { key: 'kind', label: '类型', children: asset.kind },
          { key: 'size', label: '体积', children: formatBytes(asset.bytes) },
          {
            key: 'dim',
            label: '尺寸',
            children: asset.width && asset.height ? `${asset.width}×${asset.height}` : '—',
          },
          {
            key: 'duration',
            label: '时长',
            children: asset.duration_ms === null ? '—' : `${formatNumber(asset.duration_ms)} ms`,
          },
          { key: 'nsfw', label: 'NSFW', children: asset.nsfw ? '是' : '否' },
          { key: 'added', label: '加入时间', children: formatDateTime(asset.added_at) },
          {
            key: 'encoded',
            label: '编码',
            children: asset.vector?.encoded ? (
              <Tooltip
                title={`指纹 ${asset.vector.fingerprint ?? '—'}\n维度 ${asset.vector.dim ?? '—'}\nresized ${
                  asset.vector.resized ?? '—'
                }\nvision_tokens ${asset.vector.vision_tokens ?? '—'}\n编码于 ${formatDateTime(
                  asset.vector.encoded_at,
                )}`}
              >
                <Tag color="green">
                  已编码 · 指纹 {shortFingerprint(asset.vector.fingerprint)}
                </Tag>
              </Tooltip>
            ) : (
              <Tag>未编码</Tag>
            ),
          },
          {
            key: 'sha256',
            label: 'SHA256',
            span: 2,
            children: (
              <Text className="pv-pre" copyable={{ text: asset.sha256 ?? '' }}>
                {asset.sha256 ?? '—'}
              </Text>
            ),
          },
        ]}
      />

      <Space size={16} wrap>
        <Space size={8}>
          <Text strong>NSFW 标记</Text>
          <Switch
            checked={asset.nsfw}
            loading={saving}
            onChange={handleToggleNsfw}
            checkedChildren="开"
            unCheckedChildren="关"
          />
        </Space>
      </Space>

      <div>
        <Text strong>标签（全量替换语义）</Text>
        <Select
          mode="tags"
          style={{ width: '100%', marginTop: 6 }}
          value={tagDraft}
          onChange={setTagDraft}
          placeholder="输入标签后回车"
          tokenSeparators={[',', '，', ' ']}
          options={tagDraft.map((tag) => ({ value: tag, label: tag }))}
        />
      </div>

      <div>
        <Text strong>相对路径</Text>
        <Input
          style={{ marginTop: 6 }}
          value={pathDraft}
          onChange={(event) => setPathDraft(event.target.value)}
          placeholder="例如 wallhaven/2026/cat.jpg"
        />
      </div>

      <Space>
        <Button type="primary" loading={saving} onClick={handleSaveMeta}>
          保存元数据
        </Button>
        <Popconfirm
          title="删除该资产？"
          description="将同时删除提示词、向量点与媒体文件（契约 §4）。"
          okText="删除"
          okButtonProps={{ danger: true }}
          cancelText="取消"
          onConfirm={handleDelete}
        >
          <Button danger loading={saving}>
            删除资产
          </Button>
        </Popconfirm>
      </Space>

      <Divider style={{ margin: '4px 0' }} />
      <Text type="secondary" style={{ fontSize: 12 }}>
        媒体地址（后端直出，前端不拼接）：{asset.media_url}
      </Text>
    </Space>
  ) : null;

  const promptsTab = asset ? (
    <Space orientation="vertical" size={16} style={{ width: '100%' }}>
      {asset.prompts.length === 0 ? (
        <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="还没有提示词" />
      ) : (
        asset.prompts.map((prompt) => (
          <div key={prompt.id} style={{ borderBottom: '1px solid #f0f0f0', paddingBottom: 8 }}>
            <Space size={8} wrap>
              <Tag color="blue">{prompt.role}</Tag>
              <Text type="secondary" style={{ fontSize: 12 }}>
                #{prompt.id} · {formatDateTime(prompt.created_at)}
              </Text>
            </Space>
            <Paragraph style={{ margin: '6px 0 0' }} copyable={{ text: prompt.text }}>
              {prompt.text}
            </Paragraph>
          </div>
        ))
      )}

      <Divider style={{ margin: '4px 0' }}>新增提示词</Divider>
      <Space size={8} wrap>
        <Input
          style={{ width: 160 }}
          value={promptRole}
          onChange={(event) => setPromptRole(event.target.value)}
          placeholder="role，如 caption"
          addonBefore="role"
        />
        <Space size={6}>
          <Text>同步编码</Text>
          <Switch checked={promptEncode} onChange={setPromptEncode} />
        </Space>
      </Space>
      <Input.TextArea
        rows={3}
        value={promptText}
        onChange={(event) => setPromptText(event.target.value)}
        placeholder="例如：一只在窗台上睡觉的橘猫"
      />
      <Button type="primary" loading={saving} onClick={handleAddPrompt}>
        添加提示词
      </Button>
      {promptEncode ? (
        <Text type="secondary" style={{ fontSize: 12 }}>
          编码失败时会返回 ENCODER_* 错误，但提示词已落库，可稍后用作业补编码。
        </Text>
      ) : null}
    </Space>
  ) : null;

  const similarTab = (
    <Space orientation="vertical" size={12} style={{ width: '100%' }}>
      <Space>
        <Button loading={similarLoading} onClick={handleLoadSimilar}>
          查找相似资产（k=12, dense）
        </Button>
        <Text type="secondary" style={{ fontSize: 12 }}>
          走 GET /api/assets/&#123;id&#125;/similar
        </Text>
      </Space>
      {similarError ? <ErrorState error={similarError} context="相似资产查询失败" /> : null}
      {similarLoading ? <Spin /> : null}
      {similar ? <SimilarGrid items={similar} /> : null}
    </Space>
  );

  return (
    <Drawer
      open={open}
      onClose={onClose}
      size={760}
      title={asset ? asset.rel_path : assetId === null ? '资产详情' : `资产 #${assetId}`}
      destroyOnHidden={false}
    >
      {error ? (
        <ErrorState error={error} context="资产详情加载失败" onRetry={reload} />
      ) : loading && !asset ? (
        <Spin />
      ) : asset ? (
        <Tabs
          items={[
            { key: 'overview', label: '概览', children: overview },
            { key: 'prompts', label: `提示词（${asset.prompts.length}）`, children: promptsTab },
            { key: 'similar', label: '相似资产', children: similarTab },
          ]}
        />
      ) : (
        <Empty description="没有数据" />
      )}
    </Drawer>
  );
}
