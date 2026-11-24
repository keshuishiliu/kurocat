import './stats.css';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import {
  renderBars,
  renderHBars,
  renderLine,
  redrawAllCharts,
  fmtNum,
  fmtDuration,
  fmtBytes,
  setChartTheme,
  type Point,
} from './statsChart';
import {
  appCountByCategory,
  categoryForApp,
  createCategory,
  deleteCategory,
  init as initCategories,
  listCategories,
  rollupByCategory,
  setAppCategory,
  subscribe,
  updateCategory,
  type Category,
} from './categories';
import { TrailPlayer, type TrailPoint } from './trail';

// --- 后端契约（对应 src-tauri/src/recorder.rs 的 Serialize 结构，camelCase） ---

interface LogStatus {
  enabled: boolean;
  paused: boolean;
  recording: boolean;
  logDir: string;
  totalBytes: number;
  totalEvents: number;
  todayKeydown: number;
  todayMousedown: number;
  activeSecondsToday: number;
  firstDay: string | null;
  lastDay: string | null;
  monthTables: number;
  appCount: number;
  retentionDays: number;
  activeGapMinutes: number;
  trailEnabled: boolean;
}

interface SummaryPoint {
  label: string;
  keydown: number;
  mousedown: number;
  activeSeconds: number;
}

interface RetentionPreview {
  cutoffDay: string;
  cutoffTs: number;
  rows: number;
  months: string[];
}

interface RetentionReport {
  cutoffDay: string;
  deletedRows: number;
  droppedMonths: string[];
}

interface ExportResult {
  path: string;
  rows: number;
}

interface TrailRangeInfo {
  firstTs: number | null;
  lastTs: number | null;
  points: number;
}

type SummaryDim = 'day' | 'month' | 'year' | 'hour' | 'app' | 'key';

const LS_CONSENT = 'kurocat.inputLog.consent.v1';
const LS_TRAIL_CONSENT = 'kurocat.inputLog.trailConsent.v1';
const LS_THEME = 'kurocat.stats.theme.v1';

/** 在普通浏览器里打开 stats.html 时走演示数据，方便调 UI；在 Tauri 里永远走真后端。 */
const HAS_TAURI = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window;
let demoEnabled = false;

function demoStatus(): LogStatus {
  return {
    enabled: demoEnabled,
    paused: false,
    recording: demoEnabled,
    logDir: 'C:\\Users\\you\\AppData\\Roaming\\kuro-cat-pet\\input-log',
    totalBytes: 186_432_512,
    totalEvents: 1_284_392,
    todayKeydown: 12_486,
    todayMousedown: 1_342,
    activeSecondsToday: 9_840,
    firstDay: '2026-08-03',
    lastDay: '2026-11-24',
    monthTables: 4,
    appCount: 37,
    retentionDays: 0,
    activeGapMinutes: 5,
    trailEnabled: true,
  };
}

function demoSummary(dim: string): SummaryPoint[] {
  const wave = (i: number) => Math.round(2800 + Math.sin(i * 1.7) * 1600 + (i % 6) * 420);
  if (dim === 'hour') {
    const curve = [40, 15, 8, 5, 6, 30, 320, 1800, 4200, 6100, 5800, 4900, 3200, 4600, 6400, 7100, 6800, 5400, 3200, 2600, 3100, 2400, 1100, 380];
    return curve.map((v, h) => ({
      label: String(h).padStart(2, '0'),
      keydown: v,
      mousedown: 0,
      activeSeconds: 0,
    }));
  }
  if (dim === 'app') {
    const apps: Array<[string, number]> = [
      ['Code.exe', 48210],
      ['chrome.exe', 31844],
      ['WeChat.exe', 21090],
      ['KuroCat.exe', 18730],
      ['WindowsTerminal.exe', 15220],
      ['explorer.exe', 9840],
      ['WINWORD.EXE', 7420],
      ['Notepad.exe', 5210],
      ['Spotify.exe', 3180],
      ['Photoshop.exe', 2140],
    ];
    return apps.map(([label, keydown]) => ({ label, keydown, mousedown: 0, activeSeconds: 0 }));
  }
  if (dim === 'key') {
    const keys: Array<[string, number]> = [
      ['Space', 18220],
      ['E', 14100],
      ['A', 12380],
      ['Backspace', 11240],
      ['Enter', 9860],
      ['S', 8420],
      ['T', 7910],
      ['R', 7230],
      ['LShift', 6840],
      ['O', 6120],
      ['I', 5940],
      ['N', 5210],
      ['LCtrl', 4980],
      ['Esc', 2140],
      ['F5', 1420],
    ];
    return keys.map(([label, keydown]) => ({ label, keydown, mousedown: 0, activeSeconds: 0 }));
  }
  const out: SummaryPoint[] = [];
  const today = new Date();
  for (let i = 29; i >= 0; i--) {
    const d = new Date(today);
    d.setDate(d.getDate() - i);
    out.push({
      label: `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, '0')}-${String(d.getDate()).padStart(2, '0')}`,
      keydown: wave(i),
      mousedown: Math.round(wave(i) / 9),
      activeSeconds: wave(i) / 3,
    });
  }
  return out;
}

