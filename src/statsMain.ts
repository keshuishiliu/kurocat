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
  findCategory,
  init as initCategories,
  listCategories,
  rollupByCategory,
  rootOf,
  setAppCategory,
  subscribe,
  updateCategory,
  type Category,
} from './categories';
import {
  displayNameFor,
  init as initAppOverrides,
  isIgnored,
  setDisplayName,
  setIgnored,
  subscribe as subscribeOverrides,
} from './appOverrides';
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
  /** 今天各程序占据前台的秒数合计，与 activeSecondsToday 是两个不同口径。 */
  focusSecondsToday: number;
  firstDay: string | null;
  lastDay: string | null;
  monthTables: number;
  appCount: number;
  retentionDays: number;
  activeGapMinutes: number;
  trailEnabled: boolean;
}

/** 录入访问密码状态（input_log_password_status 的返回）。 */
interface PasswordStatus {
  /** 是否已设置密码。未设置时录制被门禁压制。 */
  set: boolean;
  /** 本次运行是否已解锁（重启后回到 false）。 */
  unlocked: boolean;
}

interface SummaryPoint {
  label: string;
  keydown: number;
  mousedown: number;
  activeSeconds: number;
  focusSeconds: number;
}

/** 分类聚合的一行（对应 recorder.rs 的 CatRow）。cat 为空串 = 未归类。 */
interface CatRow {
  label: string;
  cat: string;
  n: number;
}

/** 分类聚合结果（对应 recorder.rs 的 CategorySummary）。hours/keys 范围 > 31 天时为空。 */
interface CategorySummary {
  days: CatRow[];
  hours: CatRow[];
  keys: CatRow[];
}

/**
 * 应用元信息（对应 src-tauri/src/appmeta.rs 的 AppMeta）。
 * `process` 与 settings.json 里 app_categories 的 key 一致，中文名只用于展示。
 */
interface AppMeta {
  process: string;
  displayName: string | null;
  version: string | null;
  exePath: string | null;
  icon: string | null;
}

interface AppActivity {
  process: string;
  lastDay: string;
}

/** 应用详情统计（对应 recorder.rs 的 AppStats，camelCase）。 */
interface AppStats {
  process: string;
  found: boolean;
  keydown: number;
  mousedown: number;
  activeSeconds: number;
  focusSeconds: number;
  firstDay: string | null;
  lastDay: string | null;
  daily: Array<{ day: string; keydown: number; mousedown: number; activeSeconds: number; focusSeconds: number }>;
  heatmap: Array<{ day: string; hour: number; seconds: number }>;
  topKeys: Array<{ label: string; n: number }>;
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

/** 同步配置（对应 config.rs 的 SyncSettings，camelCase）。 */
interface SyncConfig {
  enabled: boolean;
  server: string;
  account: string;
  password: string;
  machineLabel: string;
  intervalMinutes: number;
  retainDays: number;
}

/** 同步状态（对应 sync.rs 的 SyncStatus）。 */
interface SyncStatus {
  enabled: boolean;
  configured: boolean;
  intervalMinutes: number;
  machineId: string;
  lastSyncAt: number | null;
  lastSyncOk: boolean | null;
  lastSyncError: string | null;
}

/** 一次手动同步的结果（对应 sync.rs 的 SyncOutcome）。 */
interface SyncOutcome {
  uploaded: number;
  imported: number;
  skipped: number;
  categoriesMerged: boolean;
  warnings: string[];
}

/** 备份配置（对应 config.rs 的 BackupSettings）。 */
interface BackupConfig {
  enabled: boolean;
  intervalMinutes: number;
  retentionDays: number;
}

/** 备份状态（对应 backup.rs 的 BackupStatus）。 */
interface BackupStatusInfo {
  enabled: boolean;
  intervalMinutes: number;
  retentionDays: number;
  lastBackupTs: number;
  lastOk: boolean;
  lastMessage: string;
}

/** 云端快照（对应 backup.rs 的 SnapshotMeta）。 */
interface SnapshotMeta {
  ts: number;
  name: string;
  files: string[];
}

/** 一次恢复的结果（对应 backup.rs 的 RestoreReport）。 */
interface RestoreReport {
  restored: string[];
  note: string;
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
    focusSecondsToday: 21_360,
    firstDay: '2026-08-03',
    lastDay: '2026-11-24',
    monthTables: 4,
    appCount: 37,
    retentionDays: 0,
    activeGapMinutes: 5,
    trailEnabled: true,
  };
}

/** 演示模式用的应用元信息。浏览器里打开 stats.html 时也能看到图标位、中文名和路径。 */
const DEMO_APP_META: Record<string, { name: string; version: string; path: string }> = {
  'code.exe': {
    name: 'Visual Studio Code',
    version: '1.95.3',
    path: 'C:\\Users\\you\\AppData\\Local\\Programs\\Microsoft VS Code\\Code.exe',
  },
  'chrome.exe': {
    name: 'Google Chrome',
    version: '131.0.6778',
    path: 'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
  },
  'wechat.exe': {
    name: '微信',
    version: '3.9.12',
    path: 'C:\\Program Files\\Tencent\\WeChat\\WeChat.exe',
  },
  'kurocat.exe': {
    name: 'Kuro 猫猫宠',
    version: '5.6.0',
    path: 'D:\\Programs\\kurocat\\KuroCat.exe',
  },
  'windowsterminal.exe': {
    name: 'Windows 终端',
    version: '1.21.2911',
    path: 'C:\\Program Files\\WindowsApps\\Microsoft.WindowsTerminal\\WindowsTerminal.exe',
  },
  'explorer.exe': {
    name: 'Windows 资源管理器',
    version: '10.0.22621',
    path: 'C:\\Windows\\explorer.exe',
  },
  'winword.exe': {
    name: 'Microsoft Word',
    version: '16.0.17928',
    path: 'C:\\Program Files\\Microsoft Office\\root\\Office16\\WINWORD.EXE',
  },
  'notepad.exe': {
    name: '记事本',
    version: '11.2405',
    path: 'C:\\Windows\\System32\\notepad.exe',
  },
  'spotify.exe': {
    name: 'Spotify',
    version: '1.2.47',
    path: 'C:\\Users\\you\\AppData\\Roaming\\Spotify\\Spotify.exe',
  },
  'photoshop.exe': {
    name: 'Adobe Photoshop',
    version: '25.11',
    path: 'C:\\Program Files\\Adobe\\Adobe Photoshop 2024\\Photoshop.exe',
  },
};

function demoAppMeta(process: string): AppMeta {
  const hit = DEMO_APP_META[process.toLowerCase()];
  return {
    process,
    displayName: hit?.name ?? null,
    version: hit?.version ?? null,
    exePath: hit?.path ?? null,
    icon: null,
  };
}

function demoDayAgo(n: number): string {
  const d = new Date();
  d.setDate(d.getDate() - n);
  return dayStr(d);
}

