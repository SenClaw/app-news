import { useEffect, useState } from 'react'
import {
  Badge, Button, Card, Flex, Form, Input, InputNumber, List, message, Modal, Popconfirm, Select,
  Space, Switch, Table, Tag, Tooltip, Typography,
} from 'antd'
import { DeleteOutlined, EditOutlined, PlusOutlined, RobotOutlined, SyncOutlined } from '@ant-design/icons'
import { api, fmtTime, type DiscoverResult, type Settings, type Source, type Topic, type LayaStatus } from './api'

const { Text } = Typography

const COLORS = ['blue', 'gold', 'green', 'red', 'purple', 'cyan', 'magenta', 'orange', 'geekblue', 'volcano']

// ---------------------------------------------------------------------------
// Chủ đề
// ---------------------------------------------------------------------------

export function TopicsTab() {
  const [topics, setTopics] = useState<Topic[]>([])
  const [editing, setEditing] = useState<Partial<Topic> | null>(null)
  const [form] = Form.useForm()

  const load = () => api.topics().then((r) => setTopics(r.topics))
  useEffect(() => {
    load()
  }, [])

  const openModal = (t?: Topic) => {
    setEditing(t ?? {})
    form.setFieldsValue(t ?? { name: '', keywords: '', color: 'blue' })
  }

  const save = async () => {
    const v = await form.validateFields()
    const r = editing?.id ? await api.updateTopic(editing.id, v) : await api.addTopic(v)
    if (r.error) message.error(String(r.error))
    else {
      message.success(
        r.matched !== undefined && r.matched >= 0 ? `Đã lưu — ${r.matched} bài khớp từ khóa` : 'Đã lưu',
      )
      setEditing(null)
      load()
    }
  }

  return (
    <Space direction="vertical" size={12} style={{ width: '100%' }}>
      <Flex justify="space-between">
        <Text type="secondary">
          Bài chứa từ khóa (trong tiêu đề/mô tả) tự động được gán vào chủ đề — kể cả bài đã thu thập trước đó.
        </Text>
        <Button type="primary" icon={<PlusOutlined />} onClick={() => openModal()}>
          Thêm chủ đề
        </Button>
      </Flex>
      <Table
        size="small"
        rowKey="id"
        dataSource={topics}
        pagination={false}
        columns={[
          {
            title: 'Chủ đề',
            dataIndex: 'name',
            width: 200,
            render: (n: string, t) => <Tag color={t.color || 'blue'}>{n}</Tag>,
          },
          { title: 'Từ khóa', dataIndex: 'keywords', ellipsis: true },
          { title: 'Số bài', dataIndex: 'article_count', width: 90 },
          {
            title: '',
            width: 90,
            render: (_, t) => (
              <Space>
                <Button size="small" icon={<EditOutlined />} onClick={() => openModal(t)} />
                <Popconfirm title="Xoá chủ đề này?" onConfirm={() => api.deleteTopic(t.id).then(load)}>
                  <Button size="small" danger icon={<DeleteOutlined />} />
                </Popconfirm>
              </Space>
            ),
          },
        ]}
      />
      <Modal
        open={!!editing}
        title={editing?.id ? 'Sửa chủ đề' : 'Thêm chủ đề'}
        onOk={save}
        onCancel={() => setEditing(null)}
        okText="Lưu"
        cancelText="Huỷ"
      >
        <Form form={form} layout="vertical">
          <Form.Item name="name" label="Tên chủ đề" rules={[{ required: true, message: 'Nhập tên' }]}>
            <Input placeholder="Công nghệ & AI" />
          </Form.Item>
          <Form.Item
            name="keywords"
            label="Từ khóa (cách nhau dấu phẩy)"
            rules={[{ required: true, message: 'Nhập ít nhất một từ khóa' }]}
          >
            <Input.TextArea rows={3} placeholder="AI, trí tuệ nhân tạo, chip, smartphone" />
          </Form.Item>
          <Form.Item name="color" label="Màu">
            <Select options={COLORS.map((c) => ({ value: c, label: <Tag color={c}>{c}</Tag> }))} />
          </Form.Item>
        </Form>
      </Modal>
    </Space>
  )
}

