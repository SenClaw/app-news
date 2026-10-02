import { useCallback, useEffect, useState } from 'react'
import {
  Button, Card, Col, Empty, List, message, Radio, Row, Select, Space, Spin, Tag, Timeline, Tooltip, Typography,
} from 'antd'
import { HistoryOutlined, RobotOutlined, TranslationOutlined } from '@ant-design/icons'
import { api, fmtDate, fmtTime, type Story } from './api'
import StoryGraph from './graph'
import { JobRunningCard } from './jobs'
import { Md } from './md'

const { Text, Link } = Typography

export default function StoriesTab() {
  const [days, setDays] = useState(7)
  const [minArticles, setMinArticles] = useState(2)
  const [stories, setStories] = useState<Story[] | null>(null)
  const [selected, setSelected] = useState<Story | null>(null)
  const [briefBusy, setBriefBusy] = useState(false)
  const [briefSec, setBriefSec] = useState(0)
  // Story whose most recent brief came back cut off. Not persisted with the
  // cached summary, so it only flags the brief this session actually ran.
  const [briefCut, setBriefCut] = useState<number | null>(null)
  const [view, setView] = useState<'list' | 'graph'>('list')
  // Which past reading is on screen. null = the current one; the timeline is
  // always shown underneath either way, which is what people came for.
  const [pastId, setPastId] = useState<number | null>(null)
  const [transBusy, setTransBusy] = useState(false)
  const [showOriginal, setShowOriginal] = useState(false)

  const load = useCallback(() => {
    setStories(null)
    api.stories(days, minArticles, 50).then((r) => {
      setStories(r.stories)
      if (r.stories.length && !r.stories.find((s) => s.id === selected?.id)) {
        openStory(r.stories[0].id)
      }
      if (!r.stories.length) setSelected(null)
    })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [days, minArticles])

  useEffect(() => {
    load()
  }, [load])

  function openStory(id: number) {
    setPastId(null)
    api.story(id).then((r) => r.story && setSelected(r.story))
  }

  const brief = async (force: boolean) => {
    if (!selected) return
    setBriefBusy(true)
    setBriefSec(0)
    const started = Date.now()
    const ticker = window.setInterval(() => setBriefSec(Math.round((Date.now() - started) / 1000)), 1000)
    try {
      const r = await api.storyBrief(selected.id, force)
      if (r.error) message.error(String(r.error))
      setBriefCut(r.truncated ? selected.id : null)
      openStory(selected.id)
    } finally {
      window.clearInterval(ticker)
      setBriefBusy(false)
    }
  }

  const translate = async () => {
    if (!selected) return
    setTransBusy(true)
    try {
      const r = await api.translateStory(selected.id)
      if (r.error) message.error(String(r.error))
      else if (r.warning) message.warning(`Dịch được ${r.translated} bài, phần còn lại: ${r.warning}`)
      else if (r.translated === 0) message.info('Đã có sẵn bản dịch')
      else message.success(`Đã dịch ${r.translated} bài sang ${r.lang}`)
      openStory(selected.id)
    } finally {
      setTransBusy(false)
    }
  }

  // The reading currently on screen: a past one if picked, otherwise the latest.
  const history = selected?.summaries ?? []
  const shown = pastId ? history.find((h) => h.id === pastId) : null
  const shownText = shown ? shown.summary : selected?.summary
  const shownModel = shown ? shown.model : selected?.summary_model

  if (!stories) return <Spin style={{ marginTop: 40 }} />

  return (
    <Space direction="vertical" size={12} style={{ width: '100%' }}>
      <Space wrap>
        <Select
          value={days}
          onChange={setDays}
          style={{ width: 140 }}
          options={[
            { value: 3, label: '3 ngày qua' },
            { value: 7, label: '7 ngày qua' },
            { value: 14, label: '14 ngày qua' },
            { value: 30, label: '30 ngày qua' },
          ]}
        />
        <Select
          value={minArticles}
          onChange={setMinArticles}
          style={{ width: 170 }}
          options={[
            { value: 2, label: '≥ 2 bài (mặc định)' },
            { value: 3, label: '≥ 3 bài' },
            { value: 1, label: 'Tất cả (kể cả 1 bài)' },
          ]}
        />
        <Radio.Group value={view} onChange={(e) => setView(e.target.value)} optionType="button" size="small">
          <Radio.Button value="list">Danh sách</Radio.Button>
          <Radio.Button value="graph">Bản đồ liên kết</Radio.Button>
        </Radio.Group>
        <Text type="secondary">
          Bài về cùng một sự kiện từ nhiều nguồn được gom tự động thành dòng sự kiện.
        </Text>
      </Space>

      {view === 'graph' && (
        <StoryGraph
          days={days}
          minArticles={minArticles}
          onSelect={(id) => {
            openStory(id)
            setView('list')
          }}
        />
      )}

      {view === 'graph' ? null : stories.length === 0 ? (
        <Empty description="Chưa có dòng sự kiện — cần vài nguồn cùng đưa một tin (thu thập thêm rồi quay lại)" />
      ) : (
        <Row gutter={16}>
          <Col span={9}>
            <List
              size="small"
              dataSource={stories}
              renderItem={(s) => (
                <List.Item
                  onClick={() => openStory(s.id)}
                  style={{
                    cursor: 'pointer',
                    borderRadius: 8,
                    padding: '8px 10px',
                    // Works in both themes: primary tint, not a fixed color.
                    background: selected?.id === s.id ? 'color-mix(in srgb, #0ea5e9 15%, transparent)' : undefined,
                  }}
                >
                  <Space direction="vertical" size={0} style={{ width: '100%' }}>
                    <Text strong>{s.title}</Text>
                    <Space size={6}>
                      <Tag color="purple">{s.article_count} bài</Tag>
                      <Text type="secondary" style={{ fontSize: 12 }}>
                        {fmtDate(s.first_at)} → {fmtTime(s.last_at)}
                      </Text>
                      {s.has_summary && <Tag color="green">đã tóm tắt</Tag>}
                    </Space>
                  </Space>
                </List.Item>
              )}
            />
          </Col>
          <Col span={15}>
            {selected ? (
              <Card
                size="small"
                title={selected.title}
                extra={
                  <Space size={6}>
                    <Tooltip title={`Dịch tiêu đề, mô tả sang ngôn ngữ hiển thị${selected.display_language ? ` (${selected.display_language})` : ''}`}>
                      <Button
                        size="small"
                        icon={<TranslationOutlined />}
                        loading={transBusy}
                        onClick={translate}
                      >
                        Dịch
                      </Button>
                    </Tooltip>
                    <Button
                      size="small"
                      type="primary"
                      icon={<RobotOutlined />}
                      loading={briefBusy}
                      onClick={() => brief(!!selected.summary)}
                    >
                      {selected.summary ? 'AI tóm tắt lại' : 'AI tóm tắt diễn biến'}
                    </Button>
                  </Space>
                }
              >
                <Space direction="vertical" size={14} style={{ width: '100%' }}>
                  {briefBusy ? (
                    <JobRunningCard label="Đang tóm tắt diễn biến sự kiện" elapsed={briefSec} />
                  ) : (
                    shownText && (
                      <Card
                        size="small"
                        type="inner"
                        title={
                          <Space size={6} wrap>
                            <span>Tóm tắt AI{shownModel ? ` · ${shownModel}` : ''}</span>
                            {shown && (
                              <Tag color="orange">
                                bản {fmtTime(shown.created_at)} · lúc đó {shown.article_count} bài
                              </Tag>
                            )}
                          </Space>
                        }
                        extra={
                          history.length > 1 && (
                            <Space size={6}>
                              <HistoryOutlined style={{ opacity: 0.6 }} />
                              <Select
                                size="small"
                                style={{ width: 210 }}
                                value={pastId ?? 0}
                                onChange={(v) => setPastId(v === 0 ? null : v)}
                                options={[
                                  { value: 0, label: `Bản mới nhất (${history.length} lần)` },
                                  ...history.slice(1).map((h) => ({
                                    value: h.id,
                                    label: `${fmtTime(h.created_at)} · ${h.article_count} bài`,
                                  })),
                                ]}
                              />
                            </Space>
                          )
                        }
                      >
                        <Md text={shownText} truncated={!shown && briefCut === selected.id} />
                      </Card>
                    )
                  )}
                  {!!selected.translated_count && (
                    <Space size={8}>
                      <Tag color="blue">
                        Đã dịch {selected.translated_count} bài sang {selected.display_language}
                      </Tag>
                      <Button size="small" type="link" onClick={() => setShowOriginal(!showOriginal)}>
                        {showOriginal ? 'Xem bản dịch' : 'Xem nguyên bản'}
                      </Button>
                    </Space>
                  )}
                  <Timeline
                    items={(selected.timeline ?? []).map((a) => {
                      const translated = !showOriginal && !!a.title_translated
                      return {
                        children: (
                          <Space direction="vertical" size={0} style={{ width: '100%' }}>
                            <Text type="secondary" style={{ fontSize: 12 }}>
                              {fmtTime(a.published_at)} · {a.source_name}
                            </Text>
                            <Link href={a.url} target="_blank">
                              {translated ? a.title_translated : a.title}
                            </Link>
                            {translated && (
                              <Text type="secondary" style={{ fontSize: 12, fontStyle: 'italic' }}>
                                {a.title}
                              </Text>
                            )}
                            {(translated ? a.description_translated : a.description) && (
                              <Text type="secondary" style={{ fontSize: 13 }}>
                                {(translated ? a.description_translated! : a.description).slice(0, 200)}
                                {(translated ? a.description_translated! : a.description).length > 200 ? '…' : ''}
                              </Text>
                            )}
                          </Space>
                        ),
                      }
                    })}
                  />
                </Space>
              </Card>
            ) : (
              <Empty description="Chọn một dòng sự kiện" />
            )}
          </Col>
        </Row>
      )}
    </Space>
  )
}
