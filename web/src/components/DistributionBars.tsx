import { Empty, Progress } from 'antd';

import type { FacetBucket } from '../api';
import { formatNumber, safePercent } from '../utils/format';

export interface DistributionBarsProps {
  items: FacetBucket[];
  emptyText?: string;
  strokeColor?: string;
  /** 最多渲染条数，超出部分折叠为「其他」以外的提示（0 表示全部） */
  limit?: number;
}

/**
 * 分布图：用 AntD 自带组件（Progress 条）画标签/月份/来源/类型分布。
 *
 * 为什么不用 @ant-design/charts：契约 §7.7 允许「@ant-design/charts 或 AntD 自带组件」，
 * 而 v6 环境下自绘条形无额外依赖与兼容风险，构建体积也更可控。
 * 比例统一相对当前列表最大值计算，并用 safePercent 保证不出现 NaN。
 */
export function DistributionBars({
  items,
  emptyText = '暂无数据',
  strokeColor,
  limit = 0,
}: DistributionBarsProps) {
  if (!items || items.length === 0) {
    return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={emptyText} />;
  }

  const shown = limit > 0 ? items.slice(0, limit) : items;
  const max = shown.reduce((acc, item) => Math.max(acc, item.count ?? 0), 0);
  const hidden = items.length - shown.length;

  return (
    <div>
      {shown.map((item) => (
        <div className="pv-bar-row" key={item.key}>
          <span className="pv-bar-label" title={item.key}>
            {item.key}
          </span>
          <Progress
            percent={safePercent(item.count, max)}
            showInfo={false}
            size="small"
            strokeColor={strokeColor}
            railColor="#f0f0f0"
          />
          <span className="pv-bar-count">{formatNumber(item.count)}</span>
        </div>
      ))}
      {hidden > 0 ? (
        <div className="pv-bar-count" style={{ textAlign: 'left', marginTop: 4 }}>
          另有 {formatNumber(hidden)} 项未显示
        </div>
      ) : null}
    </div>
  );
}
