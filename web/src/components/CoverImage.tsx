import { EyeInvisibleOutlined, FileImageOutlined } from '@ant-design/icons';
import type { MouseEvent } from 'react';
import { useState } from 'react';

export interface CoverImageProps {
  src: string | null | undefined;
  alt: string;
  /** true 时用 CSS blur 模糊占位，点一下才显示（nsfw=true 的默认行为） */
  blurred?: boolean;
  onReveal?: () => void;
  /**
   * 封面高度比（height / width）。
   * 传数值时按该比例占位 —— 瀑布流卡片靠它得到不同高度，
   * 同时避免图片加载前高度塌陷造成 Masonry 重排抖动。不传则用 CSS 的 1:1。
   */
  aspectRatio?: number;
}

/**
 * 媒体封面。
 *
 * - `blurred=true`：图片加 `filter: blur()`，并拦下点击事件用于「先揭示」，
 *   避免一次点击直接穿透到父级的「打开详情」。
 * - 加载失败/无地址：显示占位而不是碎图图标。
 */
export function CoverImage({
  src,
  alt,
  blurred = false,
  onReveal,
  aspectRatio,
}: CoverImageProps) {
  const [failed, setFailed] = useState(false);
  const showPlaceholder = !src || failed;

  const handleClick = (event: MouseEvent<HTMLDivElement>) => {
    if (!blurred) return;
    event.stopPropagation();
    onReveal?.();
  };

  return (
    <div
      className="pv-card-cover"
      onClick={handleClick}
      style={aspectRatio ? { aspectRatio } : undefined}
    >
      {showPlaceholder ? (
        <div className="pv-blur-badge" style={{ background: '#fafafa', color: 'rgba(0,0,0,0.35)' }}>
          <FileImageOutlined style={{ fontSize: 22 }} />
          <span>{src ? '图片加载失败' : '无缩略图'}</span>
        </div>
      ) : (
        <img
          src={src}
          alt={alt}
          loading="lazy"
          className={blurred ? 'pv-blur' : undefined}
          onError={() => setFailed(true)}
        />
      )}
      {blurred && !showPlaceholder ? (
        <div className="pv-blur-badge">
          <EyeInvisibleOutlined style={{ fontSize: 20 }} />
          <span>NSFW · 点击显示</span>
        </div>
      ) : null}
    </div>
  );
}