/** 演示模式的应用统计：按进程名做种子生成形状稳定的假数据，方便调 UI。 */
function demoAppStats(process: string, from: string, to: string): AppStats {
  const days: string[] = [];
  const start = new Date(`${from}T00:00:00`);
  const end = new Date(`${to}T00:00:00`);
  if (!Number.isNaN(start.getTime()) && !Number.isNaN(end.getTime())) {
    for (let d = new Date(start); d <= end && days.length < 45; d.setDate(d.getDate() + 1)) {
      days.push(dayStr(d));
    }
  }
  const seed = Array.from(process).reduce((a, c) => a + c.charCodeAt(0), 0);
  const wave = (i: number) =>
    Math.max(0, Math.round(1800 + Math.sin((seed + i) * 0.9) * 1200 + (i % 5) * 260));
  const daily = days.map((day, i) => ({
    day,
    keydown: wave(i),
    mousedown: Math.round(wave(i) / 9),
    activeSeconds: Math.round(wave(i) / 3),
    focusSeconds: Math.round(wave(i) / 2.2),
  }));
  const heatmap = days.flatMap((day, i) => {
    const cells: Array<{ day: string; hour: number; seconds: number }> = [];
    for (let h = 0; h < 24; h++) {
      const secs = Math.round(
        900 * Math.exp(-((h - 10 - (seed % 6)) ** 2) / 40) * (0.6 + Math.sin(i * 1.3 + h) * 0.4),
      );
      if (secs > 0) cells.push({ day, hour: h, seconds: secs });
    }
    return cells;
  });
  const keydown = daily.reduce((a, r) => a + r.keydown, 0);
  return {
    process,
    found: daily.length > 0,
    keydown,
    mousedown: daily.reduce((a, r) => a + r.mousedown, 0),
    activeSeconds: daily.reduce((a, r) => a + r.activeSeconds, 0),
    focusSeconds: daily.reduce((a, r) => a + r.focusSeconds, 0),
    firstDay: days[0] ?? null,
    lastDay: days[days.length - 1] ?? null,
    daily,
    heatmap,
    topKeys: (
      [
        ['Space', 0.16],
        ['Enter', 0.11],
        ['Backspace', 0.09],
        ['E', 0.08],
        ['A', 0.07],
        ['S', 0.06],
        ['T', 0.05],
        ['LCtrl', 0.05],
      ] as Array<[string, number]>
    ).map(([label, ratio]) => ({ label, n: Math.round((keydown || 1000) * ratio) })),
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
      focusSeconds: Math.round(v * 1.6),
    }));
  }
  if (dim === 'app') {
    // 演示数据里刻意不放 KuroCat.exe：真实环境里它被排除在前台时长之外。
    const apps: Array<[string, number, number]> = [
      ['Code.exe', 48210, 14400],
      ['chrome.exe', 31844, 10800],
      ['WeChat.exe', 21090, 5400],
      ['WindowsTerminal.exe', 15220, 6300],
      ['explorer.exe', 9840, 2400],
      ['WINWORD.EXE', 7420, 4800],
      ['Notepad.exe', 5210, 1800],
      ['Spotify.exe', 3180, 9600],
      ['Photoshop.exe', 2140, 3000],
    ];
    return apps.map(([label, keydown, focusSeconds]) => ({
      label,
      keydown,
      mousedown: 0,
      activeSeconds: Math.round(keydown / 8),
      focusSeconds,
    }));
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
    return keys.map(([label, keydown]) => ({
      label,
      keydown,
      mousedown: 0,
      activeSeconds: 0,
      focusSeconds: 0,
    }));
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
      focusSeconds: Math.round(wave(i) / 2.4),
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
    case 'input_log_password_status':
      return { set: true, unlocked: true } as unknown as T;
    case 'input_log_password_set':
    case 'input_log_password_verify':
    case 'input_log_password_change':
    case 'input_log_password_reset':
      return { set: true, unlocked: true } as unknown as T;
    case 'input_log_configure':
    case 'input_log_set_trail':
      demoEnabled = Boolean(args?.enabled ?? demoEnabled);
      return demoStatus() as unknown as T;
    case 'input_log_summary':
      return demoSummary(String(args?.dim ?? 'day')) as unknown as T;
    case 'input_log_category_summary':
      return { days: [], hours: [], keys: [] } as unknown as T;
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
    case 'app_meta_get': {
      const procs = (args?.processes as string[] | undefined) ?? [];
      return procs.map(demoAppMeta) as unknown as T;
    }
    case 'input_log_app_activity':
      return demoSummary('app').map((r, i) => ({ process: r.label, lastDay: demoDayAgo(i * 3) })) as unknown as T;
    case 'input_log_app_stats':
      return demoAppStats(String(args?.process ?? ''), String(args?.from ?? ''), String(args?.to ?? '')) as unknown as T;
    case 'open_path_folder':
      return undefined as unknown as T;
    case 'get_settings':
      return {
        sync: {
          enabled: demoEnabled,
          server: 'https://dav.jianguoyun.com/dav/',
          account: '',
          password: '',
          machineLabel: '演示机',
          intervalMinutes: 30,
        },
        backup: {
          enabled: false,
          intervalMinutes: 30,
          retentionDays: 15,
        },
      } as unknown as T;
    case 'sync_configure':
      return (args?.sync ?? {}) as unknown as T;
    case 'sync_get_status':
      return {
        enabled: demoEnabled,
        configured: false,
        intervalMinutes: 30,
        machineId: 'demo0123abcd5678',
        lastSyncAt: null,
        lastSyncOk: null,
        lastSyncError: null,
      } as unknown as T;
    case 'sync_test_connection':
      return '连接成功（演示模式）' as unknown as T;
    case 'sync_now':
      return {
        uploaded: 1,
        imported: 2,
        skipped: 5,
        categoriesMerged: false,
        warnings: [],
      } as unknown as T;
    case 'backup_status':
      return {
        enabled: false,
        intervalMinutes: 30,
        retentionDays: 15,
        lastBackupTs: 0,
        lastOk: false,
        lastMessage: '尚未备份',
      } as unknown as T;
    case 'backup_configure':
      return (args?.backup ?? {}) as unknown as T;
    case 'backup_run_now':
      return {
        ts: Date.now(),
        name: String(Date.now()),
        files: ['events.db', 'meta.db'],
      } as unknown as T;
    case 'backup_list':
      return [
        { ts: Date.now() - 3_600_000, name: String(Date.now() - 3_600_000), files: ['events.db', 'meta.db', 'trail.db'] },
        { ts: Date.now() - 86_400_000, name: String(Date.now() - 86_400_000), files: ['events.db', 'meta.db'] },
      ] as unknown as T;
    case 'backup_restore':
      return {
        restored: ['events.db', 'meta.db'],
        note: '恢复前的数据已保留在 input-log/pre-restore 目录，确认无误后可自行删除。',
      } as unknown as T;
    default:
      throw new Error(`演示模式不支持 ${cmd}`);
  }
}

function $<T extends HTMLElement>(id: string): T {
  return document.getElementById(id) as unknown as T;
}

let status: LogStatus | null = null;
let currentRange = 1;
/** 自定义起止日期（date input 的值）。任一为空表示未启用自定义区间。 */
let customFrom = '';
let customTo = '';
let pendingRetentionDays: number | null = null;
let toastTimer = 0;
let allApps: SummaryPoint[] = [];
let appFilter = '';
/** 分类筛选 chip：'all' = 全部，其余为顶层分类 id（含内置「未分类」）。 */
let appCatChip = 'all';
/** 详情弹窗统计的时间范围（天数；0 = 全部）。 */
let appStatsRange = 1;
/** 快速换类下拉当前挂着的程序进程名；null = 收起。 */
let quickDropApp: string | null = null;
/** 进程名（小写）→ 图标 / 中文名 / 路径 / 版本 */
let appMetas = new Map<string, AppMeta>();
/** 进程名（小写）→ 最后一次有记录的日期 */
let appLastDays = new Map<string, string>();
let appMetaLoaded = false;
/** 当前正在弹窗编辑的程序进程名 */
let editingApp: string | null = null;

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
    const msg = String(err);
    // 后端门禁：录入相关命令在未设密码/未解锁时统一返回 LOCKED: 前缀错误
    if (msg.startsWith('LOCKED:')) {
      void handleLocked(msg.slice('LOCKED:'.length));
      return null;
    }
    toast(msg);
    return null;
  }
}

// ---------------------------------------------------------------- 访问密码门禁

let pwStatus: PasswordStatus | null = null;
type PwMode = 'set' | 'unlock' | 'change' | 'reset';
let pwMode: PwMode = 'unlock';
/** 解锁/设密码成功后要继续的动作（比如「打开刚才想改的开关」）。 */
let pwPending: (() => Promise<void>) | null = null;
/** 阻塞态弹窗（解锁/强制设密码）不允许点遮罩或 Esc 关掉。 */
let pwBlocking = false;

const PW_FIELD_IDS = ['pw-row-old', 'pw-row-new', 'pw-row-confirm'] as const;

async function refreshPwStatus(): Promise<PasswordStatus> {
  pwStatus = await call<PasswordStatus>('input_log_password_status');
  renderPwRow();
  return pwStatus;
}

function renderPwRow(): void {
  if (!pwStatus) return;
  const btn = $('pw-manage');
  const desc = $('pw-desc');
  if (!pwStatus.set) {
    btn.textContent = '设置密码';
    desc.textContent = '尚未设置。开启记录前必须先设密码，用于保护本页的录入数据与设置。';
  } else if (!pwStatus.unlocked) {
    btn.textContent = '修改密码';
    desc.textContent = '已设置，本次运行尚未解锁：查询、导出与设置都被拦截。';
  } else {
    btn.textContent = '修改密码';
    desc.textContent = '已解锁，本次运行内不再询问；重启程序后需重新解锁。';
  }
}

function setPwFieldVisible(id: string, visible: boolean): void {
  const el = $(id);
  el.hidden = !visible;
}

function clearPwInputs(): void {
  for (const id of ['input-pw-old', 'input-pw-new', 'input-pw-confirm'] as const) {
    ($(id) as HTMLInputElement).value = '';
  }
  const err = $('pw-error');
  err.hidden = true;
  err.textContent = '';
}

function openPwModal(
  mode: PwMode,
  opts?: { blocking?: boolean; after?: () => Promise<void> },
): void {
  pwMode = mode;
  pwBlocking = Boolean(opts?.blocking);
  pwPending = opts?.after ?? null;
  clearPwInputs();

  const title = $('pw-title');
  const hint = $('pw-hint');
  const ok = $('pw-ok');
  const cancel = $('pw-cancel');
  const forgot = $('pw-forgot');
  setPwFieldVisible('pw-row-old', false);
  setPwFieldVisible('pw-row-new', false);
  setPwFieldVisible('pw-row-confirm', false);
  forgot.hidden = true;
  cancel.hidden = pwBlocking;

  switch (mode) {
    case 'set':
      title.textContent = '设置访问密码';
      hint.textContent =
        '开启「记录输入」前必须设置访问密码。进设置、查数据、导出、清空前都要先输它，本次程序运行内输入一次即可。';
      setPwFieldVisible('pw-row-new', true);
      setPwFieldVisible('pw-row-confirm', true);
      ok.textContent = '设置密码';
      break;
    case 'unlock':
      title.textContent = '输入访问密码';
      hint.textContent = '录入数据已加密保存。解锁后本次运行内可以查看与修改设置。';
      setPwFieldVisible('pw-row-new', true);
      ok.textContent = '解锁';
      break;
    case 'change':
      title.textContent = '修改访问密码';
      hint.textContent = '';
      setPwFieldVisible('pw-row-old', true);
      setPwFieldVisible('pw-row-new', true);
      setPwFieldVisible('pw-row-confirm', true);
      forgot.hidden = false;
      ok.textContent = '确认修改';
      break;
    case 'reset':
      title.textContent = '重置访问密码';
      hint.textContent =
        '忘记原密码时使用。重置不会影响已记录的数据（数据密钥与密码无关），但会立即以新密码生效。';
      setPwFieldVisible('pw-row-new', true);
      setPwFieldVisible('pw-row-confirm', true);
      ok.textContent = '重置密码';
      break;
  }
  showModal('pw-modal');
  const first = (
    PW_FIELD_IDS.map((id) => $(id))
      .find((el) => !el.hidden) as HTMLElement
  ).querySelector('input');
  first?.focus();
}