// ---------------------------------------------------------------------------
// Tự tìm nguồn (AI gợi ý theo chủ đề, hoặc dò feed từ một URL)
// ---------------------------------------------------------------------------

function DiscoverPanel({ onAdded }: { onAdded: () => void }) {
  const [query, setQuery] = useState('')
  const [busy, setBusy] = useState(false)
  const [results, setResults] = useState<DiscoverResult[] | null>(null)
  const [via, setVia] = useState('')

  const run = async (autoAdd: boolean) => {
    if (!query.trim()) return
    setBusy(true)
    try {
      const r = await api.discoverSources(query.trim(), autoAdd)
      if (r.error) {
        message.error(String(r.error))
        setResults(null)
      } else {
        setResults(r.results ?? [])
        setVia(r.via ?? '')
        if (autoAdd) {
          message.success(`Đã thêm ${r.added ?? 0}/${r.found ?? 0} nguồn hợp lệ`)
          onAdded()
        } else if (!r.found) {
          message.warning('Không tìm được feed nào hoạt động')
        }
      }
    } finally {
      setBusy(false)
    }
  }

  const addOne = async (r: DiscoverResult) => {
    const res = await api.addSource({
      name: r.name,
      url: r.url,
      category: r.category,
      lang: r.lang,
      kind: r.kind ?? 'feed',
    } as any)
    if (res.error) message.error(String(res.error))
    else {
      message.success(`Đã thêm "${r.name}"`)
      setResults((prev) => prev?.map((x) => (x.url === r.url ? { ...x, added: true } : x)) ?? null)
      onAdded()
    }
  }

  return (
    <Card size="small" title="Tự tìm nguồn">
      <Space direction="vertical" size={10} style={{ width: '100%' }}>
        <Flex gap={8} wrap>
          <Input
            style={{ width: 420 }}
            placeholder="Chủ đề muốn theo dõi (AI gợi ý) hoặc dán URL một trang web"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onPressEnter={() => run(false)}
          />
          <Button type="primary" icon={<RobotOutlined />} loading={busy} onClick={() => run(false)}>
            Tìm nguồn
          </Button>
          <Button loading={busy} onClick={() => run(true)} disabled={!query.trim()}>
            Tìm & thêm luôn
          </Button>
        </Flex>
        <Text type="secondary" style={{ fontSize: 12 }}>
          Ví dụ: <i>"tin công nghệ tiếng Việt"</i> → AI gợi ý feed các trang uy tín · hoặc dán{' '}
          <i>https://vnexpress.net</i> → tự dò feed RSS của trang đó. Trang nào không có RSS thì app tự
          chuyển sang <b>quét nội dung trang</b> để lấy link bài viết. Mọi gợi ý đều được tải thử thật,
          chỉ nguồn thực sự ra bài mới hiện ở đây.
        </Text>

        {results && (
          <List
            size="small"
            dataSource={results}
            locale={{ emptyText: 'Không có kết quả' }}
            renderItem={(r) => (
              <List.Item
                actions={[
                  r.status === 'ok' && !r.added ? (
                    <Button size="small" type="link" onClick={() => addOne(r)}>
                      Thêm
                    </Button>
                  ) : null,
                ].filter(Boolean)}
              >
                <Space direction="vertical" size={0} style={{ width: '100%' }}>
                  <Space size={6} wrap>
                    {r.status === 'ok' ? (
                      r.added ? (
                        <Tag color="green">đã thêm</Tag>
                      ) : (
                        <Tag color="blue">{r.item_count} bài</Tag>
                      )
                    ) : r.status === 'exists' ? (
                      <Tag>đã có</Tag>
                    ) : (
                      <Tag color="red">lỗi</Tag>
                    )}
                    {r.kind === 'scrape' && (
                      <Tooltip title="Trang này không có RSS — sẽ thu thập bằng cách quét link bài viết trong nội dung trang">
                        <Tag color="orange">quét trang</Tag>
                      </Tooltip>
                    )}
                    <Text strong>{r.name || r.url}</Text>
                    {r.category && <Tag color="cyan">{r.category}</Tag>}
                  </Space>
                  <Text type="secondary" style={{ fontSize: 12 }}>
                    {r.url}
                  </Text>
                  {r.status === 'error' && (
                    <Text type="danger" style={{ fontSize: 12 }}>
                      {r.error}
                    </Text>
                  )}
                  {r.sample?.length ? (
                    <Text type="secondary" style={{ fontSize: 12 }}>
                      Bài mẫu: {r.sample.slice(0, 2).join(' · ')}
                    </Text>
                  ) : null}
                </Space>
              </List.Item>
            )}
          />
        )}
        {results && via === 'ai' && (
          <Text type="secondary" style={{ fontSize: 12 }}>
            Gợi ý do AI đưa ra và đã được app tải thử — nguồn không truy cập được đã bị loại.
          </Text>
        )}
      </Space>
    </Card>
  )
}