/**
 * 演示轨迹：几段「鼠标写字」，段与段之间是空闲。
 *
 * 注意点间距要贴近真实采样（约 100 毫秒）：如果均匀铺满整个时间范围，
 * 相邻两点动不动就隔几十秒，会被断线逻辑判成「中间没记录」而全部断开。
 */
function demoTrail(from: number, to: number): TrailPoint[] {
  const out: TrailPoint[] = [];
  const span = Math.max(60_000, to - from);
  const bursts = 48;
  const perBurst = 62;
  const stepMs = 100;
  for (let b = 0; b < bursts; b++) {
    const startT = from + (span * b) / bursts;
    for (let i = 0; i < perBurst; i++) {
      const p = i / (perBurst - 1);
      const t = Math.min(to, startT + i * stepMs);
      const x =
        960 + Math.sin(p * 6.2 + b) * (280 + Math.sin(b * 0.7) * 120) + Math.cos(p * 12 + b) * 30;
      const y =
        540 + Math.cos(p * 5.4 + b * 0.9) * (180 + Math.cos(b * 1.3) * 80) + Math.sin(p * 9 + b) * 26;
      out.push({ t: Math.round(t), x: Math.round(x), y: Math.round(y) });
    }
  }
  return out;
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (HAS_TAURI) return invoke<T>(cmd, args);
  switch (cmd) {
    case 'input_log_stats':
      return demoStatus() as unknown as T;
    case 'input_log_configure':
    case 'input_log_set_trail':
      demoEnabled = Boolean(args?.enabled ?? demoEnabled);
      return demoStatus() as unknown as T;
    case 'input_log_summary':
      return demoSummary(String(args?.dim ?? 'day')) as unknown as T;
    case 'input_log_retention_preview':
      return { cutoffDay: '2026-08-26', cutoffTs: 0, rows: 1_284_392, months: ['202609'] } as unknown as T;
    case 'input_log_apply_retention':
      return { cutoffDay: '', deletedRows: 0, droppedMonths: [] } as unknown as T;
    case 'input_log_clear':
      return undefined as unknown as T;
    case 'input_log_export':
      return { path: 'C:\\Users\\you\\Desktop\\kurocat-input-demo.csv', rows: 24_880 } as unknown as T;
    case 'input_log_trail_range':
      return { firstTs: Date.now() - 86_400_000, lastTs: Date.now(), points: 2400 } as unknown as T;
    case 'input_log_trail_points':
      return demoTrail(Number(args?.from ?? Date.now() - 86_400_000), Number(args?.to ?? Date.now())) as unknown as T;
    default:
      throw new Error(`演示模式不支持 ${cmd}`);
  }
}

function $<T extends HTMLElement>(id: string): T {
  return document.getElementById(id) as unknown as T;
}

let status: LogStatus | null = null;
let currentRange = 7;
let pendingRetentionDays: number | null = null;
let toastTimer = 0;
let allApps: SummaryPoint[] = [];
let appFilter = '';

// ---------------------------------------------------------------- 主题

type ThemeMode = 'dark' | 'light' | 'system';

function readThemeMode(): ThemeMode {
  const raw = localStorage.getItem(LS_THEME);
  // 默认深色：这个窗口原本就是深色的，换肤只是「可选」，不该悄悄改掉老用户的观感
  return raw === 'light' || raw === 'system' || raw === 'dark' ? raw : 'dark';
}

function systemTheme(): 'dark' | 'light' {
  return window.matchMedia && window.matchMedia('(prefers-color-scheme: light)').matches ? 'light' : 'dark';
}

function applyTheme(): void {
  const mode = readThemeMode();
  const effective = mode === 'system' ? systemTheme() : mode;
  document.documentElement.dataset.theme = effective;
  setChartTheme(effective);
  document.querySelectorAll<HTMLButtonElement>('#theme-row .chip').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.theme === mode);
  });
  trail?.refreshTheme();
  // 图表是 SVG 字符串，换肤后需要重画
  if (document.getElementById('page-stats')?.classList.contains('active')) void loadStats();
}

function setThemeMode(mode: ThemeMode): void {
  localStorage.setItem(LS_THEME, mode);
  applyTheme();
}

function bindSystemTheme(): void {
  if (!window.matchMedia) return;
  const mq = window.matchMedia('(prefers-color-scheme: light)');
  const onChange = () => {
    if (readThemeMode() === 'system') applyTheme();
  };
  if (typeof mq.addEventListener === 'function') mq.addEventListener('change', onChange);
  else if (typeof mq.addListener === 'function') mq.addListener(onChange);
}

// ---------------------------------------------------------------- 工具

function dayStr(d: Date): string {
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  return `${y}-${m}-${day}`;
}

function daysAgoInclusive(n: number): string {
  const d = new Date();
  d.setDate(d.getDate() - (n - 1));
  return dayStr(d);
}

