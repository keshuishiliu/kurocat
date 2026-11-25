/**
 * 手写 SVG 图表。刻意不引入图表库：
 * 这个窗口只在 Tauri webview 里跑，为了三张图拉一个 200 KB 的依赖不值当，
 * 而且手写后颜色与线宽可以完全按 KuroCat 的配色走。
 */

interface Palette {
  accent: string;
  accentDim: string;
  accentFaint: string;
  accent2: string;
  grid: string;
  axis: string;
  label: string;
  track: string;
}

const DARK: Palette = {
  accent: '#7ea6ff',
  accentDim: 'rgba(126, 166, 255, 0.30)',
  accentFaint: 'rgba(126, 166, 255, 0.10)',
  accent2: '#6ee7c8',
  grid: 'rgba(255, 255, 255, 0.07)',
  axis: '#7c818d',
  label: '#8b909c',
  track: 'rgba(255, 255, 255, 0.035)',
};

const LIGHT: Palette = {
  accent: '#3b6ef0',
  accentDim: 'rgba(59, 110, 240, 0.32)',
  accentFaint: 'rgba(59, 110, 240, 0.12)',
  accent2: '#0f9d78',
  grid: 'rgba(16, 24, 40, 0.08)',
  axis: '#98a0ad',
  label: '#6b7280',
  track: 'rgba(16, 24, 40, 0.06)',
};

/** 图表不读 CSS 变量（SVG 里 var() 在旧 WebView 上不可靠），换主题时由调用方显式切换。 */
let C: Palette = DARK;

export function setChartTheme(theme: 'dark' | 'light'): void {
  C = theme === 'light' ? LIGHT : DARK;
}

/** 图内文字统一字号。viewBox 宽度取容器真实像素宽，所以这 11px 就是屏幕上的 11px。 */
const FONT = 11;
/** 容器窄到这个程度就不画了，留空态；也避免 clientWidth 取到 0 时算出退化比例。 */
const MIN_W = 200;
const FALLBACK_W = 620;

/**
 * 取容器真实像素宽当 viewBox 宽度。
 * 关键点：CSS 是 width:100%，如果 viewBox 还写死 620，整张图（含文字）会被等比压缩——
 * 半宽卡片里 334/620 = 0.54，9px 的字实际只画出来 4.9px。1 单位 = 1 像素才对。
 */
function measure(host: HTMLElement): number {
  const w = host.clientWidth;
  return w >= MIN_W ? Math.round(w) : FALLBACK_W;
}

/** 记住每个图表的最后一次渲染，窗口改尺寸时原地重画，不用重新查库。 */
const replayers = new Map<HTMLElement, () => void>();

export function redrawAllCharts(): void {
  replayers.forEach((fn) => fn());
}

export interface Point {
  label: string;
  value: number;
}

function esc(s: string): string {
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

export function fmtNum(n: number): string {
  if (!Number.isFinite(n)) return '—';
  const abs = Math.abs(n);
  if (abs >= 100_000_000) return `${(n / 100_000_000).toFixed(1).replace(/\.0$/, '')} 亿`;
  if (abs >= 10_000) return `${(n / 10_000).toFixed(1).replace(/\.0$/, '')} 万`;
  return n.toLocaleString('zh-CN');
}

export function fmtDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds <= 0) return '0 分钟';
  const totalMinutes = Math.round(seconds / 60);
  if (totalMinutes < 1) return '不到 1 分钟';
  const h = Math.floor(totalMinutes / 60);
  const m = totalMinutes % 60;
  if (h > 0) return m > 0 ? `${h} 小时 ${m} 分` : `${h} 小时`;
  return `${m} 分钟`;
}

export function fmtBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i++;
  }
  return `${v >= 100 || i === 0 ? Math.round(v) : v.toFixed(1)} ${units[i]}`;
}

function empty(host: HTMLElement, message = '还没有数据'): void {
  host.innerHTML = `<div class="chart-empty">${esc(message)}</div>`;
}