async function handleLocked(reason: string): Promise<void> {
  const mask = $('pw-modal');
  if (mask.classList.contains('show')) return; // 已在弹窗流程中
  try {
    await refreshPwStatus();
  } catch {
    toast(reason);
    return;
  }
  if (!pwStatus) {
    toast(reason);
    return;
  }
  if (!pwStatus.set) {
    toast('开启记录前需要先设置访问密码');
    openPwModal('set', { blocking: true });
  } else {
    openPwModal('unlock', { blocking: true });
  }
}

async function submitPw(): Promise<void> {
  const err = $('pw-error');
  err.hidden = true;
  err.textContent = '';
  const oldV = ($('input-pw-old') as HTMLInputElement).value;
  const newV = ($('input-pw-new') as HTMLInputElement).value;
  const confV = ($('input-pw-confirm') as HTMLInputElement).value;
  try {
    let st: PasswordStatus | null = null;
    if (pwMode === 'set' || pwMode === 'reset') {
      if (newV !== confV) throw new Error('两次输入的密码不一致');
      st = await call<PasswordStatus>(
        pwMode === 'set' ? 'input_log_password_set' : 'input_log_password_reset',
        { password: newV },
      );
      toast(pwMode === 'set' ? '访问密码已设置' : '访问密码已重置');
    } else if (pwMode === 'unlock') {
      st = await call<PasswordStatus>('input_log_password_verify', { password: newV });
      if (!st.unlocked) throw new Error('密码不正确');
      toast('已解锁，本次运行内不再询问');
    } else {
      if (newV !== confV) throw new Error('两次输入的密码不一致');
      st = await call<PasswordStatus>('input_log_password_change', {
        oldPassword: oldV,
        newPassword: newV,
      });
      toast('访问密码已修改');
    }
    pwStatus = st;
    renderPwRow();
    hideModal('pw-modal');
    const after = pwPending;
    pwPending = null;
    pwBlocking = false;
    if (after) await after();
  } catch (e) {
    err.textContent = String(e);
    err.hidden = false;
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
  $('dash-focus').innerHTML = `${Math.round(st.focusSecondsToday / 60)}<small>分钟</small>`;
  $('dash-focus-hint').textContent =
    st.focusSecondsToday > 0
      ? `窗口在前台 ${fmtDuration(st.focusSecondsToday)}`
      : '窗口占据前台的时长';

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
    btn.classList.toggle(
      'active',
      !customFrom && !customTo && Number(btn.dataset.range) === currentRange,
    );
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
  // 自定义区间优先；起止颠倒自动交换
  const hasCustom = Boolean(customFrom && customTo);
  const to = hasCustom ? (customFrom <= customTo ? customTo : customFrom) : dayStr(new Date());
  let from: string;
  if (hasCustom) {
    from = customFrom <= customTo ? customFrom : customTo;
  } else if (currentRange === 0) {
    from = status?.firstDay ?? to;
  } else {
    from = daysAgoInclusive(currentRange);
  }
  const dim: SummaryDim = spanDays(from, to) > 92 ? 'month' : 'day';

  const [trendRaw, hoursRaw, appsRaw, keysRaw, catsRaw] = await Promise.all([
    guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim })),
    guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim: 'hour' })),
    guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim: 'app' })),
    guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim: 'key' })),
    guard(() => call<CategorySummary>('input_log_category_summary', { from, to })),
  ]);
  const trend = trendRaw ?? [];
  const hours = hoursRaw ?? [];
  const apps = appsRaw ?? [];
  const keys = keysRaw ?? [];
  const cats = catsRaw ?? { days: [], hours: [], keys: [] };
  // 忽略的程序不进统计口径：新数据本来就不会再产生，历史数据也一并隐藏
  // （明细保留在库里，取消忽略即可恢复显示）。
  const appsVisible = apps.filter((r) => !isIgnored(r.label));

  // ---- 趋势：按分类拼接（daily_apps 聚合，任何范围都便宜） ----
  const trendOrder = catOrderOf(cats.days);
  const dayMap = catMapOf(cats.days, dim);
  const trendPoints: Point[] = trend.map((r) => {
    const inner = dayMap.get(r.label);
    const segs = inner
      ? trendOrder
          .map((c) => ({ v: inner.get(c.id) ?? 0, c: c.color, name: c.name }))
          .filter((s) => s.v > 0)
      : undefined;
    return { label: shortLabel(r.label, dim), value: r.keydown, segs };
  });
  renderLegend('legend-trend', trendOrder);
  const trendHost = $('chart-trend');
  if (trendPoints.length > 20) {
    renderLine(trendHost, trendPoints, { height: 200, maxLabels: 9 });
  } else {
    renderBars(trendHost, trendPoints, { height: 200, maxLabels: 12 });
  }
  $('trend-sum').textContent = trendPoints.length ? `${fmtNum(sumOf(trend))} 次` : '—';

  // ---- 分布：分类拼接只在后端扫得动 events 明细时有（范围 ≤ 31 天），否则退回纯色 ----
  const hourOrder = catOrderOf(cats.hours);
  const hourMap = catMapOf(cats.hours, 'day');
  const hourPoints: Point[] = hours.map((r) => {
    const inner = hourMap.get(r.label);
    const segs = inner
      ? hourOrder
          .map((c) => ({ v: inner.get(c.id) ?? 0, c: c.color, name: c.name }))
          .filter((s) => s.v > 0)
      : undefined;
    return { label: r.label, value: r.keydown, segs };
  });
  renderLegend('legend-hour', hourOrder);
  // 整行宽之后 24 个小时桶每格有二十多像素，标签可以按小时一格给，不用再抽稀
  renderBars($('chart-hour'), hourPoints, { height: 168, maxLabels: 24 });
  $('hour-sum').textContent = hours.length ? `${fmtNum(sumOf(hours))} 次` : '—';

  // ---- 按程序排行：全量展示，容器滚动 ----
  // 后端返回的是「按键 + 时长」混合序，击键排行必须自己重排成严格按键数降序，
  // 否则前台很久但没怎么按键的程序会插到纯按键大户前面。
  const appRows = appsVisible
    .filter((r) => r.keydown > 0)
    .sort((a, b) => b.keydown - a.keydown);
  // 前台时长排行：与击键排行是两回事——看视频、读文档的程序一个键都不按，
  // 但会在这里排在前面。排序必须自己重排，不能沿用后端按「按键 + 时长」的混合序。
  const focusRows = appsVisible
    .filter((r) => r.focusSeconds > 0)
    .sort((a, b) => b.focusSeconds - a.focusSeconds);
  const renderAppCharts = () => {
    renderHBars(
      $('chart-app'),
      appRows.map((r) => ({ label: appTitle(r.label), value: r.keydown })),
      { labelWidth: 200, icons: appRows.map((r) => metaFor(r.label)?.icon ?? null) },
    );
    renderHBars(
      $('chart-focus'),
      focusRows.map((r) => ({ label: appTitle(r.label), value: Math.round(r.focusSeconds / 60) })),
      { labelWidth: 200, icons: focusRows.map((r) => metaFor(r.label)?.icon ?? null) },
    );
  };
  renderAppCharts();
  $('app-sum').textContent = appRows.length ? `${appRows.length} 个程序` : '—';
  if (await ensureAppMetas(appsVisible.map((r) => r.label))) {
    renderAppCharts();
  }
  const focusTotal = appsVisible.reduce((acc, r) => acc + r.focusSeconds, 0);
  $('focus-sum').textContent = focusTotal > 0 ? fmtDuration(focusTotal) : '—';

  // 分类汇总：子分类并入顶层，颜色直接取分类色
  const rolled = rollupByCategory(appsVisible.map((r) => ({ app: r.label, value: r.keydown })));
  const catPoints: Point[] = rolled.map((r) => ({
    label: `${r.category.icon} ${r.category.name}`,
    value: r.value,
  }));
  renderHBars($('chart-cat'), catPoints, {
    labelWidth: 118,
    colors: rolled.map((r) => r.category.color),
  });
  $('cat-sum').textContent = catPoints.length ? `${catPoints.length} 类` : '—';

  const unmapped = appsVisible.filter((r) => r.keydown > 0 && categoryForApp(r.label).id === 'uncat').length;
  const noteEl = $('stats-cat-note');
  if (unmapped > 0) {
    noteEl.hidden = false;
    noteEl.innerHTML =
      `<strong>还有 ${unmapped} 个程序没有归类。</strong>` +
      '它们被算进了「未分类」。到「设置 → 应用分类」认一下，分类统计会更准。';
  } else {
    noteEl.hidden = true;
  }

  // ---- 最常按的键：按分类拼接（events 聚合，≤ 31 天时有），全量滚动 ----
  let keyOrder: Array<{ id: string; name: string; color: string }> = [];
  let keyPoints: Point[];
  if (cats.keys.length) {
    keyOrder = catOrderOf(cats.keys);
    const km = catMapOf(cats.keys, 'day');
    keyPoints = [...km.entries()]
      .map(([label, inner]) => ({
        label,
        value: [...inner.values()].reduce((a, b) => a + b, 0),
        segs: keyOrder
          .map((c) => ({ v: inner.get(c.id) ?? 0, c: c.color, name: c.name }))
          .filter((s) => s.v > 0),
      }))
      .sort((a, b) => b.value - a.value);
  } else {
    keyPoints = keys.filter((r) => r.keydown > 0).map((r) => ({ label: r.label, value: r.keydown }));
  }
  renderLegend('legend-key', keyOrder);
  renderHBars($('chart-key'), keyPoints, { labelWidth: 96 });
  $('key-sum').textContent = keyPoints.length ? `${keyPoints.length} 种按键` : '—';
}