function timeStr(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

function toInputValue(ms: number): string {
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, '0');
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}T${p(d.getHours())}:${p(d.getMinutes())}`;
}

function fromInputValue(value: string): number {
  const ms = new Date(value).getTime();
  return Number.isFinite(ms) ? ms : 0;
}

function toast(message: string): void {
  const el = $('toast');
  el.textContent = message;
  el.classList.add('show');
  window.clearTimeout(toastTimer);
  toastTimer = window.setTimeout(() => el.classList.remove('show'), 2600);
}

function showModal(id: string): void {
  $(id).classList.add('show');
}

function hideModal(id: string): void {
  $(id).classList.remove('show');
}

async function guard<T>(task: () => Promise<T>): Promise<T | null> {
  try {
    return await task();
  } catch (err) {
    toast(String(err));
    return null;
  }
}

// ---------------------------------------------------------------- 仪表盘

function renderStatus(st: LogStatus): void {
  status = st;

  const pill = $('dash-state');
  if (st.recording) {
    pill.textContent = '记录中';
    pill.className = 'pill on';
  } else if (st.enabled && st.paused) {
    pill.textContent = '已暂停';
    pill.className = 'pill paused';
  } else {
    pill.textContent = '未开启';
    pill.className = 'pill off';
  }

  const dot = $('rec-dot');
  dot.className = st.recording ? 'brand-dot on' : st.paused ? 'brand-dot paused' : 'brand-dot';

  $('dash-keys').textContent = fmtNum(st.todayKeydown);
  $('dash-clicks').textContent = fmtNum(st.todayMousedown);
  $('dash-active').innerHTML = `${Math.round(st.activeSecondsToday / 60)}<small>分钟</small>`;
  $('dash-active-hint').textContent =
    st.activeSecondsToday > 0
      ? `连续操作约 ${fmtDuration(st.activeSecondsToday)}`
      : '连续操作覆盖的时长';

  $('dash-size').textContent = fmtBytes(st.totalBytes);
  $('dash-first').textContent = st.firstDay ?? '—';
  $('dash-retention').textContent = st.retentionDays === 0 ? '永久保留' : `${st.retentionDays} 天`;
  $('dash-total').textContent = fmtNum(st.totalEvents);
  $('dash-tables').textContent = `${st.monthTables} 张`;
  $('dash-apps').textContent = `${st.appCount} 个`;
  $('dash-gap').textContent = `${st.activeGapMinutes} 分钟`;

  $('dash-today').textContent = dayStr(new Date());
  $('brand-sub').textContent = st.recording ? '正在记录' : '输入统计';
  // 侧栏只显示末尾两段，避免在单词中间折断（完整路径在设置页）
  const sep = st.logDir.includes('\\') ? '\\' : '/';
  const parts = st.logDir.split(/[\\/]/).filter(Boolean);
  $('sidebar-foot').textContent =
    parts.length > 2 ? `…${sep}${parts.slice(-2).join(sep)}` : st.logDir;

  const note = $('dash-note');
  if (!st.enabled) {
    note.hidden = false;
    note.innerHTML =
      '<strong>还没有开始记录。</strong>到「设置」里打开「记录输入」，或直接点下面的开关。' +
      '开启前会先说明记什么、不记什么。';
  } else if (st.paused) {
    note.hidden = false;
    note.innerHTML = '<strong>记录已暂停。</strong>开关仍处于开启状态，恢复后继续写入，暂停期间不会有任何数据。';
  } else {
    note.hidden = true;
  }

  // 设置页控件同步
  ($('sw-enabled') as HTMLInputElement).checked = st.enabled;
  ($('sw-paused') as HTMLInputElement).checked = st.paused;
  ($('sw-paused') as HTMLInputElement).disabled = !st.enabled;
  ($('sw-trail') as HTMLInputElement).checked = st.trailEnabled;
  ($('sw-trail') as HTMLInputElement).disabled = !st.enabled;
  const gapSelect = $('select-gap') as HTMLSelectElement;
  const wanted = String(st.activeGapMinutes);
  if (!Array.from(gapSelect.options).some((o) => o.value === wanted)) {
    const opt = document.createElement('option');
    opt.value = wanted;
    opt.textContent = `${st.activeGapMinutes} 分钟`;
    gapSelect.appendChild(opt);
  }
  gapSelect.value = wanted;
  $('retention-current').textContent =
    st.retentionDays === 0 ? '永久保留（不自动删除任何数据）' : `保留最近 ${st.retentionDays} 天`;
  $('retention-size').textContent = fmtBytes(st.totalBytes);
  $('log-dir').textContent = st.logDir;
  ($('input-retention') as HTMLInputElement).value = st.retentionDays === 0 ? '' : String(st.retentionDays);
  $('consent-retention').textContent = st.retentionDays === 0 ? '永久' : `${st.retentionDays} 天`;
  document.querySelectorAll<HTMLButtonElement>('#range-row .chip').forEach((btn) => {
    btn.classList.toggle('active', Number(btn.dataset.range) === currentRange);
  });
  updateTrailAvailability();
}

async function refreshStatus(): Promise<void> {
  const st = await guard(() => call<LogStatus>('input_log_stats'));
  if (st) renderStatus(st);
}

// ---------------------------------------------------------------- 统计页

function shortLabel(raw: string, dim: SummaryDim): string {
  if (dim === 'day' && raw.length === 10) return raw.slice(5);
  if (dim === 'month' && raw.length >= 7) return raw.slice(2);
  return raw;
}

function sumOf(rows: SummaryPoint[]): number {
  return rows.reduce((acc, r) => acc + r.keydown, 0);
}

async function loadStats(): Promise<void> {
  const to = dayStr(new Date());
  const dim: SummaryDim = currentRange > 0 && currentRange <= 92 ? 'day' : 'month';
  let from: string;
  if (currentRange === 0) {
    from = status?.firstDay ?? to;
  } else {
    from = daysAgoInclusive(currentRange);
  }

  const [trendRaw, hoursRaw, appsRaw, keysRaw] = await Promise.all([
    guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim })),
    guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim: 'hour' })),
    guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim: 'app' })),
    guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim: 'key' })),
  ]);
  const trend = trendRaw ?? [];
  const hours = hoursRaw ?? [];
  const apps = appsRaw ?? [];
  const keys = keysRaw ?? [];

  const trendPoints: Point[] = trend.map((r) => ({
    label: shortLabel(r.label, dim),
    value: r.keydown,
  }));
  const trendHost = $('chart-trend');
  if (trendPoints.length > 20) {
    renderLine(trendHost, trendPoints, { height: 200, maxLabels: 9 });
  } else {
    renderBars(trendHost, trendPoints, { height: 200, maxLabels: 12 });
  }
  $('trend-sum').textContent = trendPoints.length ? `${fmtNum(sumOf(trend))} 次` : '—';

  // 整行宽之后 24 个小时桶每格有二十多像素，标签可以按小时一格给，不用再抽稀
  renderBars(
    $('chart-hour'),
    hours.map((r) => ({ label: r.label, value: r.keydown })),
    { height: 168, maxLabels: 24 },
  );
  $('hour-sum').textContent = hours.length ? `${fmtNum(sumOf(hours))} 次` : '—';

  const appPoints: Point[] = apps
    .filter((r) => r.keydown > 0)
    .slice(0, 10)
    .map((r) => ({ label: r.label, value: r.keydown }));
  renderHBars($('chart-app'), appPoints, { labelWidth: 200 });
  $('app-sum').textContent = appPoints.length ? `前 ${appPoints.length}` : '—';

  // 分类汇总：子分类并入顶层，颜色直接取分类色
  const rolled = rollupByCategory(apps.map((r) => ({ app: r.label, value: r.keydown })));
  const catPoints: Point[] = rolled.map((r) => ({
    label: `${r.category.icon} ${r.category.name}`,
    value: r.value,
  }));
  renderHBars($('chart-cat'), catPoints, {
    labelWidth: 118,
    colors: rolled.map((r) => r.category.color),
  });
  $('cat-sum').textContent = catPoints.length ? `${catPoints.length} 类` : '—';

  const unmapped = apps.filter((r) => r.keydown > 0 && categoryForApp(r.label).id === 'uncat').length;
  const noteEl = $('stats-cat-note');
  if (unmapped > 0) {
    noteEl.hidden = false;
    noteEl.innerHTML =
      `<strong>还有 ${unmapped} 个程序没有归类。</strong>` +
      '它们被算进了「未分类」。到「设置 → 应用分类」认一下，分类统计会更准。';
  } else {
    noteEl.hidden = true;
  }

  const keyPoints: Point[] = keys
    .filter((r) => r.keydown > 0)
    .slice(0, 15)
    .map((r) => ({ label: r.label, value: r.keydown }));
  renderHBars($('chart-key'), keyPoints, { labelWidth: 96 });
  $('key-sum').textContent = keyPoints.length ? `前 ${keyPoints.length}` : '—';
}

// ---------------------------------------------------------------- 分类

let editingCatId: string | null = null;
let pendingDeleteCatId: string | null = null;

function renderCategoryList(): void {
  const host = $('cat-list');
  const counts = appCountByCategory();
  const cats = listCategories();
  const roots = cats.filter((c) => !c.parentId);
  const rows: string[] = [];

  const pushRow = (c: Category, depth: number) => {
    const n = counts[c.id] ?? 0;
    rows.push(
      `<div class="cat-row${depth ? ' child' : ''}" data-cat="${escapeHtml(c.id)}">` +
        `<span class="cat-icon">${escapeHtml(c.icon)}</span>` +
        `<span class="cat-dot" style="background:${escapeHtml(c.color)}"></span>` +
        `<span class="cat-name">${escapeHtml(c.name)}</span>` +
        `<span class="cat-count">${n} 个应用</span>` +
        '<span class="cat-ops">' +
        `<button class="cat-op" data-act="edit" data-id="${escapeHtml(c.id)}" type="button">编辑</button>` +
        (c.builtin
          ? ''
          : `<button class="cat-op danger" data-act="delete" data-id="${escapeHtml(c.id)}" type="button">删除</button>`) +
        '</span></div>',
    );
    for (const child of cats.filter((x) => x.parentId === c.id)) pushRow(child, depth + 1);
  };

  for (const root of roots) pushRow(root, 0);
  host.innerHTML = rows.join('');
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (ch) => {
    switch (ch) {
      case '&':
        return '&amp;';
      case '<':
        return '&lt;';
      case '>':
        return '&gt;';
      case '"':
        return '&quot;';
      default:
        return '&#39;';
    }
  });
}

function openCategoryModal(id: string | null): void {
  editingCatId = id;
  const cats = listCategories();
  const current = id ? cats.find((c) => c.id === id) : undefined;
  $('cat-modal-title').textContent = current ? '编辑分类' : '新增分类';
  ($('cat-icon') as HTMLInputElement).value = current?.icon ?? '🏷️';
  ($('cat-name') as HTMLInputElement).value = current?.name ?? '';
  ($('cat-color') as HTMLInputElement).value = current?.color ?? '#8b5cf6';
  ($('cat-color-picker') as HTMLInputElement).value = current?.color ?? '#8b5cf6';

  // 上级分类：排除自己与自己的子孙，未分类不能当父级
  const banned = new Set<string>();
  if (current) {
    banned.add(current.id);
    let guard = 0;
    let changed = true;
    while (changed && guard++ < 8) {
      changed = false;
      for (const c of cats) {
        if (c.parentId && banned.has(c.parentId) && !banned.has(c.id)) {
          banned.add(c.id);
          changed = true;
        }
      }
    }
  }
  const options = ['<option value="">无</option>'];
  for (const c of cats) {
    if (banned.has(c.id) || c.id === 'uncat') continue;
    options.push(`<option value="${escapeHtml(c.id)}">${escapeHtml(`${c.icon} ${c.name}`)}</option>`);
  }
  ($('cat-parent') as HTMLSelectElement).innerHTML = options.join('');
  ($('cat-parent') as HTMLSelectElement).value = current?.parentId ?? '';
  showModal('cat-modal');
  ($('cat-name') as HTMLInputElement).focus();
}

function saveCategory(): void {
  const name = ($('cat-name') as HTMLInputElement).value.trim();
  if (!name) {
    toast('分类名称不能为空');
    return;
  }
  const icon = ($('cat-icon') as HTMLInputElement).value.trim() || '🏷️';
  const colorRaw = ($('cat-color') as HTMLInputElement).value.trim();
  const color = /^#[0-9a-fA-F]{6}$/.test(colorRaw) ? colorRaw : '#8b5cf6';
  const parentId = ($('cat-parent') as HTMLSelectElement).value || null;
  if (editingCatId) updateCategory(editingCatId, { name, icon, color, parentId });
  else createCategory({ name, icon, color, parentId });
  hideModal('cat-modal');
  editingCatId = null;
  toast('分类已保存');
}

function renderAppCategoryList(): void {
  const host = $('app-cat-list');
  const cats = listCategories();
  const filter = appFilter.trim().toLowerCase();
  const rows = allApps
    .filter((r) => r.keydown + r.mousedown > 0)
    .filter((r) => !filter || r.label.toLowerCase().includes(filter));

  if (!rows.length) {
    host.innerHTML = '<div class="chart-empty">还没有可归类的程序，先打开「记录输入」攒一点数据。</div>';
    return;
  }

  const optionsFor = (selected: string) => {
    const opts: string[] = [];
    for (const root of cats.filter((c) => !c.parentId)) {
      opts.push(
        `<option value="${escapeHtml(root.id)}"${root.id === selected ? ' selected' : ''}>${escapeHtml(
          `${root.icon} ${root.name}`,
        )}</option>`,
      );
      for (const child of cats.filter((c) => c.parentId === root.id)) {
        opts.push(
          `<option value="${escapeHtml(child.id)}"${child.id === selected ? ' selected' : ''}>${escapeHtml(
            `　${child.icon} ${child.name}`,
          )}</option>`,
        );
      }
    }
    return opts.join('');
  };

  host.innerHTML = rows
    .map((r) => {
      const cid = categoryForApp(r.label).id;
      return (
        '<div class="app-cat-row">' +
        `<span class="app-cat-name" title="${escapeHtml(r.label)}">${escapeHtml(r.label)}</span>` +
        `<span class="app-cat-meta">${fmtNum(r.keydown)}</span>` +
        `<select data-app="${escapeHtml(r.label)}">${optionsFor(cid)}</select>` +
        '</div>'
      );
    })
    .join('');
}

async function loadApps(): Promise<void> {
  if (!status) return;
  const to = dayStr(new Date());
  const from = status.firstDay ?? to;
  const rows = await guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim: 'app' }));
  allApps = rows ?? [];
  renderAppCategoryList();
}

// ---------------------------------------------------------------- 轨迹

let trail: TrailPlayer | null = null;
let trailLoaded = false;
let trailPointTotal = 0;

function ensureTrailPlayer(): TrailPlayer {
  if (trail) return trail;
  const canvas = $('trail-canvas') as unknown as HTMLCanvasElement;
  trail = new TrailPlayer(canvas, (info) => {
    const progress = $('trail-progress') as HTMLInputElement;
    if (document.activeElement !== progress) progress.value = String(Math.round(info.progress * 1000));
    $('trail-clock').textContent = info.timeMs ? timeStr(info.timeMs) : '--:--:--';
    const playBtn = $('trail-play') as HTMLButtonElement;
    playBtn.textContent = info.playing ? '暂停' : info.progress >= 1 ? '重播' : '开始';
  });
  return trail;
}

function updateTrailAvailability(): void {
  const hasPoints = trailPointTotal > 0;
  ($('trail-play') as HTMLButtonElement).disabled = !hasPoints;
  ($('trail-stop') as HTMLButtonElement).disabled = !hasPoints;
  ($('trail-progress') as HTMLInputElement).disabled = !hasPoints;
  const overlay = $('trail-overlay');
  if (hasPoints) {
    overlay.hidden = true;
    return;
  }
  overlay.hidden = false;
  const enabled = status?.trailEnabled ?? false;
  $('trail-overlay-title').textContent = enabled ? '这个时间范围没有轨迹' : '还没有开启轨迹采样';
  $('trail-overlay-desc').textContent = enabled
    ? '换个范围试试，或者确认那段时间电脑是开着的、鼠标真的动过。'
    : '到「设置 → 记录鼠标轨迹」打开开关，之后的移动才会被采样（历史数据不会补录）。';
}

async function loadTrail(): Promise<void> {
  const player = ensureTrailPlayer();
  const from = fromInputValue(($('trail-from') as HTMLInputElement).value);
  const to = fromInputValue(($('trail-to') as HTMLInputElement).value);
  if (!from || !to || to <= from) {
    toast('时间范围不合法');
    return;
  }
  const points = await guard(() =>
    call<TrailPoint[]>('input_log_trail_points', { from, to, maxPoints: 12000 }),
  );
  const rows = points ?? [];
  trailPointTotal = rows.length;
  player.setPoints(rows);
  trailLoaded = true;
  $('trail-sub').textContent = rows.length
    ? `${fmtNum(rows.length)} 个采样点 · ${timeStr(from)} ~ ${timeStr(to)}`
    : '只记录坐标与时间，不记录任何画面内容';
  updateTrailAvailability();
  if (!rows.length && status?.trailEnabled === false) {
    toast('轨迹采样还没开启，先到设置里打开');
  }
}

function setTrailRange(minutes: number): void {
  const now = Date.now();
  const from = minutes >= 1440 ? startOfToday() : now - minutes * 60_000;
  ($('trail-from') as HTMLInputElement).value = toInputValue(from);
  ($('trail-to') as HTMLInputElement).value = toInputValue(now);
}

function startOfToday(): number {
  const d = new Date();
  d.setHours(0, 0, 0, 0);
  return d.getTime();
}

async function openTrailPage(): Promise<void> {
  ensureTrailPlayer();
  if (!trailLoaded) {
    const today = new Date();
    ($('trail-to') as HTMLInputElement).value = toInputValue(today.getTime());
    ($('trail-from') as HTMLInputElement).value = toInputValue(startOfToday());
    await loadTrail();
  }
}

// ---------------------------------------------------------------- 操作

async function configure(patch: { enabled?: boolean; paused?: boolean; gapMinutes?: number }): Promise<void> {
  if (!status) return;
  const enabled = patch.enabled ?? status.enabled;
  const paused = patch.paused ?? status.paused;
  const gapMinutes = patch.gapMinutes ?? status.activeGapMinutes;
  const st = await guard(() => call<LogStatus>('input_log_configure', { enabled, paused, gapMinutes }));
  if (st) renderStatus(st);
}

function onToggleEnabled(checked: boolean): void {
  if (checked && localStorage.getItem(LS_CONSENT) !== '1') {
    ($('sw-enabled') as HTMLInputElement).checked = false;
    showModal('consent-modal');
    return;
  }
  void configure({ enabled: checked });
}

async function setTrailEnabled(enabled: boolean): Promise<void> {
  const st = await guard(() => call<LogStatus>('input_log_set_trail', { enabled }));
  if (!st) return;
  renderStatus(st);
  toast(enabled ? '轨迹采样已开启' : '轨迹采样已关闭，已有轨迹保留');
  if (enabled) trailLoaded = false;
}

function onToggleTrail(checked: boolean): void {
  if (checked && localStorage.getItem(LS_TRAIL_CONSENT) !== '1') {
    ($('sw-trail') as HTMLInputElement).checked = false;
    showModal('trail-modal');
    return;
  }
  void setTrailEnabled(checked);
}

async function doApplyRetention(days: number): Promise<void> {
  const report = await guard(() => call<RetentionReport>('input_log_apply_retention', { days }));
  if (!report) return;
  const label = days === 0 ? '永久保留' : `${days} 天`;
  toast(
    report.deletedRows > 0
      ? `保留期已设为 ${label}，删除 ${fmtNum(report.deletedRows)} 条记录`
      : `保留期已设为 ${label}`,
  );
  trailLoaded = false;
  await refreshStatus();
  if (document.getElementById('page-stats')?.classList.contains('active')) {
    await loadStats();
  }
}

async function requestRetention(days: number): Promise<void> {
  const cur = status?.retentionDays ?? 0;
  const shrinking = days !== 0 && (cur === 0 || days < cur);
  if (!shrinking) {
    await doApplyRetention(days);
    return;
  }
  const preview = await guard(() => call<RetentionPreview>('input_log_retention_preview', { days }));
  if (!preview) return;
  if (preview.rows <= 0) {
    await doApplyRetention(days);
    return;
  }
  pendingRetentionDays = days;
  const months = preview.months.length ? `（整月表：${preview.months.join('、')}）` : '';
  $('retention-warning').textContent =
    `把保留期设为 ${days} 天，将永久删除 ${preview.cutoffDay} 之前的约 ` +
    `${fmtNum(preview.rows)} 条记录${months}。`;
  showModal('retention-modal');
}

async function requestClear(): Promise<void> {
  const st = await guard(() => call<LogStatus>('input_log_stats'));
  if (!st) return;
  if (st.totalEvents === 0 && st.totalBytes === 0) {
    toast('本来就没有任何记录');
    return;
  }
  $('clear-warning').textContent =
    `将删除 ${fmtBytes(st.totalBytes)}，共约 ${fmtNum(st.totalEvents)} 条事件记录（${st.monthTables} 张月份表）。`;
  showModal('clear-modal');
}

async function doExport(): Promise<void> {
  const result = await guard(() => call<ExportResult>('input_log_export', { days: 30 }));
  if (!result) return;
  toast(`已导出 ${fmtNum(result.rows)} 条到 ${result.path}`);
}

// ---------------------------------------------------------------- 页面切换

function switchPage(page: string): void {
  document.querySelectorAll<HTMLButtonElement>('.nav-item').forEach((btn) => {
    btn.classList.toggle('active', btn.dataset.page === page);
  });
  document.querySelectorAll<HTMLElement>('.page').forEach((el) => {
    el.classList.toggle('active', el.id === `page-${page}`);
  });
  if (page === 'stats') void loadStats();
  if (page === 'trail') void openTrailPage();
  if (page === 'settings') {
    renderCategoryList();
    void loadApps();
  }
}

// ---------------------------------------------------------------- 事件绑定

function bindEvents(): void {
  document.querySelectorAll<HTMLButtonElement>('.nav-item').forEach((btn) => {
    btn.addEventListener('click', () => switchPage(btn.dataset.page ?? 'dashboard'));
  });

  // 主题
  $('theme-row').addEventListener('click', (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLButtonElement>('.chip');
    if (!btn?.dataset.theme) return;
    setThemeMode(btn.dataset.theme as ThemeMode);
  });

  ($('sw-enabled') as HTMLInputElement).addEventListener('change', (ev) => {
    onToggleEnabled((ev.target as HTMLInputElement).checked);
  });

  ($('sw-paused') as HTMLInputElement).addEventListener('change', (ev) => {
    void configure({ paused: (ev.target as HTMLInputElement).checked });
  });

  ($('sw-trail') as HTMLInputElement).addEventListener('change', (ev) => {
    onToggleTrail((ev.target as HTMLInputElement).checked);
  });

  ($('select-gap') as HTMLSelectElement).addEventListener('change', (ev) => {
    void configure({ gapMinutes: Number((ev.target as HTMLSelectElement).value) });
  });

  $('range-row').addEventListener('click', (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLButtonElement>('.chip');
    if (!btn?.dataset.range) return;
    currentRange = Number(btn.dataset.range);
    document.querySelectorAll<HTMLButtonElement>('#range-row .chip').forEach((c) => {
      c.classList.toggle('active', c === btn);
    });
    void loadStats();
  });

  document.querySelectorAll<HTMLButtonElement>('[data-days]').forEach((btn) => {
    btn.addEventListener('click', () => {
      void requestRetention(Number(btn.dataset.days ?? '0'));
    });
  });

  $('btn-retention-apply').addEventListener('click', () => {
    const raw = ($('input-retention') as HTMLInputElement).value.trim();
    const days = raw === '' ? 0 : Math.max(0, Math.min(36500, Math.floor(Number(raw) || 0)));
    void requestRetention(days);
  });

  $('btn-export').addEventListener('click', () => void doExport());
  $('btn-clear').addEventListener('click', () => void requestClear());

  // ---- 分类 ----
  $('btn-cat-new').addEventListener('click', () => openCategoryModal(null));

  $('cat-list').addEventListener('click', (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLButtonElement>('.cat-op');
    if (!btn) return;
    const id = btn.dataset.id ?? '';
    if (btn.dataset.act === 'edit') {
      openCategoryModal(id);
    } else if (btn.dataset.act === 'delete') {
      const cat = listCategories().find((c) => c.id === id);
      if (!cat) return;
      pendingDeleteCatId = id;
      $('cat-delete-warning').textContent = `删除「${cat.icon} ${cat.name}」，属于它的程序会回到「未分类」。`;
      showModal('cat-delete-modal');
    }
  });

  $('cat-cancel').addEventListener('click', () => {
    hideModal('cat-modal');
    editingCatId = null;
  });
  $('cat-save').addEventListener('click', () => saveCategory());

  ($('cat-icon') as HTMLInputElement).addEventListener('input', () => {
    // 输入框本身就是预览，这里只保证空值时不显示成一片空白
  });
  ($('cat-color-picker') as HTMLInputElement).addEventListener('input', (ev) => {
    ($('cat-color') as HTMLInputElement).value = (ev.target as HTMLInputElement).value;
  });
  ($('cat-color') as HTMLInputElement).addEventListener('input', (ev) => {
    const v = (ev.target as HTMLInputElement).value.trim();
    if (/^#[0-9a-fA-F]{6}$/.test(v)) ($('cat-color-picker') as HTMLInputElement).value = v;
  });

  $('cat-delete-cancel').addEventListener('click', () => {
    hideModal('cat-delete-modal');
    pendingDeleteCatId = null;
  });
  $('cat-delete-ok').addEventListener('click', () => {
    const id = pendingDeleteCatId;
    pendingDeleteCatId = null;
    hideModal('cat-delete-modal');
    if (id) {
      deleteCategory(id);
      toast('分类已删除');
    }
  });

  // ---- 应用分类 ----
  $('app-cat-filter').addEventListener('input', (ev) => {
    appFilter = (ev.target as HTMLInputElement).value;
    renderAppCategoryList();
  });

  $('app-cat-list').addEventListener('change', (ev) => {
    const select = (ev.target as HTMLElement).closest('select');
    if (!select) return;
    const app = select.dataset.app;
    if (!app) return;
    setAppCategory(app, (select as HTMLSelectElement).value);
    toast(`已把 ${app} 归到「${categoryForApp(app).name}」`);
  });

  // ---- 轨迹 ----
  $('trail-quick').addEventListener('click', (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLButtonElement>('.chip');
    if (!btn?.dataset.minutes) return;
    document.querySelectorAll<HTMLButtonElement>('#trail-quick .chip').forEach((c) => {
      c.classList.toggle('active', c === btn);
    });
    setTrailRange(Number(btn.dataset.minutes));
    void loadTrail();
  });

  $('trail-load').addEventListener('click', () => {
    document.querySelectorAll<HTMLButtonElement>('#trail-quick .chip').forEach((c) => c.classList.remove('active'));
    void loadTrail();
  });

  $('trail-play').addEventListener('click', () => {
    const player = ensureTrailPlayer();
    if (trailPointTotal === 0) return;
    if (player.isPlaying) player.pause();
    else player.play();
  });

  $('trail-stop').addEventListener('click', () => {
    trail?.stop();
  });

  $('trail-progress').addEventListener('input', (ev) => {
    const value = Number((ev.target as HTMLInputElement).value) / 1000;
    trail?.seek(value);
  });

  ($('trail-speed') as HTMLSelectElement).addEventListener('change', (ev) => {
    const speed = Number((ev.target as HTMLSelectElement).value) || 8;
    if (trail) trail.setSpeed(speed);
  });

  // 首次开启告知
  $('consent-cancel').addEventListener('click', () => hideModal('consent-modal'));
  $('consent-ok').addEventListener('click', () => {
    localStorage.setItem(LS_CONSENT, '1');
    hideModal('consent-modal');
    void configure({ enabled: true });
  });

  // 轨迹采样告知
  $('trail-cancel').addEventListener('click', () => hideModal('trail-modal'));
  $('trail-ok').addEventListener('click', () => {
    localStorage.setItem(LS_TRAIL_CONSENT, '1');
    hideModal('trail-modal');
    void setTrailEnabled(true);
  });

  // 保留期确认
  $('retention-cancel').addEventListener('click', () => {
    pendingRetentionDays = null;
    hideModal('retention-modal');
  });
  $('retention-ok').addEventListener('click', () => {
    hideModal('retention-modal');
    const days = pendingRetentionDays;
    pendingRetentionDays = null;
    if (days !== null) void doApplyRetention(days);
  });

  // 清空确认
  $('clear-cancel').addEventListener('click', () => hideModal('clear-modal'));
  $('clear-ok').addEventListener('click', async () => {
    hideModal('clear-modal');
    const ok = await guard(() => call<void>('input_log_clear').then(() => true));
    if (ok) {
      toast('已清空全部输入记录');
      trailPointTotal = 0;
      trailLoaded = false;
      trail?.setPoints([]);
      updateTrailAvailability();
      await refreshStatus();
      if (document.getElementById('page-stats')?.classList.contains('active')) {
        await loadStats();
      }
    }
  });

  document.querySelectorAll<HTMLElement>('.modal-mask').forEach((mask) => {
    mask.addEventListener('click', (ev) => {
      if (ev.target === mask) mask.classList.remove('show');
    });
  });

  window.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape') {
      document.querySelectorAll<HTMLElement>('.modal-mask.show').forEach((m) => m.classList.remove('show'));
      pendingRetentionDays = null;
      editingCatId = null;
      pendingDeleteCatId = null;
    }
  });

  // 图表按容器真实像素宽画，窗口一变就得重排；只重画不重新查库
  let resizeTimer = 0;
  window.addEventListener('resize', () => {
    window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(() => redrawAllCharts(), 120);
  });
}

// ---------------------------------------------------------------- 启动

async function boot(): Promise<void> {
  applyTheme();
  bindSystemTheme();
  bindEvents();
  // 分类存在 settings.json 里，先把权威版本拉回来再渲染任何列表
  await initCategories();
  // 分类变了（新增/删除/归类），统计图与两个列表都要跟着变
  subscribe(() => {
    renderCategoryList();
    renderAppCategoryList();
    if (document.getElementById('page-stats')?.classList.contains('active')) void loadStats();
  });

  await refreshStatus();

  // 调试用：stats.html?page=stats 直接打开某一页
  const wanted = new URLSearchParams(location.search).get('page');
  if (wanted === 'stats' || wanted === 'settings' || wanted === 'trail') switchPage(wanted);

  // 菜单里改了开关，统计窗口跟着变
  if (HAS_TAURI) {
    void listen('kuro-settings', () => {
      void refreshStatus();
    });
    void listen('kuro-input-log-cleaned', () => {
      void refreshStatus();
    });
  }

  // 仪表盘 30 秒自动刷新（只在可见时）
  window.setInterval(() => {
    if (document.visibilityState === 'visible' && document.getElementById('page-dashboard')?.classList.contains('active')) {
      void refreshStatus();
    }
  }, 30_000);
}

void boot();
