import { Card, Tag, Tooltip, Typography } from 'antd';

import type { Asset } from '../api';
import { formatBytes, formatDateTime } from '../utils/format';
import { CoverImage } from './CoverImage';

const { Text } = Typography;

/** 标签最多展示数量（需求 3：卡片上最多 5 个） */
const MAX_CARDS_TAGS = 5;

/** 封面高度比夹在合理区间，避免极端长图/超宽图把瀑布流拉变形 */
function coverRatio(asset: Asset): number {
  if (!asset.width || !asset.height) return 1;
  const ratio = asset.height / asset.width;
  if (!Number.isFinite(ratio) || ratio <= 0) return 1;
  return Math.min(1.9, Math.max(0.55, ratio));
}

export interface AssetCardProps {
  asset: Asset;
  /** 该卡片是否已揭示 nsfw 内容 */
  revealed: boolean;
  onReveal: (id: number) => void;
  onOpen: (asset: Asset) => void;
}

/**
 * 资产库瀑布流卡片。
 *
 * NSFW 交互（需求 6）：模糊态下点击卡片**任何位置**都只解除模糊，不打开详情；
 * 解除之后再点卡片或信息区才进详情抽屉。
 * 卡片上不展示资产 id 与文件路径（需求 3）。
 */
export function AssetCard({ asset, revealed, onReveal, onOpen }: AssetCardProps) {
  const blurred = Boolean(asset.nsfw) && !revealed;
  const tags = asset.tags ?? [];
  const firstPrompt = asset.prompts?.[0]?.text ?? null;

  return (
    <Card
      size="small"
      hoverable
      variant="outlined"
      styles={{ body: { padding: 10 } }}
      onClick={() => {
        // 第一次点击只负责解除模糊，避免误触直接打开详情
        if (blurred) {
          onReveal(asset.id);
          return;
        }
        onOpen(asset);
      }}
      cover={
        <CoverImage
          src={asset.thumb_url}
          alt="资产缩略图"
          blurred={blurred}
          aspectRatio={coverRatio(asset)}
          onReveal={() => onReveal(asset.id)}
        />
      }
    >
      {firstPrompt ? (
        <Tooltip title={firstPrompt}>
          <div className="pv-meta-line" style={{ color: 'rgba(0,0,0,0.75)', fontWeight: 500 }}>
            {firstPrompt}
          </div>
        </Tooltip>
      ) : null}

      <div className="pv-meta-line" style={{ marginTop: 4 }}>
        {formatBytes(asset.bytes)}
        {asset.width && asset.height ? ` · ${asset.width}×${asset.height}` : ''}
      </div>

      <div style={{ marginTop: 6, display: 'flex', gap: 4, flexWrap: 'wrap' }}>
        {asset.kind === 'video' ? (
          <Tag color="blue" style={{ marginInlineEnd: 0 }}>
            视频
          </Tag>
        ) : asset.kind === 'other' ? (
          <Tag style={{ marginInlineEnd: 0 }}>其他</Tag>
        ) : null}
        <Tag color={asset.vector?.encoded ? 'green' : 'default'} style={{ marginInlineEnd: 0 }}>
          {asset.vector?.encoded ? '已编码' : '未编码'}
        </Tag>
        {asset.nsfw ? (
          <Tag color="red" style={{ marginInlineEnd: 0 }}>
            NSFW
          </Tag>
        ) : null}
        {(asset.prompts?.length ?? 0) > 1 ? (
          <Tag style={{ marginInlineEnd: 0 }}>{asset.prompts.length} 提示词</Tag>
        ) : null}
      </div>

      {tags.length > 0 ? (
        <div style={{ marginTop: 6, display: 'flex', gap: 4, flexWrap: 'wrap' }}>
          {tags.slice(0, MAX_CARDS_TAGS).map((tag) => (
            <Tag key={tag} color="processing" style={{ marginInlineEnd: 0 }}>
              {tag}
            </Tag>
          ))}
          {tags.length > MAX_CARDS_TAGS ? (
            <Text type="secondary" style={{ fontSize: 12 }}>
              +{tags.length - MAX_CARDS_TAGS}
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