/** 分类分段顺序：按总次数降序，颜色跨柱一致。 */
function catOrderOf(rows: CatRow[]): Array<{ id: string; name: string; color: string }> {
  const totals = new Map<string, number>();
  for (const r of rows) totals.set(r.cat, (totals.get(r.cat) ?? 0) + r.n);
  return [...totals.entries()]
    .sort((a, b) => b[1] - a[1])
    .map(([id]) => {
      const c = findCategory(id || 'uncat');
      return { id, name: c?.name ?? '未分类', color: c?.color ?? '#8b909c' };
    });
}

/** label → (分类 id → n)。dim='month' 时天标签折叠成月。 */
function catMapOf(rows: CatRow[], dim: SummaryDim): Map<string, Map<string, number>> {
  const out = new Map<string, Map<string, number>>();
  for (const r of rows) {
    const label = dim === 'month' ? r.label.slice(0, 7) : r.label;
    let inner = out.get(label);
    if (!inner) {
      inner = new Map();
      out.set(label, inner);
    }
    inner.set(r.cat, (inner.get(r.cat) ?? 0) + r.n);
  }
  return out;
}

/** 图例：分类色点 + 名称。空数组时隐藏容器。 */
function renderLegend(id: string, order: Array<{ name: string; color: string }>): void {
  $(id).innerHTML = order
    .map(
      (c) =>
        `<span class="lg-item"><i class="lg-dot" style="background:${c.color}"></i>${escapeHtml(c.name)}</span>`,
    )
    .join('');
}

