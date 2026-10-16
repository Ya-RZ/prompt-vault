import { Alert, Button, Space, Tag } from 'antd';

import { describeError } from '../api';

export interface ErrorStateProps {
  /** 任意异常；内部统一按契约 {error:{code,message,detail}} 解析成人话 */
  error: unknown;
  /** 上下文前缀，例如「资产列表加载失败」 */
  context?: string;
  onRetry?: () => void;
}

/**
 * 统一的错误态展示。
 * 后端未启动 / 侧车宕机 / 指纹不一致 / 413 / 金标失败 都会走到这里，
 * 保证任何页面在请求失败时显示可读信息而不是白屏。
 */
export function ErrorState({ error, context, onRetry }: ErrorStateProps) {
  const { title, description, code } = describeError(error);

  return (
    <Alert
      type="error"
      showIcon
      title={context ? `${context} — ${title}` : title}
      description={
        <Space orientation="vertical" size={8} style={{ width: '100%' }}>
          <span style={{ whiteSpace: 'pre-wrap' }}>{description}</span>
          <Space size={8} wrap>
            <Tag color="red">{code}</Tag>
            {onRetry ? (
              <Button size="small" onClick={onRetry}>
                重试
              </Button>
            ) : null}
          </Space>
        </Space>
      }
    />
  );
}
