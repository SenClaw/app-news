// Tiny fetch wrapper for the News app REST API (served from the same origin
// in production; proxied to :4640 in `npm run dev`).

async function j<T = any>(url: string, init?: RequestInit): Promise<T> {
  const r = await fetch(url, {
    headers: { 'Content-Type': 'application/json' },
    ...init,
  })
  return (await r.json()) as T
}

const post = (url: string, body?: any) =>
  j(url, { method: 'POST', body: body === undefined ? undefined : JSON.stringify(body) })

export interface Status {
  ok: boolean
  articles_total: number
  articles_24h: number
  sources_active: number
  sources_error: number
  last_fetch_at: string
}

export interface Source {
  id: number
  name: string
  url: string
  category: string
  lang: string
  status: string
  last_fetch_at: string
  last_status: string
  last_error: string
  note: string
  article_count: number
  /** 'feed' = RSS/Atom · 'scrape' = quét link bài viết từ nội dung trang. */
  kind: 'feed' | 'scrape'
}

export interface Topic {
  id: number
  name: string
  keywords: string
  color: string
  article_count: number
}

export interface Analysis {
  summary: string
  sentiment: string
  importance: number
  clickbait: boolean
  reliability: string
  tags: string[]
  model: string
  at: string
}

export interface Article {
  id: number
  source_id: number
  source_name: string
  url: string
  title: string
  description: string
  image_url: string
  author: string
  category: string
  story_id: number | null
  story_size: number
  published_at: string
  has_content: boolean
  /** Chuyên mục do System One (Laya) chọn; rỗng = chưa phân loại / chưa đủ chắc. */
  section: string
  section_conf: number
  content?: string
  fetched_at?: string
  topics?: { id: number; name: string; color: string }[]
  title_translated?: string
  description_translated?: string
  analysis?: Analysis
  related?: Article[]
}

export interface Trend {
  phrase: string
  count: number
  prev_count: number
  score: number
  article_ids: number[]
  samples: { id: number; title: string; source: string; published_at: string; url: string }[]
}

export interface Story {
  id: number
  title: string
  first_at: string
  last_at: string
  article_count: number
  has_summary?: boolean
  summary?: string
  summary_model?: string
  summary_at?: string
  timeline?: Article[]
  /** Các lần tóm tắt trước, mới nhất trước. */
  summaries?: StorySummary[]
  display_language?: string
  translated_count?: number
}

export interface StorySummary {
  id: number
  summary: string
  model: string
  article_count: number
  last_at: string
  created_at: string
}

export interface Dashboard {
  articles_total: number
  articles_24h: number
  sources_active: number
  sources_error: number
  last_fetch_at: string
  per_day: { day: string; count: number }[]
  top_topics: { id: number; name: string; color: string; count: number }[]
  trends: Trend[]
  hot_stories: Story[]
  recent_articles: Article[]
}

export interface GraphNode {
  id: number
  title: string
  first_at: string
  last_at: string
  article_count: number
}

export interface GraphEdge {
  a: number
  b: number
  weight: number
  shared: string[]
}

export interface StoryGraphData {
  days: number
  nodes: GraphNode[]
  edges: GraphEdge[]
  /** Liên kết mạnh nhất giữ lại cho mỗi sự kiện (0 = giữ hết). */
  links_per_story: number
  /** Tổng liên kết máy tìm được, trước khi tỉa cho dễ đọc. */
  edges_total: number
  /** Số liên kết yếu đã ẩn — nói ra, không nuốt im. */
  edges_hidden: number
}

export interface AiLink {
  a: number
  b: number
  relation: string
  why: string
}

export interface AiCluster {
  name: string
  story_ids: number[]
  why: string
}

export interface GraphAnalysis {
  ok?: boolean
  error?: string
  model?: string
  summary?: string
  clusters?: AiCluster[]
  ai_links?: AiLink[]
  noise?: AiLink[]
}

export interface DiscoverResult {
  status: 'ok' | 'error' | 'exists'
  url: string
  input_url?: string
  name: string
  category?: string
  lang?: string
  feed_title?: string
  item_count?: number
  sample?: string[]
  error?: string
  added?: boolean
  kind?: 'feed' | 'scrape'
}

/** Một việc dài hơi đang chạy trên server (sống qua reload / đổi tab). */
export interface Job {
  key: string
  kind: string
  label: string
  started_at: number
  elapsed_sec: number
}

/** Một bản điểm tin đã chạy. `text` chỉ có khi lấy chi tiết. */
export interface DigestRecord {
  id: number
  hours: number
  focus: string
  topic_id: number | null
  topic_name: string
  article_count: number
  model: string
  truncated: boolean
  created_at: string
  preview?: string
  text?: string
}

export interface Settings {
  fetch_interval_min: number
  retention_days: number
  auto_fetch: boolean
  /** Ngôn ngữ hiển thị cuối cùng: AI trả lời và bài dịch đều theo ngôn ngữ này. */
  display_language: string
  /** Bao lâu thì tự gom lại dòng sự kiện (0 = tắt). */
  auto_regroup_hours: number
  /** Dấu hiệu trang điểm tin, mỗi dòng một mẫu (rỗng = dùng mặc định). */
  digest_markers: string
  /** Phân loại chuyên mục + chủ đề bằng System One (Laya). */
  laya_classify: boolean
}

