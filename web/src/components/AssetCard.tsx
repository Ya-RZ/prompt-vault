import { Card, Tag, Tooltip, Typography } from 'antd';

import type { Asset } from '../api';
import { formatBytes, formatDateTime } from '../utils/format';
import { CoverImage } from './CoverImage';

const { Text } = Typography;

export interface AssetCardProps {
  asset: Asset;
  /** 该卡片是否已揭示 nsfw 内容 */
  revealed: boolean;
  onReveal: (id: number) => void;
  onOpen: (asset: Asset) => void;
}

/** 资产库网格卡片；封面点击进详情抽屉（nsfw 时第一次点击只做揭示） */
export function AssetCard({ asset, revealed, onReveal, onOpen }: AssetCardProps) {
  const blurred = Boolean(asset.nsfw) && !revealed;

  return (
    <Card
      size="small"
      hoverable
      variant="outlined"
      styles={{ body: { padding: 10 } }}
      onClick={() => {
        if (blurred) return; // 模糊态下由 CoverImage 拦下并揭示
        onOpen(asset);
      }}
      cover={
        <CoverImage
          src={asset.thumb_url}
          alt={asset.rel_path}
          blurred={blurred}
          onReveal={() => onReveal(asset.id)}
        />
      }
    >
      <Tooltip title={asset.rel_path}>
        <div className="pv-meta-line" style={{ color: 'rgba(0,0,0,0.75)', fontWeight: 500 }}>
          {asset.rel_path}
        </div>
      </Tooltip>

      <div className="pv-meta-line" style={{ marginTop: 4 }}>
        #{asset.id} · {formatBytes(asset.bytes)}
        {asset.width && asset.height ? ` · ${asset.width}×${asset.height}` : ''}
      </div>

      <div style={{ marginTop: 6, display: 'flex', gap: 4, flexWrap: 'wrap' }}>
        <Tag color={asset.vector?.encoded ? 'green' : 'default'} style={{ marginInlineEnd: 0 }}>
          {asset.vector?.encoded ? '已编码' : '未编码'}
        </Tag>
        {asset.kind === 'video' ? <Tag color="blue" style={{ marginInlineEnd: 0 }}>视频</Tag> : null}
        {asset.nsfw ? <Tag color="red" style={{ marginInlineEnd: 0 }}>NSFW</Tag> : null}
        {(asset.prompts?.length ?? 0) > 0 ? (
          <Tag style={{ marginInlineEnd: 0 }}>{asset.prompts.length} 提示词</Tag>
        ) : null}
      </div>

      {(asset.tags?.length ?? 0) > 0 ? (
        <div style={{ marginTop: 6, display: 'flex', gap: 4, flexWrap: 'wrap' }}>
          {asset.tags.slice(0, 3).map((tag) => (
            <Tag key={tag} color="processing" style={{ marginInlineEnd: 0 }}>
              {tag}
            </Tag>
          ))}
          {asset.tags.length > 3 ? (
            <Text type="secondary" style={{ fontSize: 12 }}>
              +{asset.tags.length - 3}
            </Text>
          ) : null}
        </div>
      ) : null}

      <Tooltip title={formatDateTime(asset.added_at)}>
        <div className="pv-meta-line" style={{ marginTop: 6 }}>
          {formatDateTime(asset.added_at)}
        </div>
      </Tooltip>
    </Card>
  );
}
