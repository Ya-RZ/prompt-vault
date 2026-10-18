import { FileImageOutlined, InboxOutlined, SearchOutlined } from '@ant-design/icons';
import {
  Alert,
  App as AntdApp,
  Button,
  Card,
  Collapse,
  Empty,
  Input,
  InputNumber,
  Modal,
  Segmented,
  Select,
  Space,
  Spin,
  Switch,
  Table,
  Tag,
  Tooltip,
  Typography,
  Upload,
  type TableProps,
} from 'antd';
import { useEffect, useState } from 'react';

import {
  getSimilarPrompts,
  searchImageByFile,
  searchLexical,
  searchText,
  type LexicalHit,
  type SearchHit,
  type SearchMode,
  type SearchResponse,
  type SimilarPromptItem,
} from '../api';
import { CoverImage } from '../components/CoverImage';
import { ErrorState } from '../components/ErrorState';

const { Text, Paragraph } = Typography;

const MODE_OPTIONS: Array<{ value: SearchMode; label: string }> = [
  { value: 'hybrid', label: 'hybrid（稠密+稀疏+词法）' },
  { value: 'dense', label: 'dense（仅稠密向量）' },
  { value: 'lexical', label: 'lexical（纯词法，零模型成本）' },
];

/** 单条搜索结果卡片：分数、命中提示词、sources、相似提示词入口 */
function SearchResultCard({
  hit,
  revealed,
  onReveal,
  onOpenSimilarPrompts,
}: {
  hit: SearchHit;
  revealed: boolean;
  onReveal: () => void;
  onOpenSimilarPrompts: (promptId: number, text: string) => void;
}) {
  // 先解构，保证 TS 在闭包里仍能保持已收窄的类型
  const matched = hit.matched;

  return (
    <Card
      size="small"
      variant="outlined"
      styles={{ body: { padding: 10 } }}
      cover={
        <CoverImage
          src={hit.asset.thumb_url}
          alt={hit.asset.rel_path}
          blurred={Boolean(hit.asset.nsfw) && !revealed}
          onReveal={onReveal}
        />
      }
    >
      <Space size={4} wrap>
        <Tag color="blue" style={{ marginInlineEnd: 0 }}>
          score {hit.score?.toFixed(4) ?? '—'}
        </Tag>
        {(hit.sources ?? []).map((source) => (
          <Tag key={source} style={{ marginInlineEnd: 0 }}>
            {source}
          </Tag>
        ))}
        {hit.asset.nsfw ? (
          <Tag color="red" style={{ marginInlineEnd: 0 }}>
            NSFW
          </Tag>
        ) : null}
      </Space>

      <Tooltip title={hit.asset.rel_path}>
        <div className="pv-meta-line" style={{ marginTop: 6, color: 'rgba(0,0,0,0.75)' }}>
          #{hit.asset.id} {hit.asset.rel_path}
        </div>
      </Tooltip>

      {matched ? (
        <div style={{ marginTop: 6 }}>
          <Text type="secondary" style={{ fontSize: 12 }}>
            命中提示词（{matched.role}）
          </Text>
          <Paragraph
            style={{ margin: '2px 0 0', fontSize: 13 }}
            ellipsis={{ rows: 3, expandable: true, symbol: '展开' }}
            copyable={{ text: matched.text }}
          >
            {matched.text}
          </Paragraph>
          <Button
            size="small"
            type="link"
            style={{ padding: 0, marginTop: 2 }}
            onClick={() => onOpenSimilarPrompts(matched.prompt_id, matched.text)}
          >
            相似提示词
          </Button>
        </div>
      ) : (
        <Text type="secondary" style={{ fontSize: 12 }}>
          无命中提示词（可能是路径/标签命中）
        </Text>
      )}

      <div className="pv-meta-line" style={{ marginTop: 6 }}>
        rank：dense {hit.rank?.dense ?? '—'} / sparse {hit.rank?.sparse ?? '—'} / lexical{' '}
        {hit.rank?.lexical ?? '—'} / rrf {hit.rank?.rrf ?? '—'}
      </div>
    </Card>
  );
}

