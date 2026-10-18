import { Card, Tag, Tooltip, Typography } from 'antd';

import type { Asset } from '../api';
import { formatBytes, formatDateTime } from '../utils/format';
import { CoverImage } from './CoverImage';

const { Text } = Typography;

/** 标签最多展示数量（需求 3：卡片上最多 5 个） */
const MAX_CARDS_TAGS = 5;

/** 封面高度比夹在合理区间，避免极端长图/超宽图把瀑布流拉变形 */
/**
 * 封面比例 = 图片真实 **宽高比**（width / height）。
 *
 * 注意方向：CSS 的 `aspect-ratio` 是 `width / height`，之前这里传的是 `height / width`，
 * 横图会被渲染成竖框、再被 `object-fit` 裁掉大半 —— 这是「首页图片不等比例」的主因。
 * 只保留一个极端保护（8:1），避免病态长图把瀑布流撑爆。
 */
function coverRatio(asset: Asset): number {
  if (!asset.width || !asset.height) return 1;
  const ratio = asset.width / asset.height;
  if (!Number.isFinite(ratio) || ratio <= 0) return 1;
  return Math.min(8, Math.max(0.125, ratio));
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
 * 点击行为（需求 6）：
 * - **点图片**：NSFW 未揭示时只解除模糊；已揭示或非 NSFW 时打开详情
 * - **点图片下方的信息区**（提示词/尺寸/标签）：直接打开详情，与模糊状态无关
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
      cover={
        // 图片区：模糊态由 CoverImage 拦截点击（stopPropagation）并解除模糊；
        // 其余情况冒泡到这里打开详情。
        <div onClick={() => onOpen(asset)}>
          <CoverImage
            src={asset.thumb_url}
            alt="资产缩略图"
            blurred={blurred}
            aspectRatio={coverRatio(asset)}
            onReveal={() => onReveal(asset.id)}
          />
        </div>
      }
    >
      <div onClick={() => onOpen(asset)} style={{ cursor: 'pointer' }}>
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
      </div>
    </Card>
  );
}
