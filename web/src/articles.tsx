import { useCallback, useEffect, useState } from 'react'
import {
  Alert, Button, Descriptions, Drawer, Flex, Image, Input, List, message, Rate, Select, Space,
  Table, Tag, Typography,
} from 'antd'
import { FileTextOutlined, RobotOutlined } from '@ant-design/icons'
import {
  api, fmtTime, sentimentTag, type Article, type SectionCount, type Source, type Topic,
} from './api'
import { Md } from './md'

const { Text, Link, Paragraph } = Typography

export default function ArticlesTab() {
  const [articles, setArticles] = useState<Article[]>([])
  const [sources, setSources] = useState<Source[]>([])
  const [topics, setTopics] = useState<Topic[]>([])
  const [loading, setLoading] = useState(false)
  const [q, setQ] = useState('')
  const [sourceId, setSourceId] = useState<number | undefined>()
  const [topicId, setTopicId] = useState<number | undefined>()
  const [sections, setSections] = useState<SectionCount[]>([])
  const [section, setSection] = useState<string | undefined>()
  const [hours, setHours] = useState<number | undefined>()
  const [detail, setDetail] = useState<Article | null>(null)

  const load = useCallback(() => {
    setLoading(true)
    api
      .articles({ q: q || undefined, source_id: sourceId, topic_id: topicId, section, hours, limit: 100 })
      .then((r) => setArticles(r.articles))
      .finally(() => setLoading(false))
  }, [q, sourceId, topicId, section, hours])

  useEffect(() => {
    load()
  }, [load])
  useEffect(() => {
    api.sources().then((r) => setSources(r.sources))
    api.topics().then((r) => setTopics(r.topics))
    api.sections().then((r) => setSections(r.sections))
  }, [])

  return (
    <Space direction="vertical" size={12} style={{ width: '100%' }}>
      <Flex gap={8} wrap>
        <Input.Search
          placeholder="Tìm bài (gõ không dấu vẫn khớp)…"
          allowClear
          style={{ width: 280 }}
          onSearch={setQ}
        />
        <Select
          placeholder="Nguồn"
          allowClear
          style={{ width: 200 }}
          value={sourceId}
          onChange={setSourceId}
          options={sources.map((s) => ({ value: s.id, label: s.name }))}
        />
        <Select
          placeholder="Chủ đề"
          allowClear
          style={{ width: 180 }}
          value={topicId}
          onChange={setTopicId}
          options={topics.map((t) => ({ value: t.id, label: t.name }))}
        />
        <Select
          placeholder="Chuyên mục"
          allowClear
          style={{ width: 200 }}
          value={section}
          onChange={setSection}
          options={sections.map((s) => ({ value: s.section, label: `${s.section} (${s.count})` }))}
        />
        <Select
          placeholder="Thời gian"
          allowClear
          style={{ width: 140 }}
          value={hours}
          onChange={setHours}
          options={[
            { value: 6, label: '6 giờ qua' },
            { value: 24, label: '24 giờ qua' },
            { value: 72, label: '3 ngày qua' },
            { value: 168, label: '7 ngày qua' },
          ]}
        />
      </Flex>

      <Table
        size="small"
        rowKey="id"
        loading={loading}
        dataSource={articles}
        pagination={{ pageSize: 20, showSizeChanger: false }}
        onRow={(a) => ({ onClick: () => openDetail(a.id), style: { cursor: 'pointer' } })}
        columns={[
          {
            title: 'Tiêu đề',
            dataIndex: 'title',
            render: (t: string, a) => (
              <Space direction="vertical" size={0}>
                <Text strong>{t}</Text>
                <Text type="secondary" style={{ fontSize: 12 }} ellipsis>
                  {a.description?.slice(0, 140)}
                </Text>
              </Space>
            ),
          },
          {
            title: 'Chuyên mục',
            dataIndex: 'section',
            width: 140,
            render: (s: string, a) =>
              s ? (
                <Tag
                  style={{ cursor: 'pointer' }}
                  title={`System One · độ chắc ${Math.round(a.section_conf * 100)}%`}
                  onClick={(e) => {
                    e.stopPropagation()
                    setSection(s)
                  }}
                >
                  {s}
                </Tag>
              ) : null,
          },
          { title: 'Nguồn', dataIndex: 'source_name', width: 150, ellipsis: true },
          {
            title: 'Đăng lúc',
            dataIndex: 'published_at',
            width: 110,
            render: (v: string) => <Text type="secondary">{fmtTime(v)}</Text>,
          },
          {
            title: '',
            dataIndex: 'story_size',
            width: 90,
            // Every article technically has a story; the badge only means
            // "nhiều nguồn/bài cùng đưa" — singletons stay unmarked.
            render: (n: number) => (n >= 2 ? <Tag color="purple">sự kiện ×{n}</Tag> : null),
          },
        ]}
      />

      <ArticleDrawer article={detail} onClose={() => setDetail(null)} onReload={(a) => setDetail(a)} />
    </Space>
  )

  function openDetail(id: number) {
    api.article(id).then((r) => {
      if (r.article) setDetail(r.article)
    })
  }
}