/** 竖直柱状图。适合逐日趋势。 */
export function renderBars(
  host: HTMLElement,
  data: Point[],
  opts: { height?: number; maxLabels?: number } = {},
): void {
  replayers.set(host, () => renderBars(host, data, opts));
  const rows = data.filter((d) => Number.isFinite(d.value));
  if (!rows.length) {
    empty(host);
    return;
  }
  const W = measure(host);
  const H = opts.height ?? 168;
  const padL = 48;
  const padR = 10;
  const padT = 12;
  const padB = 24;
  const iw = W - padL - padR;
  const ih = H - padT - padB;
  const max = Math.max(1, ...rows.map((d) => d.value));
  const slot = iw / rows.length;
  const barW = Math.max(2, Math.min(26, slot * 0.62));
  const labelStep = stepFor(rows, slot, opts.maxLabels ?? 8);

  const parts: string[] = [];
  parts.push(`<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img">`);

  // 网格与 y 轴刻度
  for (let i = 0; i <= 4; i++) {
    const y = padT + (ih * i) / 4;
    const val = Math.round((max * (4 - i)) / 4);
    parts.push(
      `<line x1="${padL}" y1="${y.toFixed(1)}" x2="${W - padR}" y2="${y.toFixed(1)}" stroke="${C.grid}" stroke-width="1"/>`,
    );
    parts.push(
      `<text x="${padL - 7}" y="${(y + 4).toFixed(1)}" fill="${C.axis}" font-size="${FONT}" text-anchor="end">${esc(fmtNum(val))}</text>`,
    );
  }

  // 柱
  rows.forEach((d, i) => {
    const h = max > 0 ? (d.value / max) * ih : 0;
    const x = padL + slot * i + (slot - barW) / 2;
    const y = padT + ih - h;
    if (h > 0.2) {
      parts.push(
        `<rect x="${x.toFixed(1)}" y="${y.toFixed(1)}" width="${barW.toFixed(1)}" height="${h.toFixed(1)}" rx="${Math.min(2, h / 2).toFixed(1)}" fill="${C.accentDim}"/>`,
      );
    }
  });

  // x 轴标签（抽稀，首尾必留）
  rows.forEach((d, i) => {
    const last = rows.length - 1;
    const isEdge = i === 0 || i === last;
    // 距末尾不足一个 step 时不再打标签，否则会和最后一个标签挤在一起
    if (!isEdge && (i % labelStep !== 0 || last - i < labelStep)) return;
    const x = padL + slot * i + slot / 2;
    parts.push(
      `<text x="${x.toFixed(1)}" y="${H - 7}" fill="${C.label}" font-size="${FONT}" text-anchor="middle">${esc(d.label)}</text>`,
    );
  });

  parts.push('</svg>');
  host.innerHTML = parts.join('');
}

/**
 * x 轴标签抽稀步长。
 *
 * 只按「行数 / maxLabels」算的话，窄卡片里标签必然糊成一片——24 个小时桶挤在 334px 里，
 * 每格只有 13px，却还想画 12 个标签。所以同时按像素兜底：先估算一个标签要占多宽，
 * 再取「像素需要的步长」和「数量上限」里更保守的那个。
 */
function stepFor(rows: Point[], slot: number, maxLabels: number): number {
  const longest = rows.reduce((m, d) => Math.max(m, d.label.length), 0);
  const minLabelPx = longest * FONT * 0.62 + 6;
  const byWidth = Math.ceil(minLabelPx / Math.max(1, slot));
  return Math.max(1, byWidth, Math.ceil(rows.length / Math.max(1, maxLabels)));
}

/** 折线 + 面积填充。适合看长区间趋势形态。 */
export function renderLine(
  host: HTMLElement,
  data: Point[],
  opts: { height?: number; maxLabels?: number } = {},
): void {
  replayers.set(host, () => renderLine(host, data, opts));
  const rows = data.filter((d) => Number.isFinite(d.value));
  if (rows.length < 2) {
    renderBars(host, rows, opts);
    return;
  }
  const W = measure(host);
  const H = opts.height ?? 190;
  const padL = 48;
  const padR = 12;
  const padT = 14;
  const padB = 24;
  const iw = W - padL - padR;
  const ih = H - padT - padB;
  const max = Math.max(1, ...rows.map((d) => d.value));
  const stepX = iw / (rows.length - 1);
  const yOf = (v: number) => padT + ih - (v / max) * ih;
  const labelStep = stepFor(rows, stepX, opts.maxLabels ?? 8);

  const pts = rows.map((d, i) => `${(padL + stepX * i).toFixed(1)},${yOf(d.value).toFixed(1)}`);
  const area = `M ${padL},${padT + ih} L ${pts.join(' L ')} L ${(padL + iw).toFixed(1)},${padT + ih} Z`;

  const parts: string[] = [];
  parts.push(`<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img">`);
  parts.push(
    `<defs><linearGradient id="fill-grad" x1="0" y1="0" x2="0" y2="1"><stop offset="0%" stop-color="${C.accent}" stop-opacity="0.22"/><stop offset="100%" stop-color="${C.accent}" stop-opacity="0"/></linearGradient></defs>`,
  );

  for (let i = 0; i <= 4; i++) {
    const y = padT + (ih * i) / 4;
    const val = Math.round((max * (4 - i)) / 4);
    parts.push(
      `<line x1="${padL}" y1="${y.toFixed(1)}" x2="${W - padR}" y2="${y.toFixed(1)}" stroke="${C.grid}" stroke-width="1"/>`,
    );
    parts.push(
      `<text x="${padL - 7}" y="${(y + 4).toFixed(1)}" fill="${C.axis}" font-size="${FONT}" text-anchor="end">${esc(fmtNum(val))}</text>`,
    );
  }

  parts.push(`<path d="${area}" fill="url(#fill-grad)"/>`);
  parts.push(
    `<polyline points="${pts.join(' ')}" fill="none" stroke="${C.accent}" stroke-width="1.6" stroke-linejoin="round" stroke-linecap="round"/>`,
  );

  rows.forEach((d, i) => {
    const last = rows.length - 1;
    const isEdge = i === 0 || i === last;
    // 距末尾不足一个 step 时不再打标签，否则会和最后一个标签挤在一起
    if (!isEdge && (i % labelStep !== 0 || last - i < labelStep)) return;
    parts.push(
      `<text x="${(padL + stepX * i).toFixed(1)}" y="${H - 7}" fill="${C.label}" font-size="${FONT}" text-anchor="middle">${esc(d.label)}</text>`,
    );
  });

  parts.push('</svg>');
  host.innerHTML = parts.join('');
}

