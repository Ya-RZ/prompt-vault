import { App as AntdApp, Form, Input, Modal, Select, Space, Tag, Typography } from 'antd';
import { useState } from 'react';

import {
  createAssetPrompt,
  describeError,
  updateAssetMeta,
  type Asset,
} from '../api';
import { CoverImage } from './CoverImage';

const { Text } = Typography;

interface UploadMetaFormValues {
  title?: string;
  tags?: string[];
  prompt?: string;
}

export interface AssetUploadModalProps {
  /** 刚上传成功的资产；为 null 时弹窗不渲染内容 */
  asset: Asset | null;
  open: boolean;
  onClose: () => void;
  /** 保存成功后通知外层刷新列表 */
  onSaved: () => void;
}

/** 上传文件名（去掉目录），作为「标题」的默认值 */
function defaultTitle(asset: Asset): string {
  const base = asset.rel_path?.split(/[\\/]/).pop() ?? '';
  return base;
}

/**
 * 上传成功后的「填写信息」弹窗。
 *
 * 「标题」走 AssetDto.title（与文件路径无关）；标签走 tags（全量替换语义）；
 * 提示词走 `POST /api/assets/{id}/prompts`。
 */
export function AssetUploadModal({ asset, open, onClose, onSaved }: AssetUploadModalProps) {
  const { message } = AntdApp.useApp();
  const [form] = Form.useForm<UploadMetaFormValues>();
  const [saving, setSaving] = useState(false);

  const handleSave = async () => {
    if (!asset) return;
    let values: UploadMetaFormValues;
    try {
      values = await form.validateFields();
    } catch {
      return; // 校验失败，AntD 已在字段上给出提示
    }

    setSaving(true);
    try {
      const title = values.title?.trim();
      // tags 为全量替换语义：这里提交表单里的完整标签列表
      await updateAssetMeta(asset.id, {
        title: title ?? '',
        tags: values.tags ?? [],
      });

      const promptText = values.prompt?.trim();
      if (promptText) {
        try {
          await createAssetPrompt(asset.id, { text: promptText, role: 'caption', encode: true });
        } catch (cause) {
          // 契约 §4：编码失败时提示词已落库，可稍后用作业补编码
          const presentation = describeError(cause);
          message.warning(`提示词可能已保存，但编码失败：${presentation.title}`);
        }
      }

      message.success('资产信息已保存');
      onSaved();
      onClose();
    } catch (cause) {
      const presentation = describeError(cause);
      message.error(`保存失败：${presentation.title}`);
    } finally {
      setSaving(false);
    }
  };

  return (
    <Modal
      open={open}
      title="填写资产信息"
      okText="保存"
      cancelText="稍后再说"
      confirmLoading={saving}
      onOk={handleSave}
      onCancel={onClose}
      destroyOnHidden
    >
      {asset ? (
        <Space orientation="vertical" size={12} style={{ width: '100%' }}>
          <div style={{ display: 'flex', gap: 12, alignItems: 'flex-start' }}>
            <div style={{ width: 96, flex: '0 0 auto' }}>
              <CoverImage src={asset.thumb_url} alt="刚上传的资产" />
            </div>
            <Space orientation="vertical" size={4}>
              <Text strong>上传成功</Text>
              <Text type="secondary" style={{ fontSize: 12 }}>
                {asset.kind} · {asset.width && asset.height ? `${asset.width}×${asset.height}` : '尺寸未知'}
              </Text>
              <Tag color="green" style={{ marginInlineEnd: 0 }}>
                哈希 {asset.hash_algo}
              </Tag>
              {asset.hash ? (
                <Text type="secondary" className="pv-pre" style={{ fontSize: 11 }}>
                  {asset.hash.slice(0, 16)}…
                </Text>
              ) : null}
            </Space>
          </div>

          <Form
            // key 随资产变化重建表单，让 initialValues 生效（避免手动 setFieldsValue 的时序问题）
            key={asset.id}
            form={form}
            layout="vertical"
            requiredMark={false}
            initialValues={{
              title: defaultTitle(asset),
              tags: asset.tags ?? [],
              prompt: '',
            }}
          >
            <Form.Item label="标题" name="title" extra="只影响展示，不会改动文件路径">
              <Input placeholder="例如 wallhaven/2026/cat.jpg" allowClear />
            </Form.Item>
            <Form.Item label="标签" name="tags">
              <Select
                mode="tags"
                placeholder="输入标签后回车，可多个"
                tokenSeparators={[',', '，', ' ']}
              />
            </Form.Item>
            <Form.Item
              label="提示词"
              name="prompt"
              extra="保存时会同步编码；编码失败不影响提示词落库"
            >
              <Input.TextArea rows={3} placeholder="例如：一只在窗台上睡觉的橘猫" />
            </Form.Item>
          </Form>
        </Space>
      ) : null}
    </Modal>
  );
}