// ---------------------------------------------------------------------------
// Nguồn tin + cài đặt + nhật ký
// ---------------------------------------------------------------------------

export function SourcesTab({ onChange }: { onChange: () => void }) {
  const [sources, setSources] = useState<Source[]>([])
  const [editing, setEditing] = useState<Partial<Source> | null>(null)
  const [fetchingId, setFetchingId] = useState<number | null>(null)
  const [settings, setSettings] = useState<Settings | null>(null)
  const [activity, setActivity] = useState<{ kind: string; message: string; at: string }[]>([])
  const [regrouping, setRegrouping] = useState(false)
  const [laya, setLaya] = useState<LayaStatus | null>(null)
  const [form] = Form.useForm()

  const load = () => {
    api.sources().then((r) => setSources(r.sources))
    api.settings().then(setSettings)
    api.activity().then((r) => setActivity(r.activity))
    api.sections().then((r) => setLaya(r.laya))
  }
  useEffect(() => {
    load()
  }, [])

  const openModal = (s?: Source) => {
    setEditing(s ?? {})
    form.setFieldsValue(s ?? { name: '', url: '', category: '', lang: 'vi', note: '', kind: 'feed' })
  }

  const save = async () => {
    const v = await form.validateFields()
    const r = editing?.id ? await api.updateSource(editing.id, v) : await api.addSource(v)
    if (r.error) message.error(String(r.error))
    else {
      message.success('Đã lưu nguồn')
      setEditing(null)
      load()
      onChange()
    }
  }

  const fetchOne = async (id: number) => {
    setFetchingId(id)
    try {
      const r = await api.fetchSource(id)
      if (r.error) message.error(String(r.error))
      else message.success(`+${r.new} bài mới${r.not_modified ? ' (feed chưa đổi)' : ''}`)
      load()
      onChange()
    } finally {
      setFetchingId(null)
    }
  }

  const saveSettings = async (patch: Partial<Settings>) => {
    const s = await api.saveSettings(patch)
    setSettings(s)
    message.success('Đã lưu cài đặt')
  }

  const regroupNow = async () => {
    setRegrouping(true)
    try {
      const r = await api.rebuildStories()
      if (r.error) message.error(String(r.error))
      else
        message.success(
          `Đã gom lại: ${r.multi_article_stories} dòng sự kiện từ ${r.articles} bài` +
            (r.skipped_digest ? ` (bỏ qua ${r.skipped_digest} trang điểm tin)` : ''),
        )
      onChange()
    } finally {
      setRegrouping(false)
    }
  }

  return (
    <Space direction="vertical" size={14} style={{ width: '100%' }}>
      <Flex justify="space-between">
        <Text type="secondary">
          Nguồn là feed RSS/Atom, hoặc một trang chuyên mục thường (app tự quét link bài viết trong nội dung
          trang khi trang đó không có RSS). App tự quét định kỳ; bài trùng bị bỏ qua, bài mới tự gán chủ đề +
          gom sự kiện.
        </Text>
        <Button type="primary" icon={<PlusOutlined />} onClick={() => openModal()}>
          Thêm nguồn
        </Button>
      </Flex>

      <DiscoverPanel
        onAdded={() => {
          load()
          onChange()
        }}
      />

      <Table
        size="small"
        rowKey="id"
        dataSource={sources}
        pagination={false}
        columns={[
          {
            title: 'Nguồn',
            dataIndex: 'name',
            render: (n: string, s) => (
              <Space direction="vertical" size={0}>
                <Space size={6}>
                  <Text strong>{n}</Text>
                  {s.kind === 'scrape' && (
                    <Tooltip title="Trang không có RSS — app quét link bài viết ngay trong nội dung trang">
                      <Tag color="orange">quét trang</Tag>
                    </Tooltip>
                  )}
                </Space>
                <Text type="secondary" style={{ fontSize: 12 }} ellipsis>
                  {s.url}
                </Text>
              </Space>
            ),
          },
          { title: 'Nhóm', dataIndex: 'category', width: 110, render: (c: string) => c && <Tag>{c}</Tag> },
          { title: 'Bài', dataIndex: 'article_count', width: 70 },
          {
            title: 'Lần quét cuối',
            dataIndex: 'last_fetch_at',
            width: 160,
            render: (v: string, s) =>
              s.last_status === 'error' ? (
                <Tooltip title={s.last_error}>
                  <Badge status="error" text={<Text type="danger">lỗi · {fmtTime(v)}</Text>} />
                </Tooltip>
              ) : v && !v.startsWith('1970') ? (
                <Badge status="success" text={fmtTime(v)} />
              ) : (
                <Text type="secondary">chưa quét</Text>
              ),
          },
          {
            title: 'Quét',
            dataIndex: 'status',
            width: 80,
            render: (st: string, s) => (
              <Switch
                size="small"
                checked={st === 'active'}
                onChange={(on) =>
                  api.updateSource(s.id, { status: on ? 'active' : 'paused' } as any).then(() => {
                    load()
                    onChange()
                  })
                }
              />
            ),
          },
          {
            title: '',
            width: 130,
            render: (_, s) => (
              <Space>
                <Tooltip title="Quét nguồn này ngay">
                  <Button
                    size="small"
                    icon={<SyncOutlined />}
                    loading={fetchingId === s.id}
                    onClick={() => fetchOne(s.id)}
                  />
                </Tooltip>
                <Button size="small" icon={<EditOutlined />} onClick={() => openModal(s)} />
                <Popconfirm
                  title={`Xoá nguồn và toàn bộ ${s.article_count} bài của nó?`}
                  onConfirm={() =>
                    api.deleteSource(s.id).then(() => {
                      load()
                      onChange()
                    })
                  }
                >
                  <Button size="small" danger icon={<DeleteOutlined />} />
                </Popconfirm>
              </Space>
            ),
          },
        ]}
      />

      {settings && (
        <Card size="small" title="Cài đặt thu thập">
          <Space size={20} wrap>
            <Space>
              <Text>Tự động quét</Text>
              <Switch checked={settings.auto_fetch} onChange={(v) => saveSettings({ auto_fetch: v })} />
            </Space>
            <Space>
              <Text>Chu kỳ (phút)</Text>
              <InputNumber
                min={5}
                max={1440}
                value={settings.fetch_interval_min}
                onChange={(v) => v && saveSettings({ fetch_interval_min: v })}
              />
            </Space>
            <Space>
              <Text>Giữ bài (ngày)</Text>
              <InputNumber
                min={3}
                max={365}
                value={settings.retention_days}
                onChange={(v) => v && saveSettings({ retention_days: v })}
              />
            </Space>
          </Space>
        </Card>
      )}

      {settings && (
        <Card size="small" title="Phân loại bằng System One (Laya)">
          <Space direction="vertical" size={10} style={{ width: '100%' }}>
            <Space wrap>
              <Text>Bật phân loại</Text>
              <Switch
                checked={settings.laya_classify}
                onChange={(v) => saveSettings({ laya_classify: v })}
              />
              {laya && <LayaState s={laya} />}
            </Space>
            <Text type="secondary" style={{ fontSize: 12 }}>
              Model quyết định chạy trên máy (runtime sen-sysone) xếp mỗi bài trong{' '}
              {laya?.window_days ?? 7} ngày gần nhất vào một chuyên mục, và gán lại chủ đề khi đủ
              chắc. Chưa cài sen-sysone hoặc chưa tải model thì chủ đề vẫn gán theo từ khóa như cũ.
              Sửa chủ đề thì các bài trong cửa sổ được phân loại lại.
            </Text>
          </Space>
        </Card>
      )}

      {settings && (
        <Card size="small" title="Ngôn ngữ hiển thị">
          <Space direction="vertical" size={10} style={{ width: '100%' }}>
            <Space wrap>
              <Text>Ngôn ngữ cuối cùng</Text>
              <Select
                style={{ width: 200 }}
                value={settings.display_language}
                onChange={(v) => saveSettings({ display_language: v })}
                options={[
                  { value: 'Tiếng Việt', label: 'Tiếng Việt' },
                  { value: 'English', label: 'English' },
                  { value: '中文', label: '中文' },
                  { value: '日本語', label: '日本語' },
                  { value: '한국어', label: '한국어' },
                  { value: 'Français', label: 'Français' },
                  { value: 'Español', label: 'Español' },
                  { value: 'Deutsch', label: 'Deutsch' },
                  { value: 'ไทย', label: 'ไทย' },
                ]}
              />
            </Space>
            <Text type="secondary" style={{ fontSize: 12 }}>
              Nguồn giữ nguyên ngôn ngữ gốc. AI trả lời (tóm tắt, điểm tin, nhận định) bằng ngôn
              ngữ này, và nút “Dịch” ở mỗi dòng sự kiện dịch tiêu đề/mô tả sang đây — bản gốc vẫn
              hiện bên dưới.
            </Text>
          </Space>
        </Card>
      )}

      {settings && (
        <Card size="small" title="Gom dòng sự kiện">
          <Space direction="vertical" size={10} style={{ width: '100%' }}>
            <Space wrap>
              <Text>Tự gom lại mỗi (giờ)</Text>
              <InputNumber
                min={0}
                max={720}
                value={settings.auto_regroup_hours}
                onChange={(v) => saveSettings({ auto_regroup_hours: v ?? 0 })}
              />
              <Text type="secondary" style={{ fontSize: 12 }}>0 = tắt</Text>
              <Button size="small" loading={regrouping} onClick={regroupNow}>
                Gom lại ngay
              </Button>
            </Space>
            <Text type="secondary" style={{ fontSize: 12 }}>
              Bài mới được xếp vào dòng sự kiện đang có tại thời điểm nó về; gom lại toàn bộ kho
              theo chu kỳ giúp sửa những phán đoán sớm bị sai.
            </Text>
            <Space direction="vertical" size={4} style={{ width: '100%' }}>
              <Text>Dấu hiệu trang “điểm tin” (mỗi dòng một mẫu, để trống = mặc định)</Text>
              <Input.TextArea
                rows={4}
                defaultValue={settings.digest_markers}
                onBlur={(e) => saveSettings({ digest_markers: e.target.value })}
                placeholder={'điểm tin\nbản tin\ntoàn cảnh\n24h qua'}
              />
              <Text type="secondary" style={{ fontSize: 12 }}>
                Bài có tiêu đề khớp các mẫu này là trang tổng hợp nhiều tin, không phải một sự
                kiện — vẫn lưu và tìm kiếm được, chỉ không gom vào dòng sự kiện nào.
              </Text>
            </Space>
          </Space>
        </Card>
      )}

      <Card size="small" title="Hoạt động gần đây">
        <List
          size="small"
          dataSource={activity}
          locale={{ emptyText: 'Chưa có hoạt động' }}
          renderItem={(a) => (
            <List.Item>
              <Space>
                <Tag>{a.kind}</Tag>
                <Text>{a.message}</Text>
                <Text type="secondary" style={{ fontSize: 12 }}>{fmtTime(a.at)}</Text>
              </Space>
            </List.Item>
          )}
        />
      </Card>

      <Modal
        open={!!editing}
        title={editing?.id ? 'Sửa nguồn' : 'Thêm nguồn'}
        onOk={save}
        onCancel={() => setEditing(null)}
        okText="Lưu"
        cancelText="Huỷ"
      >
        <Form form={form} layout="vertical">
          <Form.Item name="kind" label="Kiểu nguồn">
            <Select
              options={[
                { value: 'feed', label: 'Feed RSS/Atom — chính xác và nhẹ nhất' },
                { value: 'scrape', label: 'Quét nội dung trang — cho trang không có RSS' },
              ]}
            />
          </Form.Item>
          <Form.Item noStyle shouldUpdate={(a, b) => a.kind !== b.kind}>
            {({ getFieldValue }) =>
              getFieldValue('kind') === 'scrape' ? (
                <Form.Item
                  name="url"
                  label="URL trang chuyên mục cần quét"
                  extra="Trỏ vào trang danh sách bài (ví dụ trang chuyên mục), không phải trang bài lẻ. App đọc HTML để lấy link bài viết, rồi mở từng bài mới để lấy tiêu đề/tóm tắt/ngày đăng. Trang chỉ hiện bài bằng JavaScript sẽ không quét được."
                  rules={[{ required: true, pattern: /^https?:\/\//, message: 'URL phải bắt đầu bằng http(s)://' }]}
                >
                  <Input placeholder="https://trang-tin.vn/thoi-su" />
                </Form.Item>
              ) : (
                <Form.Item
                  name="url"
                  label="URL feed (RSS/Atom)"
                  rules={[{ required: true, pattern: /^https?:\/\//, message: 'URL phải bắt đầu bằng http(s)://' }]}
                >
                  <Input placeholder="https://vnexpress.net/rss/tin-moi-nhat.rss" />
                </Form.Item>
              )
            }
          </Form.Item>
          <Form.Item name="name" label="Tên hiển thị (bỏ trống = lấy tên từ feed/trang)">
            <Input placeholder="VnExpress" />
          </Form.Item>
          <Form.Item name="category" label="Nhóm nguồn">
            <Input placeholder="Tổng hợp / Công nghệ / Kinh doanh…" />
          </Form.Item>
          <Form.Item name="lang" label="Ngôn ngữ">
            <Select options={[{ value: 'vi' }, { value: 'en' }, { value: '' , label: 'khác' }]} />
          </Form.Item>
          <Form.Item name="note" label="Ghi chú">
            <Input />
          </Form.Item>
        </Form>
      </Modal>
    </Space>
  )
}

/** Một dòng trạng thái của bộ phân loại System One. */
function LayaState({ s }: { s: LayaStatus }) {
  if (!s.enabled) return <Tag>đang tắt</Tag>
  if (s.last?.state === 'unavailable')
    return (
      <Tooltip title={s.last.error ?? ''}>
        <Tag color="orange">System One chưa sẵn sàng — đang dùng từ khóa</Tag>
      </Tooltip>
    )
  return (
    <Tag color={s.pending > 0 ? 'processing' : 'green'}>
      {s.pending > 0
        ? `đã phân loại ${s.classified} · còn ${s.pending} bài`
        : `đã phân loại ${s.classified} bài`}
    </Tag>
  )
}