/** Trạng thái bộ phân loại System One. */
export interface LayaStatus {
  enabled: boolean
  model: string
  window_days: number
  classified: number
  pending: number
  last: { state: 'ok' | 'unavailable'; classified: number; error: string | null; at: number } | null
}

export interface SectionCount {
  section: string
  count: number
}

export interface ArticleFilter {
  q?: string
  source_id?: number
  topic_id?: number
  story_id?: number
  category?: string
  section?: string
  hours?: number
  limit?: number
  offset?: number
}

const qs = (params: Record<string, any>) => {
  const p = Object.entries(params)
    .filter(([, v]) => v !== undefined && v !== null && v !== '')
    .map(([k, v]) => `${k}=${encodeURIComponent(String(v))}`)
    .join('&')
  return p ? `?${p}` : ''
}

export const api = {
  status: () => j<Status>('/api/status'),
  dashboard: () => j<Dashboard>('/api/dashboard'),

  sources: (status?: string) => j<{ sources: Source[] }>(`/api/sources${qs({ status })}`),
  addSource: (b: Partial<Source>) => post('/api/sources', b),
  updateSource: (id: number, patch: Partial<Source>) => post(`/api/sources/${id}`, patch),
  deleteSource: (id: number) => post(`/api/sources/${id}/delete`),
  fetchSource: (id: number) => post(`/api/sources/${id}/fetch`),
  fetchAll: () => post('/api/fetch'),

  articles: (f: ArticleFilter) => j<{ articles: Article[] }>(`/api/articles${qs(f as any)}`),
  article: (id: number) => j<{ article?: Article; error?: string }>(`/api/articles/${id}`),
  fetchContent: (id: number) => post(`/api/articles/${id}/content`),
  analyzeArticle: (id: number, force = false, with_content = false) =>
    post(`/api/articles/${id}/analyze`, { force, with_content }),

  topics: () => j<{ topics: Topic[] }>('/api/topics'),
  sections: (hours?: number) =>
    j<{ hours: number; sections: SectionCount[]; laya: LayaStatus }>(`/api/sections${qs({ hours })}`),
  addTopic: (b: Partial<Topic>) => post('/api/topics', b),
  updateTopic: (id: number, patch: Partial<Topic>) => post(`/api/topics/${id}`, patch),
  deleteTopic: (id: number) => post(`/api/topics/${id}/delete`),

  trends: (hours: number) => j<{ hours: number; article_count: number; trends: Trend[] }>(`/api/trends${qs({ hours })}`),
  analyzeTrends: (hours: number) => post('/api/trends/analyze', { hours }),
  trendSummary: (phrase: string, hours: number) => post('/api/trends/summary', { phrase, hours }),

  stories: (days = 7, min = 2, limit = 30) =>
    j<{ stories: Story[] }>(`/api/stories${qs({ days, min_articles: min, limit })}`),
  storyGraph: (days = 7, min = 2, perNode = 3) =>
    j<StoryGraphData>(`/api/stories/graph${qs({ days, min_articles: min, per_node: perNode })}`),
  analyzeGraph: (days: number, min_articles: number, question?: string) =>
    post('/api/stories/graph/analyze', { days, min_articles, question }) as Promise<GraphAnalysis>,
  discoverSources: (query: string, auto_add = false) =>
    post('/api/sources/discover', { query, auto_add }) as Promise<{
      ok?: boolean
      error?: string
      via?: string
      found?: number
      added?: number
      results?: DiscoverResult[]
    }>,
  story: (id: number) => j<{ story?: Story; error?: string }>(`/api/stories/${id}`),
  storyBrief: (id: number, force = false) => post(`/api/stories/${id}/brief`, { force }),
  translateStory: (id: number) => post(`/api/stories/${id}/translate`, {}),
  rebuildStories: () => post('/api/stories/rebuild', {}),

  digest: (b: { hours?: number; focus?: string; topic_id?: number }) => post('/api/digest', b),
  digestHistory: (limit = 30) =>
    j<{ digests: DigestRecord[]; running: Job | null }>(`/api/digests${qs({ limit })}`),
  digestGet: (id: number) => j<{ digest?: DigestRecord; error?: string }>(`/api/digests/${id}`),
  digestDelete: (id: number) => post(`/api/digests/${id}/delete`),
  jobs: () => j<{ jobs: Job[] }>('/api/jobs'),

  settings: () => j<Settings>('/api/settings'),
  saveSettings: (b: Partial<Settings>) => post('/api/settings', b),
  activity: () => j<{ activity: { kind: string; message: string; at: string }[] }>('/api/activity'),
}

export const fmtTime = (iso?: string) => {
  if (!iso) return ''
  const d = new Date(iso)
  if (isNaN(d.getTime())) return iso
  return d.toLocaleString('vi-VN', { hour: '2-digit', minute: '2-digit', day: '2-digit', month: '2-digit' })
}

export const fmtDate = (iso?: string) => {
  if (!iso) return ''
  const d = new Date(iso)
  if (isNaN(d.getTime())) return iso
  return d.toLocaleString('vi-VN', { day: '2-digit', month: '2-digit', year: 'numeric' })
}

export const sentimentTag = (s?: string): { color: string; label: string } => {
  switch (s) {
    case 'positive':
      return { color: 'green', label: 'Tích cực' }
    case 'negative':
      return { color: 'red', label: 'Tiêu cực' }
    case 'mixed':
      return { color: 'orange', label: 'Trái chiều' }
    default:
      return { color: 'default', label: 'Trung lập' }
  }
}
