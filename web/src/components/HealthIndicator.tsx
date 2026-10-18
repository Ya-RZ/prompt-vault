import { ReloadOutlined } from '@ant-design/icons';
import { Badge, Button, Space, Tag, Tooltip, Typography } from 'antd';

import { describeError, type GoldenInfo, type HealthResponse } from '../api';
import { formatDateTime, shortFingerprint } from '../utils/format';

const { Text } = Typography;

export interface HealthIndicatorProps {
  health: HealthResponse | null;
  error: unknown;
  loading: boolean;
  onRefresh: () => void;
}

function goldenTagColor(status: GoldenInfo['status']): string {
  switch (status) {
    case 'passed':
      return 'green';
    case 'failed':
      return 'red';
    case 'unsupported':
      return 'orange';
    case 'unchecked':
    default:
      return 'default';
  }
}

/** golden 状态的说明性 tooltip（契约 §1.2 / §3：unsupported 必须显式暴露原因） */
function goldenTooltip(golden: GoldenInfo): string {
  const lines: string[] = [];
  switch (golden.status) {
    case 'passed':
      lines.push('金标校验已通过：编码器与基准一致。');
      break;
    case 'failed':
      lines.push('金标校验未通过：/v1/verify 不达标，导入与编码作业会被拒绝（409 GOLDEN_FAILED）。');
      break;
    case 'unsupported':
      lines.push(
        '侧车未提供 /v1/verify（开发文档 §5.4 改动 1 未实施），金标校验不可用。',
        '主服务不因此拒绝启动，但会在作业开始前按 unsupported 记录。',
      );
      break;
    case 'unchecked':
    default:
      lines.push('尚未执行金标校验。');
      break;
  }
  if (golden.detail) lines.push(`detail：${golden.detail}`);
  if (golden.checked_at) lines.push(`检查时间：${formatDateTime(golden.checked_at)}`);
  if (golden.cos_text !== null) lines.push(`cos_text：${golden.cos_text}`);
  if (golden.cos_image !== null) lines.push(`cos_image：${golden.cos_image}`);
  if (golden.threshold !== null) lines.push(`阈值：${golden.threshold}`);
  if (golden.device_mismatch) lines.push('⚠️ 设备与基准不一致。');
  return lines.join('\n');
}

/** 页头右侧的健康徽标：状态、编码指纹短码、侧车设备、金标状态 */
export function HealthIndicator({ health, error, loading, onRefresh }: HealthIndicatorProps) {
  const refreshButton = (
    <Tooltip title="重新拉取 /api/health">
      <Button
        size="small"
        type="text"
        icon={<ReloadOutlined />}
        loading={loading}
        onClick={onRefresh}
        aria-label="刷新健康状态"
      />
    </Tooltip>
  );

  if (!health) {
    const presentation = error ? describeError(error) : null;
    const label = error ? '后端不可达' : loading ? '检查中…' : '健康数据缺失';
    const color = error ? 'red' : loading ? 'default' : 'orange';
    return (
      <Space size={8}>
        <Tooltip
          title={
            presentation
              ? presentation.description
              : loading
                ? '正在请求 /api/health…'
                : '已收到响应但没有可用的健康数据，请检查主服务版本是否与 INTERFACES.md 一致。'
          }
        >
          <Tag color={color} style={{ marginInlineEnd: 0 }}>
            {label}
          </Tag>
        </Tooltip>
        {refreshButton}
      </Space>
    );
  }

  const ready = health.status === 'READY';
  const encoder = health.encoder;

  return (
    <Space size={12} wrap>
      <Tooltip
        title={
          ready
            ? '主服务与编码侧车均正常。'
            : '服务降级：侧车不可达、金标失败或索引指纹不一致，详见 /api/health。'
        }
      >
        <span>
          <Badge status={ready ? 'success' : 'warning'} />
          <Text strong style={{ marginInlineStart: 6 }}>
            {health.status}
          </Text>
        </span>
      </Tooltip>

      <Tooltip
        title={
          <span className="pv-pre">
            索引指纹：{health.index.fingerprint || '—'}
            {'\n'}编码器指纹：{encoder.fingerprint || '—'}
          </span>
        }
      >
        <Tag className="pv-header-fingerprint" style={{ marginInlineEnd: 0 }}>
          指纹 {shortFingerprint(health.index.fingerprint || encoder.fingerprint)}
        </Tag>
      </Tooltip>

      <Tooltip
        title={
          encoder.mock
            ? '当前是 --mock 模式：未加载模型，向量由 sha256 派生，仅供链路验证'
            : `模型目录：${encoder.model_dir || '—'}\n模型：${encoder.model || '—'}\n权重加载：${encoder.loaded ? '已加载' : '未加载（首次编码时加载）'}`
        }
      >
        <Tag style={{ marginInlineEnd: 0 }}>
          {encoder.mock ? 'mock' : `${encoder.device || '—'} / ${encoder.dim || '—'} 维`}
        </Tag>
      </Tooltip>

      <Tooltip
        title={`稀疏路（sparse）：${health.index.sparse_enabled ? '可用' : '不可用，hybrid 会退化为 dense'}`}
      >
        <Tag color={health.index.sparse_enabled ? 'green' : 'default'} style={{ marginInlineEnd: 0 }}>
          {health.index.sparse_enabled ? '稀疏可用' : '稀疏不可用'}
        </Tag>
      </Tooltip>

      <Tooltip title={<span className="pv-pre">{goldenTooltip(health.golden)}</span>}>
        <Tag color={goldenTagColor(health.golden.status)} style={{ marginInlineEnd: 0 }}>
          金标 {health.golden.status}
        </Tag>
      </Tooltip>

      <Tooltip title={`已编码点 ${health.index.points} / 资产 ${health.index.assets} / 提示词 ${health.index.prompts}`}>
        <Text type="secondary" style={{ fontSize: 12 }}>
          {health.index.points} 点
        </Text>
      </Tooltip>

      {refreshButton}
    </Space>
  );
}