/** 两个 YYYY-MM-DD 之间的天数差。 */
function spanDays(from: string, to: string): number {
  const f = new Date(`${from}T00:00:00`);
  const t = new Date(`${to}T00:00:00`);
  return Math.round((t.getTime() - f.getTime()) / 86_400_000);
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

// ---------------------------------------------------------------- 应用分类

function metaFor(label: string): AppMeta | undefined {
  return appMetas.get(label.toLowerCase());
}

/** 展示名：自定义名（详情弹窗里改的）优先，其次软件自带的名字，最后退回进程名。 */
function appTitle(label: string): string {
  const custom = displayNameFor(label);
  if (custom) return custom;
  const name = metaFor(label)?.displayName?.trim();
  if (name) return name;
  return label.replace(/\.exe$/i, '');
}

/** 没图标时的占位字：中文名首字，或进程名首字母。 */
function appInitial(label: string): string {
  const ch = appTitle(label).trim().charAt(0);
  return ch ? ch.toUpperCase() : '?';
}

/** 展示名排序：中文按拼音、英文按字母，忽略大小写。 */
const appTitleCollator = new Intl.Collator('zh-Hans-CN', { numeric: true, sensitivity: 'base' });

/** 「今天 / 昨天 / 3 天前 / 2 个月前」，按日历天算，不看时刻。 */
function lastSeenText(process: string): string {
  const day = appLastDays.get(process.toLowerCase());
  if (!day) return '';
  const [y, m, d] = day.split('-').map(Number);
  if (!y || !m || !d) return '';
  const today = new Date();
  const todayMid = new Date(today.getFullYear(), today.getMonth(), today.getDate());
  const diff = Math.round((todayMid.getTime() - new Date(y, m - 1, d).getTime()) / 86_400_000);
  if (diff <= 0) return '今天';
  if (diff === 1) return '昨天';
  if (diff < 30) return `${diff} 天前`;
  if (diff < 365) return `${Math.floor(diff / 30)} 个月前`;
  return `${Math.floor(diff / 365)} 年前`;
}

/** 分类筛选 chips：全部 + 各顶层分类，带应用计数。 */
function renderAppCatChips(): void {
  const host = $('app-cat-chips');
  const candidates = allApps.filter((r) => r.keydown + r.mousedown > 0);
  const countFor = (id: string) =>
    candidates.filter((r) => rootOf(categoryForApp(r.label).id).id === id).length;
  const chip = (id: string, label: string, color: string | null) => {
    const n = id === 'all' ? candidates.length : countFor(id);
    return (
      `<button class="chip cat-chip${appCatChip === id ? ' active' : ''}" data-chip="${escapeHtml(id)}" type="button">` +
      (color ? `<i class="cat-chip-dot" style="background:${escapeHtml(color)}"></i>` : '') +
      `${escapeHtml(label)}<b>${n}</b></button>`
    );
  };
  const parts: string[] = [chip('all', '全部', null)];
  for (const c of listCategories().filter((x) => !x.parentId)) {
    parts.push(chip(c.id, `${c.icon} ${c.name}`, c.color));
  }
  host.innerHTML = parts.join('');
}

function renderAppCategoryList(): void {
  const host = $('app-cat-list');
  const filter = appFilter.trim().toLowerCase();
  const candidates = allApps.filter((r) => r.keydown + r.mousedown > 0);
  const rows = candidates.filter((r) => {
    if (appCatChip !== 'all' && rootOf(categoryForApp(r.label).id).id !== appCatChip) return false;
    if (!filter) return true;
    const meta = metaFor(r.label);
    const hay = [r.label, meta?.displayName ?? '', meta?.exePath ?? '', displayNameFor(r.label) ?? '']
      .join(' ')
      .toLowerCase();
    return hay.includes(filter);
  });
  rows.sort((a, b) => appTitleCollator.compare(appTitle(a.label), appTitle(b.label)));

  renderAppCatChips();

  const countEl = $('app-cat-count');
  countEl.textContent = candidates.length ? `${rows.length} / ${candidates.length} 个程序` : '';

  if (!rows.length) {
    host.innerHTML = filter
      ? '<div class="chart-empty">没有匹配的程序。</div>'
      : appCatChip !== 'all'
        ? '<div class="chart-empty">这个分类下还没有程序，点上面的「全部」看看。</div>'
        : '<div class="chart-empty">还没有可归类的程序，先打开「记录输入」攒一点数据。</div>';
    return;
  }

  host.innerHTML = rows
    .map((r) => {
      const meta = metaFor(r.label);
      const cat = categoryForApp(r.label);
      const title = appTitle(r.label);
      const path = meta?.exePath ?? '';
      const ver = meta?.version ? `v${meta.version.replace(/^v/i, '')}` : '';
      const icon = meta?.icon
        ? `<img src="${meta.icon}" alt="" />`
        : `<span class="app-card-initial">${escapeHtml(appInitial(r.label))}</span>`;
      const seen = lastSeenText(r.label);
      // 两个口径分开写，别合成一个数：看视频两小时前台很长、活跃是 0，
      // 合成一个「总计」会让人以为统计坏了。
      const durParts: string[] = [];
      if (r.focusSeconds > 0) durParts.push(`前台 ${fmtDuration(r.focusSeconds)}`);
      if (r.activeSeconds > 0) durParts.push(`活跃 ${fmtDuration(r.activeSeconds)}`);
      const duration = durParts.length ? durParts.join(' · ') : '暂无计时';
      const ignoredBadge = isIgnored(r.label)
        ? '<span class="app-ignored-badge" title="已忽略：不再记录新数据，统计里也隐藏">已忽略</span>'
        : '';
      return (
        `<div class="app-card${isIgnored(r.label) ? ' ignored' : ''}" data-app="${escapeHtml(r.label)}" role="button" tabindex="0">` +
        `<div class="app-card-icon">${icon}</div>` +
        '<div class="app-card-main">' +
        '<div class="app-card-line">' +
        `<span class="app-card-name" title="${escapeHtml(title)}">${escapeHtml(title)}</span>` +
        (ver ? `<span class="app-card-ver">${escapeHtml(ver)}</span>` : '') +
        '</div>' +
        `<div class="app-card-path${path ? '' : ' missing'}" title="${escapeHtml(path || '未记录路径')}">${escapeHtml(
          path || '未记录路径（程序没在运行，注册表里也没登记）',
        )}</div>` +
        '<div class="app-card-foot">' +
        `<button class="app-cat-badge badge-btn" data-act="badge" style="--cat-color:${escapeHtml(cat.color)}" title="点击快速更换分类">` +
        `<i></i>${escapeHtml(cat.name)}</button>` +
        ignoredBadge +
        '<span class="app-card-sep">·</span>' +
        `<span>${escapeHtml(duration)}</span>` +
        (seen ? `<span class="app-card-sep">·</span><span>${escapeHtml(seen)}</span>` : '') +
        '</div></div></div>'
      );
    })
    .join('');
}

function renderCategoryPicker(app: string): void {
  const host = $('app-cat-pick');
  const cats = listCategories();
  const currentId = categoryForApp(app).id;
  const button = (id: string, color: string, label: string, child = false) =>
    `<button class="cat-pick${id === currentId ? ' active' : ''}${child ? ' child' : ''}" ` +
    `data-cat="${escapeHtml(id)}" type="button"><i style="background:${escapeHtml(color)}"></i>` +
    `${escapeHtml(label)}</button>`;

  const parts: string[] = [];
  for (const root of cats.filter((c) => !c.parentId)) {
    parts.push(button(root.id, root.color, `${root.icon} ${root.name}`));
    for (const child of cats.filter((c) => c.parentId === root.id)) {
      parts.push(button(child.id, child.color, `${child.icon} ${child.name}`, true));
    }
  }
  host.innerHTML = parts.join('');
}

function openAppCategoryModal(app: string): void {
  editingApp = app;
  const meta = metaFor(app);

  const iconHost = $('app-detail-icon');
  iconHost.innerHTML = meta?.icon
    ? `<img src="${meta.icon}" alt="" />`
    : `<span class="app-detail-initial">${escapeHtml(appInitial(app))}</span>`;

  $('app-detail-name').textContent = appTitle(app);
  $('app-detail-ver').textContent = meta?.version ? `v${meta.version.replace(/^v/i, '')}` : '';

  const path = meta?.exePath ?? '';
  const pathEl = $('app-detail-path');
  pathEl.textContent = path || '未记录路径（程序没在运行，注册表里也没登记）';
  pathEl.classList.toggle('missing', !path);

  ($('app-detail-copy') as HTMLButtonElement).disabled = !path;
  ($('app-detail-reveal') as HTMLButtonElement).disabled = !path;

  // 改名输入框复位：每次打开都是「显示态」，点「改名」才进入编辑
  const nameInput = $('app-detail-name-input') as HTMLInputElement;
  nameInput.hidden = true;
  nameInput.value = '';
  $('app-detail-name').hidden = false;
  ($('app-detail-rename') as HTMLButtonElement).textContent = '改名';

  // 忽略开关同步当前状态
  ($('sw-app-ignored') as HTMLInputElement).checked = isIgnored(app);

  renderCategoryPicker(app);
  showModal('app-cat-modal');
  void loadAppStats();
}

// ---------------------------------------------------------------- 快速换类

function closeCatQuickDrop(): void {
  const drop = $('cat-quickdrop');
  drop.hidden = true;
  quickDropApp = null;
}

/** 点卡片上的分类徽标：在徽标正下方弹一个分类下拉，选完即改，不用进详情。 */
function openCatQuickDrop(app: string, anchor: HTMLElement): void {
  const drop = $('cat-quickdrop');
  if (quickDropApp === app && !drop.hidden) {
    closeCatQuickDrop();
    return;
  }
  quickDropApp = app;
  const cats = listCategories();
  const currentId = categoryForApp(app).id;
  const button = (id: string, color: string, label: string, child = false) =>
    `<button class="cat-pick${id === currentId ? ' active' : ''}${child ? ' child' : ''}" ` +
    `data-cat="${escapeHtml(id)}" type="button"><i style="background:${escapeHtml(color)}"></i>` +
    `${escapeHtml(label)}</button>`;

  const parts: string[] = [];
  for (const root of cats.filter((c) => !c.parentId)) {
    parts.push(button(root.id, root.color, `${root.icon} ${root.name}`));
    for (const child of cats.filter((c) => c.parentId === root.id)) {
      parts.push(button(child.id, child.color, `${child.icon} ${child.name}`, true));
    }
  }
  drop.innerHTML =
    `<div class="cat-quickdrop-title">${escapeHtml(appTitle(app))} 归到哪个分类</div>` +
    parts.join('');
  drop.hidden = false;

  // 定位：贴着徽标往下放，靠右/靠下放不下时往回收
  const rect = anchor.getBoundingClientRect();
  const dropW = drop.offsetWidth;
  const dropH = drop.offsetHeight;
  let x = rect.left;
  let y = rect.bottom + 6;
  if (x + dropW > window.innerWidth - 8) x = Math.max(8, window.innerWidth - dropW - 8);
  if (y + dropH > window.innerHeight - 8) y = Math.max(8, rect.top - dropH - 6);
  drop.style.left = `${x}px`;
  drop.style.top = `${y}px`;
}

// ---------------------------------------------------------------- 应用详情统计

async function loadAppStats(): Promise<void> {
  const app = editingApp;
  if (!app) return;
  const to = dayStr(new Date());
  const from = appStatsRange === 0 ? (status?.firstDay ?? to) : daysAgoInclusive(appStatsRange);
  $('app-stat-cards').innerHTML = '<div class="chart-empty">加载中…</div>';
  $('app-heatmap').innerHTML = '<div class="chart-empty">加载中…</div>';
  const st = await guard(() => call<AppStats>('input_log_app_stats', { process: app, from, to }));
  // 弹窗已经切到别的程序（或已关闭）就别再往界面上写了
  if (!st || editingApp !== app) return;

  if (!st.found) {
    $('app-stat-cards').innerHTML = '<div class="chart-empty">这个程序还没有统计数据。</div>';
    $('app-heatmap').innerHTML = '<div class="chart-empty">暂无前台时长数据。</div>';
    renderBars($('app-trend'), [], { height: 130, maxLabels: 8 });
    $('app-topkeys').innerHTML = '<div class="chart-empty">暂无按键数据。</div>';
    return;
  }

  const card = (label: string, value: string) =>
    `<div class="app-stat-card"><div class="app-stat-label">${escapeHtml(label)}</div>` +
    `<div class="app-stat-value">${escapeHtml(value)}</div></div>`;
  $('app-stat-cards').innerHTML =
    card('击键', fmtNum(st.keydown)) +
    card('鼠标点击', fmtNum(st.mousedown)) +
    card('活跃时长', fmtDuration(st.activeSeconds)) +
    card('前台时长', fmtDuration(st.focusSeconds));

  renderAppHeatmap(st.heatmap);
  renderBars(
    $('app-trend'),
    st.daily.map((d) => ({ label: d.day.slice(5), value: d.keydown })),
    { height: 130, maxLabels: 8 },
  );
  renderAppTopKeys(st.topKeys);
}

/** 热力图：行 = 天，列 = 0~23 点，颜色深浅 = 该程序这个小时挂在前台的秒数。 */
function renderAppHeatmap(cells: AppStats['heatmap']): void {
  const host = $('app-heatmap');
  if (!cells.length) {
    host.innerHTML = '<div class="chart-empty">该范围内没有前台时长数据。</div>';
    return;
  }
  const max = Math.max(...cells.map((c) => c.seconds), 1);
  const byKey = new Map(cells.map((c) => [`${c.day}|${c.hour}`, c.seconds]));
  const days = Array.from(new Set(cells.map((c) => c.day))).sort();
  // 开方拉伸：1 小时挂满和 5 分钟挂一下在视觉上也能拉开层次。
  // 颜色用主题 accent 的 RGB，深浅两个主题都成立。
  const color = (secs: number): string => {
    const t = Math.sqrt(Math.min(1, secs / max));
    const alpha = 0.14 + t * 0.86;
    return `rgba(126, 166, 255, ${alpha.toFixed(3)})`;
  };
  const parts: string[] = ['<div class="hm-grid">'];
  parts.push('<div class="hm-hours"><span></span>');
  for (let h = 0; h < 24; h++) {
    parts.push(`<span>${h % 3 === 0 ? String(h).padStart(2, '0') : ''}</span>`);
  }
  parts.push('</div>');
  for (const day of days) {
    parts.push(
      `<div class="hm-row"><span class="hm-day" title="${escapeHtml(day)}">${escapeHtml(day.slice(5))}</span><div class="hm-cells">`,
    );
    for (let h = 0; h < 24; h++) {
      const secs = byKey.get(`${day}|${h}`) ?? 0;
      const style = secs > 0 ? ` style="background:${color(secs)}"` : '';
      parts.push(
        `<span class="hm-cell${secs > 0 ? '' : ' empty'}"${style} title="${escapeHtml(
          `${day} ${String(h).padStart(2, '0')}:00 · ${secs > 0 ? fmtDuration(secs) : '无记录'}`,
        )}"></span>`,
      );
    }
    parts.push('</div></div>');
  }
  parts.push('</div>');
  host.innerHTML = parts.join('');
}

/** 该程序的 top 按键：轻量横条，不进图表登记表（弹窗一关就没了）。 */
function renderAppTopKeys(keys: AppStats['topKeys']): void {
  const host = $('app-topkeys');
  if (!keys.length) {
    host.innerHTML = '<div class="chart-empty">暂无按键数据。</div>';
    return;
  }
  const max = Math.max(...keys.map((k) => k.n), 1);
  host.innerHTML = keys
    .map(
      (k) =>
        `<div class="topkey-row"><span class="topkey-name">${escapeHtml(k.label)}</span>` +
        `<div class="topkey-bar"><i style="width:${Math.max(4, Math.round((k.n / max) * 100))}%"></i></div>` +
        `<span class="topkey-n">${fmtNum(k.n)}</span></div>`,
    )
    .join('');
}

async function loadApps(): Promise<void> {
  if (!status) return;
  const to = dayStr(new Date());
  const from = status.firstDay ?? to;
  const rows = await guard(() => call<SummaryPoint[]>('input_log_summary', { from, to, dim: 'app' }));
  allApps = rows ?? [];
  // 先用进程名出一版骨架，图标和中文名解析完再重画，避免空白等待
  renderAppCategoryList();

  if (await ensureAppMetas(allApps.map((r) => r.label))) {
    renderAppCategoryList();
  }

  if (!appMetaLoaded) {
    const acts = await guard(() => call<AppActivity[]>('input_log_app_activity', { from, to }));
    for (const act of acts ?? []) appLastDays.set(act.process.toLowerCase(), act.lastDay);
    appMetaLoaded = true;
    renderAppCategoryList();
  }
}

/** 批量补齐缺失的应用元信息（中文名 / 图标），结果进 appMetas 缓存。返回是否真的拉取了新数据。 */
async function ensureAppMetas(labels: string[]): Promise<boolean> {
  const missing = labels.filter((label) => !appMetas.has(label.toLowerCase()));
  if (!missing.length) return false;
  const metas = await guard(() => call<AppMeta[]>('app_meta_get', { processes: missing }));
  for (const meta of metas ?? []) appMetas.set(meta.process.toLowerCase(), meta);
  return true;
}

// ---------------------------------------------------------------- 轨迹

let trail: TrailPlayer | null = null;
let trailLoaded = false;
let trailPointTotal = 0;
/** 最近一次加载的原始采样点（未筛选），切换分类筛选时在内存里重放，不重新请求后端 */
let trailRows: TrailPoint[] = [];
let trailRangeFrom = 0;
let trailRangeTo = 0;
/** 分类筛选：集合内的 key 显示、不在的不显示。首次加载初始化为全选。 */
const trailFilter = new Set<string>();
let trailFilterInit = false;
/** 未归类筛选 key：无 cat、锁屏/隐私期间、以及分类已被删除的点都归这组 */
const TRAIL_CAT_NONE = '__none__';

/** 采样点 → 筛选 key。分类已被删除的点跟随「未归类」一起筛，不留孤儿项。 */
function trailFilterKey(p: TrailPoint): string {
  return p.cat && findCategory(p.cat) ? p.cat : TRAIL_CAT_NONE;
}

/** 按当前筛选集合过滤点集并重放（光标归零）。副标题同步显示已显示/总数。 */
function applyTrailFilter(): void {
  const shown = trailRows.filter((p) => trailFilter.has(trailFilterKey(p)));
  trail?.setPoints(shown);
  const sub = $('trail-sub');
  if (!sub) return;
  sub.textContent = trailRows.length
    ? `${fmtNum(shown.length)} / ${fmtNum(trailRows.length)} 个采样点 · ${timeStr(trailRangeFrom)} ~ ${timeStr(trailRangeTo)}`
    : '只记录坐标与时间，不记录任何画面内容';
}

/** 筛选集合与这次数据的 key 求交：删掉已不存在的 key，且保证至少剩一个。 */
function syncTrailFilter(): void {
  const present = new Set(trailRows.map(trailFilterKey));
  for (const key of Array.from(trailFilter)) {
    if (!present.has(key)) trailFilter.delete(key);
  }
  if (!trailFilter.size && present.size) {
    trailFilter.add(present.values().next().value as string);
  }
}

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
  // 分类着色：采样点的 cat → 分类色。每次绘制动态取最新分类设置，
  // 改名/换色立即生效，不用重载数据；未归类/老数据用灰色。
  trail.setCategoryColors((cat) => findCategory(cat)?.color ?? '#8b909c');
  return trail;
}

