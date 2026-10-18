/**
 * 标签 → 稳定的彩色。
 *
 * 需求：每个标签都有颜色、看起来是随机的、但**不要灰/白/黑**。
 *
 * 做法：对标签名做 FNV-1a 哈希得到色相（0–360），饱和度与明度固定在
 * 「鲜艳且可读」的区间：
 * - 底色：S=80% L=93%（浅彩底，避免刺眼也避免灰）
 * - 文字：S=65% L=32%（深彩字，保证对比度）
 * - 边框：S=65% L=78%
 *
 * 同一个标签名永远得到同一个颜色，不需要把颜色写进数据库。
 */
export interface TagColorStyle {
  color: string;
  backgroundColor: string;
  borderColor: string;
}

export function tagColor(name: string): TagColorStyle {
  let hash = 2166136261;
  for (let i = 0; i < name.length; i += 1) {
    hash ^= name.charCodeAt(i);
    hash = Math.imul(hash, 16777619);
  }
  const hue = Math.abs(hash) % 360;
  return {
    color: `hsl(${hue} 65% 32%)`,
    backgroundColor: `hsl(${hue} 80% 93%)`,
    borderColor: `hsl(${hue} 65% 78%)`,
  };
}