/** 横向条。适合按程序 / 按键排行。`colors` 可按行着色（按分类统计时用分类色）。 */
export function renderHBars(
  host: HTMLElement,
  data: Point[],
  opts: {
    labelWidth?: number;
    rowHeight?: number;
    colors?: string[];
    /** 每行的图标 data URL（与 data 下标对应），null/undefined 该行不画图标。 */
    icons?: (string | null)[];
  } = {},
): void {
  replayers.set(host, () => renderHBars(host, data, opts));
  const rows = data.filter((d) => Number.isFinite(d.value) && d.value > 0);
  if (!rows.length) {
    empty(host);
    return;
  }
  const W = measure(host);
  const rowH = opts.rowHeight ?? 26;
  const barH = Math.min(12, rowH - 10);
  const valueW = 56;
  // 标签区最多吃掉 48% 宽度，剩下的留给条本身，否则窄卡片里条会只剩一小截
  const labelW = Math.max(72, Math.min(opts.labelWidth ?? 130, Math.round(W * 0.48)));
  const H = rows.length * rowH + 8;
  const barMax = Math.max(40, W - labelW - valueW - 10);
  const max = Math.max(1, ...rows.map((d) => d.value));
  const maxChars = Math.max(6, Math.floor(labelW / (FONT * 0.8)));
  // 带图标的行文字要给图标让位，可用宽度缩短
  const ICON = 16;
  const ICON_GAP = 5;
  const maxCharsIcon = Math.max(4, Math.floor((labelW - ICON - ICON_GAP) / (FONT * 0.8)));

  const parts: string[] = [];
  parts.push(`<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img">`);

  rows.forEach((d, i) => {
    const y = 4 + i * rowH;
    // 文字与条在行内垂直居中，11px 的基线偏移约 4px
    const barY = y + (rowH - barH) / 2;
    const ty = barY + barH / 2 + 4;
    const w = Math.max(2, (d.value / max) * barMax);
    const color = opts.colors?.[i] ?? (i === 0 ? C.accent : C.accentDim);
    const icon = opts.icons?.[i] ?? null;
    if (icon) {
      parts.push(
        `<image x="0" y="${(ty - 12).toFixed(1)}" width="${ICON}" height="${ICON}" href="${esc(icon)}"/>`,
      );
    }
    const tx = icon ? ICON + ICON_GAP : 0;
    const mc = icon ? maxCharsIcon : maxChars;
    parts.push(
      `<text x="${tx}" y="${ty.toFixed(1)}" fill="${C.label}" font-size="${FONT}">${esc(clip(d.label, mc))}</text>`,
    );
    parts.push(
      `<rect x="${labelW}" y="${barY.toFixed(1)}" width="${barMax}" height="${barH}" rx="3" fill="${C.track}"/>`,
    );
    parts.push(
      `<rect x="${labelW}" y="${barY.toFixed(1)}" width="${w.toFixed(1)}" height="${barH}" rx="3" fill="${color}"/>`,
    );
    parts.push(
      `<text x="${W}" y="${ty.toFixed(1)}" fill="${C.label}" font-size="${FONT}" text-anchor="end">${esc(fmtNum(d.value))}</text>`,
    );
  });

  parts.push('</svg>');
  host.innerHTML = parts.join('');
}

function clip(s: string, max: number): string {
  return s.length <= max ? s : `${s.slice(0, max - 1)}…`;
}