/** 相似提示词弹窗（GET /api/prompts/{id}/similar） */
function SimilarPromptModal({
  target,
  onClose,
}: {
  target: { promptId: number; text: string } | null;
  onClose: () => void;
}) {
  const [items, setItems] = useState<SimilarPromptItem[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const targetId = target?.promptId ?? null;

  useEffect(() => {
    if (targetId === null) {
      setItems(null);
      setError(null);
      return;
    }
    let active = true;
    setLoading(true);
    setError(null);
    getSimilarPrompts(targetId, 20)
      .then((response) => {
        if (active) setItems(response.items ?? []);
      })
      .catch((cause: unknown) => {
        if (active) setError(cause);
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [targetId]);

  return (
    <Modal
      open={target !== null}
      title="相似提示词"
      onCancel={() => {
        setItems(null);
        onClose();
      }}
      footer={null}
      width={720}
    >
      {target ? (
        <Space orientation="vertical" size={12} style={{ width: '100%' }}>
          <Alert type="info" showIcon title={`基准提示词 #${target.promptId}`} description={target.text} />
          {error ? <ErrorState error={error} context="相似提示词查询失败" /> : null}
          {loading ? <Spin /> : null}
          {items && items.length === 0 ? (
            <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="没有相似提示词" />
          ) : null}
          {(items ?? []).map((item) => (
            <Card key={item.prompt.id} size="small" variant="outlined">
              <Space size={8} wrap>
                <Tag color="blue">score {item.score?.toFixed(4) ?? '—'}</Tag>
                <Tag>{item.prompt.role}</Tag>
                <Text type="secondary" style={{ fontSize: 12 }}>
                  #{item.prompt.id} · 资产 #{item.asset.id} {item.asset.rel_path}
                </Text>
              </Space>
              <Paragraph style={{ margin: '6px 0 0' }} copyable={{ text: item.prompt.text }}>
                {item.prompt.text}
              </Paragraph>
            </Card>
          ))}
        </Space>
      ) : null}
    </Modal>
  );
}

/** /search —— 文本检索 + 以图搜图 + 词法调试 */
export function SearchPage() {
  const { message } = AntdApp.useApp();

  const [entry, setEntry] = useState<'text' | 'image'>('text');
  const [q, setQ] = useState('');
  const [k, setK] = useState(20);
  const [searchMode, setSearchMode] = useState<SearchMode>('hybrid');
  const [nsfw, setNsfw] = useState(false);
  const [tags, setTags] = useState<string[]>([]);

  const [result, setResult] = useState<SearchResponse | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<unknown>(null);
  const [imageName, setImageName] = useState<string | null>(null);
  const [revealedIds, setRevealedIds] = useState<number[]>([]);
  const [similarTarget, setSimilarTarget] = useState<{ promptId: number; text: string } | null>(null);

  // 词法调试
  const [lexicalQ, setLexicalQ] = useState('');
  const [lexicalRows, setLexicalRows] = useState<LexicalHit[] | null>(null);
  const [lexicalLoading, setLexicalLoading] = useState(false);
  const [lexicalError, setLexicalError] = useState<unknown>(null);

  const runTextSearch = async () => {
    const query = q.trim();
    if (!query) {
      message.warning('请输入检索文本');
      return;
    }
    setLoading(true);
    setError(null);
    setImageName(null);
    try {
      const response = await searchText({
        q: query,
        k,
        mode: searchMode,
        nsfw,
        tag: tags.length > 0 ? tags : undefined,
      });
      setResult(response);
    } catch (cause) {
      setError(cause);
      setResult(null);
    } finally {
      setLoading(false);
    }
  };

  const runImageSearch = async (file: File) => {
    setLoading(true);
    setError(null);
    setImageName(file.name);
    try {
      const response = await searchImageByFile(file, {
        k,
        nsfw,
        tag: tags.length > 0 ? tags : undefined,
      });
      setResult(response);
    } catch (cause) {
      setError(cause);
      setResult(null);
    } finally {
      setLoading(false);
    }
  };

  const runLexical = async () => {
    const query = lexicalQ.trim();
    if (!query) {
      message.warning('请输入词法检索文本');
      return;
    }
    setLexicalLoading(true);
    setLexicalError(null);
    try {
      const response = await searchLexical(query, 50);
      setLexicalRows(response.items ?? []);
    } catch (cause) {
      setLexicalError(cause);
      setLexicalRows(null);
    } finally {
      setLexicalLoading(false);
    }
  };

  const lexicalColumns: TableProps<LexicalHit>['columns'] = [
    { title: '资产 ID', dataIndex: 'asset_id', width: 90 },
    { title: '命中字段', dataIndex: 'hit_field', width: 110, render: (value: string) => <Tag>{value}</Tag> },
    { title: '远端路径', dataIndex: 'rel_path', ellipsis: true },
    { title: '命中文本', dataIndex: 'hit_text', ellipsis: true },
    {
      title: 'score',
      dataIndex: 'score',
      width: 100,
      render: (value: number) => value?.toFixed?.(4) ?? '—',
    },
  ];

  const modeMismatch = result !== null && result.effective_mode !== result.mode;

  return (
    <Space orientation="vertical" size={12} style={{ width: '100%' }}>
      <Card size="small" variant="outlined" styles={{ body: { padding: 12 } }}>
        <Space orientation="vertical" size={12} style={{ width: '100%' }}>
          <Segmented
            value={entry}
            onChange={(value) => setEntry(value as 'text' | 'image')}
            options={[
              { value: 'text', icon: <SearchOutlined />, label: '文本检索' },
              { value: 'image', icon: <FileImageOutlined />, label: '以图搜图' },
            ]}
          />

          {entry === 'text' ? (
            <Space size={8} wrap>
              <Input.Search
                style={{ width: 380 }}
                placeholder="输入提示词或描述，例如：橘猫 窗台"
                value={q}
                onChange={(event) => setQ(event.target.value)}
                onSearch={runTextSearch}
                enterButton="搜索"
                loading={loading}
              />
              <Select
                style={{ width: 250 }}
                value={searchMode}
                onChange={setSearchMode}
                options={MODE_OPTIONS}
              />
            </Space>
          ) : (
            <Upload.Dragger
              accept="image/*"
              multiple={false}
              showUploadList={false}
              disabled={loading}
              beforeUpload={(file) => {
                void runImageSearch(file as File);
                return false;
              }}
              style={{ padding: 8 }}
            >
              <p style={{ margin: 0 }}>
                <InboxOutlined style={{ fontSize: 30, color: '#1677ff' }} />
              </p>
              <p style={{ margin: '6px 0 0' }}>点击或把图片拖到这里，进行以图搜图</p>
              <p style={{ margin: 0 }}>
                <Text type="secondary" style={{ fontSize: 12 }}>
                  {imageName ? `上次查询：${imageName}` : '支持 JPG / PNG / WebP'}
                </Text>
              </p>
            </Upload.Dragger>
          )}

          <Space size={12} wrap>
            <Space size={6}>
              <Text>k</Text>
              <InputNumber min={1} max={200} value={k} onChange={(value) => setK(value ?? 20)} />
            </Space>
            <Select
              mode="tags"
              style={{ minWidth: 190 }}
              placeholder="tag（可多个）"
              value={tags}
              onChange={setTags}
              options={tags.map((tag) => ({ value: tag, label: tag }))}
            />
            <Tooltip title="默认过滤 NSFW 资产；打开表示包含 NSFW。">
              <Space size={6}>
                <Switch checked={nsfw} onChange={setNsfw} />
                <Text>包含 NSFW</Text>
              </Space>
            </Tooltip>
          </Space>
        </Space>
      </Card>

      {error ? <ErrorState error={error} context="检索失败" /> : null}

      {result ? (
        <Space orientation="vertical" size={8} style={{ width: '100%' }}>
          <Alert
            type="success"
            showIcon
            title={`查询「${result.query}」共 ${result.total} 条 · took ${result.took_ms ?? '—'} ms`}
            description={
              <span>
                请求模式 <Tag>{result.mode}</Tag> → 实际模式 <Tag>{result.effective_mode}</Tag>
                {result.query_image
                  ? ` · 查询图 resized ${result.query_image.resized ?? '—'} / vision_tokens ${
                      result.query_image.vision_tokens ?? '—'
                    }`
                  : ''}
              </span>
            }
          />

          {modeMismatch ? (
            <Alert
              type="warning"
              showIcon
              title={`请求的 mode=${result.mode}，实际按 ${result.effective_mode} 执行`}
              description="稀疏向量不可用时 hybrid 会自动退化为 dense。若需要零模型成本的检索，请改用 lexical 模式。"
            />
          ) : null}

          {(result.warnings ?? []).map((warning) => (
            <Alert key={warning} type="warning" showIcon title="检索警告" description={warning} />
          ))}

          {result.items.length === 0 ? (
            <Card variant="outlined">
              <Empty description="没有命中结果" />
            </Card>
          ) : (
            <div className="pv-grid" style={{ gridTemplateColumns: 'repeat(auto-fill, minmax(230px, 1fr))' }}>
              {result.items.map((hit) => (
                <SearchResultCard
                  key={`${hit.asset.id}-${hit.matched?.prompt_id ?? 'none'}`}
                  hit={hit}
                  revealed={revealedIds.includes(hit.asset.id)}
                  onReveal={() => setRevealedIds((prev) => [...prev, hit.asset.id])}
                  onOpenSimilarPrompts={(promptId, text) => setSimilarTarget({ promptId, text })}
                />
              ))}
            </div>
          )}
        </Space>
      ) : null}

      <Collapse
        items={[
          {
            key: 'lexical',
            label: '词法检索调试',
            children: (
              <Space orientation="vertical" size={8} style={{ width: '100%' }}>
                <Space size={8}>
                  <Input.Search
                    style={{ width: 320 }}
                    placeholder="纯词法命中：prompt.text / rel_path / tag.name"
                    value={lexicalQ}
                    onChange={(event) => setLexicalQ(event.target.value)}
                    onSearch={runLexical}
                    enterButton="词法检索"
                    loading={lexicalLoading}
                  />
                </Space>
                {lexicalError ? <ErrorState error={lexicalError} context="词法检索失败" /> : null}
                {lexicalRows ? (
                  <Table<LexicalHit>
                    size="small"
                    rowKey={(record) => `${record.asset_id}-${record.hit_field}-${record.hit_text}`}
                    columns={lexicalColumns}
                    dataSource={lexicalRows}
                    pagination={false}
                    scroll={{ x: 760 }}
                  />
                ) : (
                  <Text type="secondary">尚未执行词法检索。</Text>
                )}
              </Space>
            ),
          },
        ]}
      />

      <SimilarPromptModal target={similarTarget} onClose={() => setSimilarTarget(null)} />
    </Space>
  );
}