function ArticleDrawer({
  article,
  onClose,
  onReload,
}: {
  article: Article | null
  onClose: () => void
  onReload: (a: Article) => void
}) {
  const [busy, setBusy] = useState('')

  if (!article) return null
  const a = article

  const reload = () => api.article(a.id).then((r) => r.article && onReload(r.article))

  const fetchContent = async () => {
    setBusy('content')
    try {
      const r = await api.fetchContent(a.id)
      if (r.error) message.error(String(r.error))
      else message.success(r.cached ? 'Đã có toàn văn (cache)' : 'Đã tải toàn văn')
      reload()
    } finally {
      setBusy('')
    }
  }

  const analyze = async (force: boolean) => {
    setBusy('ai')
    try {
      const r = await api.analyzeArticle(a.id, force, !a.content)
      if (r.error) message.error(String(r.error))
      reload()
    } finally {
      setBusy('')
    }
  }

  const an = a.analysis
  const st = sentimentTag(an?.sentiment)

  return (
    <Drawer open width={680} onClose={onClose} title={<Text strong>{a.title}</Text>}>
      <Space direction="vertical" size={14} style={{ width: '100%' }}>
        <Space size={[6, 6]} wrap>
          <Tag>{a.source_name}</Tag>
          <Tag>{fmtTime(a.published_at)}</Tag>
          {a.category && <Tag color="cyan">{a.category}</Tag>}
          {(a.topics ?? []).map((t) => (
            <Tag key={t.id} color={t.color || 'blue'}>{t.name}</Tag>
          ))}
        </Space>
        <Link href={a.url} target="_blank">Mở bài gốc ↗</Link>
        {a.image_url && <Image src={a.image_url} style={{ maxHeight: 220, objectFit: 'cover' }} />}
        <Paragraph style={{ marginBottom: 0 }}>{a.description}</Paragraph>

        <Space>
          <Button
            size="small"
            icon={<FileTextOutlined />}
            loading={busy === 'content'}
            onClick={fetchContent}
            disabled={!!a.content}
          >
            {a.content ? 'Đã có toàn văn' : 'Tải toàn văn'}
          </Button>
          <Button
            size="small"
            type="primary"
            icon={<RobotOutlined />}
            loading={busy === 'ai'}
            onClick={() => analyze(!!an)}
          >
            {an ? 'AI đánh giá lại' : 'AI đánh giá bài này'}
          </Button>
        </Space>

        {an && (
          <Alert
            type="info"
            message={
              <Space size={[6, 6]} wrap>
                <Tag color={st.color}>{st.label}</Tag>
                <span>
                  Quan trọng: <Rate disabled value={an.importance} count={5} style={{ fontSize: 13 }} />
                </span>
                {an.clickbait && <Tag color="volcano">Nghi giật tít</Tag>}
                {an.tags.map((t) => (
                  <Tag key={t}>{t}</Tag>
                ))}
              </Space>
            }
            description={
              <Space direction="vertical" size={4} style={{ marginTop: 6 }}>
                <Md text={an.summary} />
                <Text type="secondary" style={{ fontSize: 12 }}>
                  Độ tin cậy: {an.reliability} · {an.model && `model: ${an.model} · `}
                  {fmtTime(an.at)}
                </Text>
              </Space>
            }
          />
        )}

        {a.content && (
          <Descriptions column={1} size="small" title="Toàn văn (trích xuất)">
            <Descriptions.Item>
              <Paragraph style={{ whiteSpace: 'pre-wrap', maxHeight: 320, overflow: 'auto', marginBottom: 0 }}>
                {a.content}
              </Paragraph>
            </Descriptions.Item>
          </Descriptions>
        )}

        {(a.related ?? []).length > 0 && (
          <List
            size="small"
            header={<Text strong>Tin liên quan cùng sự kiện</Text>}
            dataSource={a.related}
            renderItem={(r) => (
              <List.Item>
                <Space direction="vertical" size={0} style={{ width: '100%' }}>
                  <Link href={r.url} target="_blank" ellipsis>{r.title}</Link>
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    {r.source_name} · {fmtTime(r.published_at)}
                  </Text>
                </Space>
              </List.Item>
            )}
          />
        )}
      </Space>
    </Drawer>
  )
}
