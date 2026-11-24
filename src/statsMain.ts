import './stats.css';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import {
  renderBars,
  renderHBars,
  renderLine,
  fmtNum,
  fmtDuration,
  fmtBytes,
  type Point,
} from './statsChart';

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

type SummaryDim = 'day' | 'month' | 'year' | 'hour' | 'app' | 'key';

const LS_CONSENT = 'kurocat.inputLog.consent.v1';

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

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  if (HAS_TAURI) return invoke<T>(cmd, args);
  switch (cmd) {
    case 'input_log_stats':
      return demoStatus() as unknown as T;
    case 'input_log_configure':
      demoEnabled = Boolean(args?.enabled);
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

  renderBars(
    $('chart-hour'),
    hours.map((r) => ({ label: r.label, value: r.keydown })),
    { height: 168, maxLabels: 12 },
  );
  $('hour-sum').textContent = hours.length ? `${fmtNum(sumOf(hours))} 次` : '—';

  const appPoints: Point[] = apps
    .filter((r) => r.keydown > 0)
    .slice(0, 10)
    .map((r) => ({ label: r.label, value: r.keydown }));
  renderHBars($('chart-app'), appPoints, { labelWidth: 132 });
  $('app-sum').textContent = appPoints.length ? `前 ${appPoints.length}` : '—';

  const keyPoints: Point[] = keys
    .filter((r) => r.keydown > 0)
    .slice(0, 15)
    .map((r) => ({ label: r.label, value: r.keydown }));
  renderHBars($('chart-key'), keyPoints, { labelWidth: 96 });
  $('key-sum').textContent = keyPoints.length ? `前 ${keyPoints.length}` : '—';
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

async function doApplyRetention(days: number): Promise<void> {
  const report = await guard(() => call<RetentionReport>('input_log_apply_retention', { days }));
  if (!report) return;
  const label = days === 0 ? '永久保留' : `${days} 天`;
  toast(
    report.deletedRows > 0
      ? `保留期已设为 ${label}，删除 ${fmtNum(report.deletedRows)} 条记录`
      : `保留期已设为 ${label}`,
  );
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
}

// ---------------------------------------------------------------- 事件绑定

function bindEvents(): void {
  document.querySelectorAll<HTMLButtonElement>('.nav-item').forEach((btn) => {
    btn.addEventListener('click', () => switchPage(btn.dataset.page ?? 'dashboard'));
  });

  ($('sw-enabled') as HTMLInputElement).addEventListener('change', (ev) => {
    onToggleEnabled((ev.target as HTMLInputElement).checked);
  });

  ($('sw-paused') as HTMLInputElement).addEventListener('change', (ev) => {
    void configure({ paused: (ev.target as HTMLInputElement).checked });
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

  // 首次开启告知
  $('consent-cancel').addEventListener('click', () => hideModal('consent-modal'));
  $('consent-ok').addEventListener('click', () => {
    localStorage.setItem(LS_CONSENT, '1');
    hideModal('consent-modal');
    void configure({ enabled: true });
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
    }
  });
}

// ---------------------------------------------------------------- 启动

async function boot(): Promise<void> {
  bindEvents();
  await refreshStatus();

  // 调试用：stats.html?page=stats 直接打开某一页
  const wanted = new URLSearchParams(location.search).get('page');
  if (wanted === 'stats' || wanted === 'settings') switchPage(wanted);

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