/**
 * 轨迹页图例兼分类筛选：只列出这次回放里真实出现过的分类，
 * 点击 chip 切换显示/隐藏（至少保留一个）。
 */
function renderTrailLegend(): void {
  const el = document.getElementById('trail-legend');
  if (!el) return;
  const order: string[] = [];
  let hasNone = false;
  for (const p of trailRows) {
    const key = trailFilterKey(p);
    if (key === TRAIL_CAT_NONE) hasNone = true;
    else if (!order.includes(key)) order.push(key);
  }
  const items: string[] = [];
  for (const id of order) {
    const c = findCategory(id);
    if (!c) {
      // 分类已被删除：点会画灰，归入未归类
      hasNone = true;
      continue;
    }
    items.push(trailFilterChip(id, c.name, c.color));
  }
  if (hasNone) {
    items.push(trailFilterChip(TRAIL_CAT_NONE, '未归类', '#8b909c'));
  }
  el.innerHTML = items.join('');
}

function trailFilterChip(key: string, name: string, color: string): string {
  const off = trailFilter.has(key) ? '' : ' off';
  return `<span class="lg-item trail-filter${off}" data-cat-key="${escapeHtml(key)}" role="button" title="点击显示/隐藏该分类"><i class="lg-dot" style="background:${escapeHtml(color)}"></i>${escapeHtml(name)}</span>`;
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
  // 确保播放器（含分类着色注入）已就绪，applyTrailFilter 里的 setPoints 才有归宿
  ensureTrailPlayer();
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
  trailRows = rows;
  trailPointTotal = rows.length;
  trailRangeFrom = from;
  trailRangeTo = to;
  if (!trailFilterInit) {
    // 首次加载：全选所有出现过的分类
    for (const p of rows) trailFilter.add(trailFilterKey(p));
    trailFilterInit = true;
  } else {
    // 时间范围变了：这次数据里没有的 key 会被剪掉（syncTrailFilter 保证至少剩一个）
    syncTrailFilter();
  }
  renderTrailLegend();
  applyTrailFilter();
  trailLoaded = true;
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

async function onToggleEnabled(checked: boolean): Promise<void> {
  if (checked) {
    // 门禁：开启记录必须已设密码且本次运行已解锁（后端同样强制）
    const st = pwStatus ?? (await refreshPwStatus().catch(() => null));
    if (!st) {
      ($('sw-enabled') as HTMLInputElement).checked = false;
      return;
    }
    if (!st.set) {
      openPwModal('set', { blocking: true, after: continueEnable });
      return;
    }
    if (!st.unlocked) {
      openPwModal('unlock', { blocking: true, after: continueEnable });
      return;
    }
  }
  if (checked && localStorage.getItem(LS_CONSENT) !== '1') {
    ($('sw-enabled') as HTMLInputElement).checked = false;
    showModal('consent-modal');
    return;
  }
  void configure({ enabled: checked });
}

/** 密码设好/解锁后的续动作：回到「打开记录开关」的流程。 */
async function continueEnable(): Promise<void> {
  ($('sw-enabled') as HTMLInputElement).checked = true;
  await onToggleEnabled(true);
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

// ---------------------------------------------------------------- 数据同步

let syncStatus: SyncStatus | null = null;

function fillSyncForm(cfg: SyncConfig): void {
  ($('sw-sync') as HTMLInputElement).checked = cfg.enabled;
  ($('input-sync-server') as HTMLInputElement).value = cfg.server;
  ($('input-sync-account') as HTMLInputElement).value = cfg.account;
  ($('input-sync-password') as HTMLInputElement).value = cfg.password;
  ($('input-sync-label') as HTMLInputElement).value = cfg.machineLabel;
  ($('select-sync-interval') as HTMLSelectElement).value = String(cfg.intervalMinutes);
  ($('input-sync-retain') as HTMLInputElement).value = String(cfg.retainDays);
}

function collectSyncConfig(): SyncConfig {
  return {
    enabled: ($('sw-sync') as HTMLInputElement).checked,
    server: ($('input-sync-server') as HTMLInputElement).value.trim(),
    account: ($('input-sync-account') as HTMLInputElement).value.trim(),
    password: ($('input-sync-password') as HTMLInputElement).value,
    machineLabel: ($('input-sync-label') as HTMLInputElement).value.trim(),
    intervalMinutes: Number(($('select-sync-interval') as HTMLSelectElement).value) || 0,
    retainDays: Math.max(1, Math.min(365, Number(($('input-sync-retain') as HTMLInputElement).value) || 15)),
  };
}

function renderSyncStatus(st: SyncStatus): void {
  syncStatus = st;
  const el = $('sync-status');
  if (!st.configured) {
    el.textContent = '尚未配置：填好服务器、账号和应用密码，点「测试连接」验证';
  } else if (st.lastSyncAt == null) {
    el.textContent = '已配置，等待首次同步（启动 / 退出 / 到点各试一次）';
  } else if (st.lastSyncOk) {
    el.textContent = `上次同步 ${timeStr(st.lastSyncAt)}：成功`;
  } else {
    el.textContent = `上次同步 ${timeStr(st.lastSyncAt)}：失败（${st.lastSyncError ?? '未知原因'}）`;
  }
  $('sync-machine').textContent =
    st.machineId === '—' ? '未生成' : `本机 ID ${st.machineId.slice(0, 8)}`;

  ($('sw-sync') as HTMLInputElement).checked = st.enabled;

  // 仪表盘徽标：只在开启且配置完整时露面
  const cell = $('dash-sync-cell');
  if (st.enabled && st.configured) {
    cell.hidden = false;
    $('dash-sync').textContent =
      st.lastSyncAt == null
        ? '待首次同步'
        : st.lastSyncOk
          ? `✓ ${timeStr(st.lastSyncAt)}`
          : `✗ ${timeStr(st.lastSyncAt)}`;
  } else {
    cell.hidden = true;
  }
}

async function refreshSyncStatus(): Promise<void> {
  const st = await guard(() => call<SyncStatus>('sync_get_status'));
  if (st) renderSyncStatus(st);
}

async function loadSyncConfig(): Promise<void> {
  const s = await guard(() => call<{ sync: SyncConfig }>('get_settings'));
  if (s?.sync) fillSyncForm(s.sync);
  await refreshSyncStatus();
}

async function saveSync(): Promise<void> {
  const cfg = collectSyncConfig();
  const saved = await guard(() => call<SyncConfig>('sync_configure', { sync: cfg }));
  if (!saved) return;
  toast(saved.enabled ? '同步设置已保存，正在按新配置执行' : '同步设置已保存');
  await refreshSyncStatus();
}

async function testSync(): Promise<void> {
  const cfg = collectSyncConfig();
  toast('正在测试连接…');
  const msg = await guard(() => call<string>('sync_test_connection', { sync: cfg }));
  if (msg) toast(msg);
}

async function syncNow(): Promise<void> {
  const btn = $('btn-sync-now') as HTMLButtonElement;
  if (btn.disabled) return;
  btn.disabled = true;
  try {
    const out = await guard(() => call<SyncOutcome>('sync_now'));
    if (out) {
      const parts = [`上传 ${out.uploaded} 包`, `导入 ${out.imported} 包`];
      if (out.skipped) parts.push(`已同步过 ${out.skipped} 包`);
      if (out.categoriesMerged) parts.push('分类与标注已合并');
      if (out.warnings.length) parts.push(`警告 ${out.warnings.length} 条`);
      toast(`同步完成：${parts.join('，')}`);
      await refreshSyncStatus();
    }
  } finally {
    btn.disabled = false;
  }
}

// ---------------------------------------------------------------- 数据备份

let backupStatusInfo: BackupStatusInfo | null = null;
/** 恢复弹窗当前选中的快照时间点；null = 未选。 */
let restoreSelection: number | null = null;
/** 恢复弹窗当前展示的快照列表。 */
let restoreSnapshots: SnapshotMeta[] = [];
/** 恢复二次确认阶段：false = 选快照，true = 已警告等最终确认。 */
let restoreConfirmed = false;

function fillBackupForm(cfg: BackupConfig): void {
  ($('sw-backup') as HTMLInputElement).checked = cfg.enabled;
  ($('select-backup-interval') as HTMLSelectElement).value = String(cfg.intervalMinutes);
  ($('input-backup-retain') as HTMLInputElement).value = String(cfg.retentionDays);
}

function collectBackupConfig(): BackupConfig {
  return {
    enabled: ($('sw-backup') as HTMLInputElement).checked,
    intervalMinutes:
      Number(($('select-backup-interval') as HTMLSelectElement).value) || 30,
    retentionDays: Math.max(
      1,
      Math.min(365, Number(($('input-backup-retain') as HTMLInputElement).value) || 15),
    ),
  };
}

function renderBackupStatus(st: BackupStatusInfo): void {
  backupStatusInfo = st;
  ($('sw-backup') as HTMLInputElement).checked = st.enabled;
  ($('select-backup-interval') as HTMLSelectElement).value = String(st.intervalMinutes);
  ($('input-backup-retain') as HTMLInputElement).value = String(st.retentionDays);

  const el = $('backup-status');
  if (!st.enabled) {
    el.textContent = st.lastBackupTs
      ? `自动备份已关闭。上次备份 ${timeStr(st.lastBackupTs)}`
      : '未开启：开启后按周期把数据库快照上传网盘';
  } else if (st.lastBackupTs === 0) {
    el.textContent = '已开启，等待首次备份（开启时立刻做一次）';
  } else {
    el.textContent = st.lastMessage || (st.lastOk ? '上次备份成功' : '上次备份失败');
  }
  $('backup-last').textContent = st.lastBackupTs ? timeStr(st.lastBackupTs) : '—';
}

async function refreshBackupStatus(): Promise<void> {
  const st = await guard(() => call<BackupStatusInfo>('backup_status'));
  if (st) renderBackupStatus(st);
}

async function loadBackupConfig(): Promise<void> {
  const s = await guard(() => call<{ backup: BackupConfig }>('get_settings'));
  if (s?.backup) fillBackupForm(s.backup);
  await refreshBackupStatus();
}

async function saveBackup(): Promise<void> {
  const cfg = collectBackupConfig();
  if (cfg.enabled && !backupStatusInfo) {
    // 状态还没拉到就先拉一次，避免误判同步配置
    await refreshBackupStatus();
  }
  const saved = await guard(() => call<BackupConfig>('backup_configure', { backup: cfg }));
  if (!saved) return;
  toast(saved.enabled ? '备份设置已保存，正在做首次快照' : '备份设置已保存');
  await refreshBackupStatus();
}

async function backupNow(): Promise<void> {
  const btn = $('btn-backup-now') as HTMLButtonElement;
  if (btn.disabled) return;
  btn.disabled = true;
  btn.textContent = '备份中…';
  try {
    const snap = await guard(() => call<SnapshotMeta>('backup_run_now'));
    if (snap) {
      toast(`备份完成：${timeStr(snap.ts)}`);
      await refreshBackupStatus();
    }
  } finally {
    btn.disabled = false;
    btn.textContent = '立即备份';
  }
}

async function openRestoreModal(): Promise<void> {
  restoreSelection = null;
  restoreConfirmed = false;
  $('restore-confirm').hidden = true;
  ($('restore-ok') as HTMLButtonElement).disabled = true;
  ($('restore-ok') as HTMLButtonElement).textContent = '恢复';
  $('restore-empty').textContent = '正在读取网盘快照…';
  $('restore-empty').hidden = false;
  showModal('restore-modal');
  const list = await guard(() => call<SnapshotMeta[]>('backup_list'));
  if (!list) {
    hideModal('restore-modal');
    return;
  }
  restoreSnapshots = list;
  renderRestoreList();
}

function renderRestoreList(): void {
  const box = $('restore-list');
  box.querySelectorAll('.restore-item').forEach((el) => el.remove());
  const empty = $('restore-empty');
  if (!restoreSnapshots.length) {
    empty.textContent = '网盘上还没有快照：先备份一次，或稍等自动备份';
    empty.hidden = false;
    return;
  }
  empty.hidden = true;
  for (const snap of restoreSnapshots) {
    const item = document.createElement('div');
    item.className =
      'restore-item' + (snap.ts === restoreSelection ? ' selected' : '');
    item.textContent = timeStr(snap.ts);
    if (snap.files.length) {
      const files = document.createElement('span');
      files.className = 'restore-files';
      files.textContent =
        snap.files.length === 3 ? '含轨迹' : '按键统计';
      item.appendChild(files);
    }
    item.addEventListener('click', () => {
      restoreSelection = snap.ts;
      restoreConfirmed = false;
      $('restore-confirm').hidden = true;
      ($('restore-ok') as HTMLButtonElement).textContent = '恢复';
      ($('restore-ok') as HTMLButtonElement).disabled = false;
      box.querySelectorAll('.restore-item').forEach((el) => el.classList.remove('selected'));
      item.classList.add('selected');
    });
    box.appendChild(item);
  }
}

async function confirmRestore(): Promise<void> {
  if (restoreSelection == null) return;
  // 两段式确认：第一下只弹警告
  if (!restoreConfirmed) {
    restoreConfirmed = true;
    $('restore-confirm').hidden = false;
    ($('restore-ok') as HTMLButtonElement).textContent = '确认恢复';
    return;
  }
  const btn = $('restore-ok') as HTMLButtonElement;
  btn.disabled = true;
  btn.textContent = '恢复中…';
  const report = await guard(() => call<RestoreReport>('backup_restore', { ts: restoreSelection }));
  if (report) {
    hideModal('restore-modal');
    toast(`已恢复：${report.restored.join('、')}`);
    // 数据被整库替换：状态、图表、应用列表全部重新拉
    await refreshStatus();
    await refreshBackupStatus();
    void loadStats();
  } else {
    btn.disabled = false;
    btn.textContent = '确认恢复';
  }
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
  if (page === 'categories') renderCategoryList();
  if (page === 'app-categories') {
    void loadApps().then(() => {
      // 调试用：stats.html?page=app-categories&app=chrome.exe 直接弹出归类面板
      const app = new URLSearchParams(location.search).get('app');
      if (app) openAppCategoryModal(app);
    });
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
    // 点快捷 chip 即退出自定义区间
    customFrom = '';
    customTo = '';
    ($('input-from') as HTMLInputElement).value = '';
    ($('input-to') as HTMLInputElement).value = '';
    document.querySelectorAll<HTMLButtonElement>('#range-row .chip').forEach((c) => {
      c.classList.toggle('active', c === btn);
    });
    void loadStats();
  });

  // 自定义起止日期：两个都填了才生效
  const applyCustomRange = () => {
    customFrom = ($('input-from') as HTMLInputElement).value;
    customTo = ($('input-to') as HTMLInputElement).value;
    if (!customFrom || !customTo) return;
    document.querySelectorAll<HTMLButtonElement>('#range-row .chip').forEach((c) => {
      c.classList.remove('active');
    });
    void loadStats();
  };
  $('input-from').addEventListener('change', applyCustomRange);
  $('input-to').addEventListener('change', applyCustomRange);

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

  // ---- 数据同步 ----
  $('btn-sync-save').addEventListener('click', () => void saveSync());
  $('btn-sync-test').addEventListener('click', () => void testSync());
  $('btn-sync-now').addEventListener('click', () => void syncNow());
  // 开关切换 = 立即按当前表单保存（与其它开关「改动立即生效」的行为一致）
  ($('sw-sync') as HTMLInputElement).addEventListener('change', () => {
    void saveSync();
  });

  // ---- 数据备份 ----
  $('btn-backup-save').addEventListener('click', () => void saveBackup());
  $('btn-backup-now').addEventListener('click', () => void backupNow());
  $('btn-backup-list').addEventListener('click', () => void openRestoreModal());
  $('restore-cancel').addEventListener('click', () => hideModal('restore-modal'));
  $('restore-ok').addEventListener('click', () => void confirmRestore());
  ($('sw-backup') as HTMLInputElement).addEventListener('change', () => {
    void saveBackup();
  });

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

  $('app-cat-chips').addEventListener('click', (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLButtonElement>('.chip');
    if (!btn?.dataset.chip) return;
    appCatChip = btn.dataset.chip;
    renderAppCategoryList();
  });

  $('app-cat-list').addEventListener('click', (ev) => {
    const target = ev.target as HTMLElement;
    // 点分类徽标 = 快速换类下拉；点卡片其余部分 = 详情弹窗
    const badge = target.closest<HTMLElement>('.app-cat-badge');
    if (badge) {
      const card = badge.closest<HTMLElement>('.app-card');
      if (card?.dataset.app) openCatQuickDrop(card.dataset.app, badge);
      return;
    }
    const card = target.closest<HTMLElement>('.app-card');
    if (!card?.dataset.app) return;
    openAppCategoryModal(card.dataset.app);
  });

  $('app-cat-list').addEventListener('keydown', (ev) => {
    if (ev.key !== 'Enter' && ev.key !== ' ') return;
    const card = (ev.target as HTMLElement).closest<HTMLElement>('.app-card');
    if (!card?.dataset.app) return;
    ev.preventDefault();
    openAppCategoryModal(card.dataset.app);
  });

  $('cat-quickdrop').addEventListener('click', (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLElement>('.cat-pick');
    if (!btn?.dataset.cat || !quickDropApp) return;
    const target = quickDropApp;
    setAppCategory(target, btn.dataset.cat);
    toast(`已把 ${appTitle(target)} 归到「${categoryForApp(target).name}」`);
    closeCatQuickDrop();
    renderAppCategoryList();
  });

  // 点下拉以外的任何地方都收起（徽标自己的点击由上面的监听处理，不会到这里）
  document.addEventListener('click', (ev) => {
    if (quickDropApp === null) return;
    const target = ev.target as HTMLElement;
    if (target.closest('#cat-quickdrop') || target.closest('.app-cat-badge')) return;
    closeCatQuickDrop();
  });

  $('app-cat-pick').addEventListener('click', (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLElement>('.cat-pick');
    if (!btn?.dataset.cat || !editingApp) return;
    const target = editingApp;
    setAppCategory(target, btn.dataset.cat);
    toast(`已把 ${appTitle(target)} 归到「${categoryForApp(target).name}」`);
    hideModal('app-cat-modal');
    editingApp = null;
    renderAppCategoryList();
  });

  $('app-cat-cancel').addEventListener('click', () => {
    hideModal('app-cat-modal');
    editingApp = null;
  });

  $('app-detail-copy').addEventListener('click', () => {
    const path = editingApp ? metaFor(editingApp)?.exePath : null;
    if (!path) return;
    void navigator.clipboard.writeText(path).then(
      () => toast('路径已复制'),
      () => toast('复制失败，请手动选中路径'),
    );
  });

  $('app-detail-reveal').addEventListener('click', () => {
    const path = editingApp ? metaFor(editingApp)?.exePath : null;
    if (!path) return;
    void guard(() => call('open_path_folder', { path }));
  });

  // 改名：点「改名」进入编辑，再点「保存」（或回车）生效；清空 = 恢复软件默认名
  $('app-detail-rename').addEventListener('click', () => {
    const app = editingApp;
    if (!app) return;
    const btn = $('app-detail-rename') as HTMLButtonElement;
    const input = $('app-detail-name-input') as HTMLInputElement;
    const nameSpan = $('app-detail-name');
    if (input.hidden) {
      // 预填当前生效的展示名（自定义名优先，其次 exe 名）
      input.value = appTitle(app);
      nameSpan.hidden = true;
      input.hidden = false;
      btn.textContent = '保存';
      input.focus();
      input.select();
      return;
    }
    const val = input.value.trim();
    setDisplayName(app, val);
    nameSpan.textContent = appTitle(app);
    nameSpan.hidden = false;
    input.hidden = true;
    btn.textContent = '改名';
    toast(val ? '显示名已更新' : '已清除自定义名，恢复软件默认名');
    renderAppCategoryList();
  });
  ($('app-detail-name-input') as HTMLInputElement).addEventListener('keydown', (ev) => {
    if (ev.key !== 'Enter') return;
    ev.preventDefault();
    ($('app-detail-rename') as HTMLButtonElement).click();
  });

  // 忽略开关：只影响新数据与统计口径，历史数据保留
  ($('sw-app-ignored') as HTMLInputElement).addEventListener('change', (ev) => {
    const app = editingApp;
    if (!app) return;
    const on = (ev.target as HTMLInputElement).checked;
    setIgnored(app, on);
    toast(
      on
        ? `已忽略 ${appTitle(app)}：不再记录它的新数据，统计里也会隐藏`
        : `已取消忽略 ${appTitle(app)}`,
    );
    renderAppCategoryList();
    if (document.getElementById('page-stats')?.classList.contains('active')) void loadStats();
  });

  // 详情统计的时间范围
  $('app-stats-range').addEventListener('click', (ev) => {
    const btn = (ev.target as HTMLElement).closest<HTMLButtonElement>('.chip');
    if (!btn?.dataset.range) return;
    appStatsRange = Number(btn.dataset.range);
    document.querySelectorAll<HTMLButtonElement>('#app-stats-range .chip').forEach((c) => {
      c.classList.toggle('active', c === btn);
    });
    void loadAppStats();
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

  // 图例 chip = 分类筛选开关（至少保留一个分类）
  $('trail-legend').addEventListener('click', (ev) => {
    const chip = (ev.target as HTMLElement).closest<HTMLElement>('.trail-filter');
    const key = chip?.dataset.catKey;
    if (!key) return;
    if (trailFilter.has(key)) {
      if (trailFilter.size <= 1) {
        toast('至少保留一个分类');
        return;
      }
      trailFilter.delete(key);
    } else {
      trailFilter.add(key);
    }
    renderTrailLegend();
    applyTrailFilter();
  });

  // 全屏播放：舞台 + 控制条一起进全屏，Esc、双击画布或再点一次按钮退出
  const toggleTrailFullscreen = (): void => {
    if (document.fullscreenElement) {
      void document.exitFullscreen();
      return;
    }
    $('trail-player')
      .requestFullscreen()
      .catch(() => toast('当前环境不支持全屏'));
  };
  $('trail-fullscreen').addEventListener('click', toggleTrailFullscreen);
  ($('trail-canvas') as HTMLCanvasElement).addEventListener('dblclick', toggleTrailFullscreen);
  document.addEventListener('fullscreenchange', () => {
    ($('trail-fullscreen') as HTMLButtonElement).textContent = document.fullscreenElement
      ? '退出全屏'
      : '全屏';
  });

  // 首次开启告知
  $('consent-cancel').addEventListener('click', () => hideModal('consent-modal'));
  $('consent-ok').addEventListener('click', () => {
    localStorage.setItem(LS_CONSENT, '1');
    hideModal('consent-modal');
    void configure({ enabled: true });
  });

  // 访问密码：入口按钮 + 弹窗内按钮 + 输入框回车提交
  $('pw-manage').addEventListener('click', () => {
    const st = pwStatus;
    if (!st) {
      void refreshPwStatus().then(() => openPwModal(pwStatus?.set ? 'change' : 'set'));
      return;
    }
    openPwModal(st.set ? 'change' : 'set');
  });
  $('pw-cancel').addEventListener('click', () => {
    pwPending = null;
    pwBlocking = false;
    hideModal('pw-modal');
  });
  $('pw-forgot').addEventListener('click', () => openPwModal('reset'));
  $('pw-ok').addEventListener('click', () => void submitPw());
  for (const id of ['input-pw-old', 'input-pw-new', 'input-pw-confirm'] as const) {
    ($(id) as HTMLInputElement).addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter') {
        ev.preventDefault();
        void submitPw();
      }
    });
  }

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
      trailRows = [];
      trailFilter.clear();
      trailFilterInit = false;
      renderTrailLegend();
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
      // 阻塞态的密码弹窗不允许点遮罩关掉
      if (mask.id === 'pw-modal' && pwBlocking) return;
      if (ev.target === mask) mask.classList.remove('show');
    });
  });

  window.addEventListener('keydown', (ev) => {
    if (ev.key === 'Escape') {
      document.querySelectorAll<HTMLElement>('.modal-mask.show').forEach((m) => {
        if (m.id === 'pw-modal' && pwBlocking) return;
        m.classList.remove('show');
      });
      pendingRetentionDays = null;
      editingCatId = null;
      pendingDeleteCatId = null;
      closeCatQuickDrop();
      // 恢复弹窗的两段式确认也要复位，Esc 后再打开不该停在警告态
      restoreSelection = null;
      restoreConfirmed = false;
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
  // 分类与应用标注存在 settings.json 里，先把权威版本拉回来再渲染任何列表
  await Promise.all([initCategories(), initAppOverrides()]);
  // 分类变了（新增/删除/归类），统计图与两个列表都要跟着变
  subscribe(() => {
    renderCategoryList();
    renderAppCategoryList();
    // 轨迹着色与图例也依赖分类设置：colorFor 是动态取的，重设一次触发重绘
    trail?.setCategoryColors((cat) => findCategory(cat)?.color ?? '#8b909c');
    // 图例筛选依赖分类存在性：分类被删会让部分 key 失效，需要修剪筛选集并重放；
    // 仅改名/换色时只刷新图例文案与颜色，不打断播放进度
    if (trailLoaded && trailRows.length) {
      const before = Array.from(trailFilter);
      syncTrailFilter();
      const pruned = before.length !== trailFilter.size;
      renderTrailLegend();
      if (pruned) applyTrailFilter();
    }
    if (document.getElementById('page-stats')?.classList.contains('active')) void loadStats();
  });
  // 自定义名 / 忽略变了：列表立即重画（改名会变标题，忽略会影响统计口径）
  subscribeOverrides(() => {
    renderAppCategoryList();
    if (document.getElementById('page-stats')?.classList.contains('active')) void loadStats();
  });

  // 门禁前置：已设密码但本次运行还没解锁 → 一进窗口就弹解锁窗。
  // 之后的 refreshStatus 会拿不到数据，但 handleLocked 看到弹窗已开不会重复弹。
  try {
    const st = await refreshPwStatus();
    if (st.set && !st.unlocked) openPwModal('unlock', { blocking: true });
  } catch {
    /* 密码状态不可用时保持静默，后续命令的 LOCKED 错误会兜底 */
  }

  await refreshStatus();
  // 同步配置与状态独立于 LogStatus，单独拉一次
  void loadSyncConfig();
  // 备份配置与状态同理
  void loadBackupConfig();

  // 调试用：stats.html?page=stats 直接打开某一页
  const wanted = new URLSearchParams(location.search).get('page');
  const pages = ['stats', 'settings', 'trail', 'categories', 'app-categories'];
  if (wanted && pages.includes(wanted)) switchPage(wanted);

  // 菜单里改了开关，统计窗口跟着变
  if (HAS_TAURI) {
    void listen('kuro-settings', () => {
      void refreshStatus();
      void refreshSyncStatus();
    });
    void listen('kuro-input-log-cleaned', () => {
      void refreshStatus();
    });
  }

  // 仪表盘 30 秒自动刷新（只在可见时）
  window.setInterval(() => {
    if (document.visibilityState === 'visible' && document.getElementById('page-dashboard')?.classList.contains('active')) {
      void refreshStatus();
      void refreshSyncStatus();
    }
  }, 30_000);
}

void boot();
