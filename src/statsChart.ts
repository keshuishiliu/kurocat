/**
 * 手写 SVG 图表。刻意不引入图表库：
 * 这个窗口只在 Tauri webview 里跑，为了三张图拉一个 200 KB 的依赖不值当，
 * 而且手写后颜色与线宽可以完全按 KuroCat 的配色走。
 */

const C = {
  accent: '#7ea6ff',
  accentDim: 'rgba(126, 166, 255, 0.30)',
  accentFaint: 'rgba(126, 166, 255, 0.10)',
  accent2: '#6ee7c8',
  grid: 'rgba(255, 255, 255, 0.07)',
  axis: '#7c818d',
  label: '#8b909c',
};

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
  const rows = data.filter((d) => Number.isFinite(d.value));
  if (!rows.length) {
    empty(host);
    return;
  }
  const W = 620;
  const H = opts.height ?? 168;
  const padL = 44;
  const padR = 10;
  const padT = 12;
  const padB = 22;
  const iw = W - padL - padR;
  const ih = H - padT - padB;
  const max = Math.max(1, ...rows.map((d) => d.value));
  const slot = iw / rows.length;
  const barW = Math.max(1.5, Math.min(26, slot * 0.62));
  const maxLabels = opts.maxLabels ?? 8;
  const labelStep = Math.max(1, Math.ceil(rows.length / maxLabels));

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
      `<text x="${padL - 6}" y="${(y + 3).toFixed(1)}" fill="${C.axis}" font-size="9" text-anchor="end">${esc(fmtNum(val))}</text>`,
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
      `<text x="${x.toFixed(1)}" y="${H - 6}" fill="${C.label}" font-size="9" text-anchor="middle">${esc(d.label)}</text>`,
    );
  });

  parts.push('</svg>');
  host.innerHTML = parts.join('');
}

/** 折线 + 面积填充。适合看长区间趋势形态。 */
export function renderLine(
  host: HTMLElement,
  data: Point[],
  opts: { height?: number; maxLabels?: number } = {},
): void {
  const rows = data.filter((d) => Number.isFinite(d.value));
  if (rows.length < 2) {
    renderBars(host, rows, opts);
    return;
  }
  const W = 620;
  const H = opts.height ?? 190;
  const padL = 44;
  const padR = 12;
  const padT = 14;
  const padB = 22;
  const iw = W - padL - padR;
  const ih = H - padT - padB;
  const max = Math.max(1, ...rows.map((d) => d.value));
  const stepX = iw / (rows.length - 1);
  const yOf = (v: number) => padT + ih - (v / max) * ih;
  const maxLabels = opts.maxLabels ?? 8;
  const labelStep = Math.max(1, Math.ceil(rows.length / maxLabels));

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
      `<text x="${padL - 6}" y="${(y + 3).toFixed(1)}" fill="${C.axis}" font-size="9" text-anchor="end">${esc(fmtNum(val))}</text>`,
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
      `<text x="${(padL + stepX * i).toFixed(1)}" y="${H - 6}" fill="${C.label}" font-size="9" text-anchor="middle">${esc(d.label)}</text>`,
    );
  });

  parts.push('</svg>');
  host.innerHTML = parts.join('');
}

/** 横向条。适合按程序 / 按键排行。 */
export function renderHBars(
  host: HTMLElement,
  data: Point[],
  opts: { labelWidth?: number; rowHeight?: number } = {},
): void {
  const rows = data.filter((d) => Number.isFinite(d.value) && d.value > 0);
  if (!rows.length) {
    empty(host);
    return;
  }
  const W = 620;
  const rowH = opts.rowHeight ?? 22;
  const labelW = opts.labelWidth ?? 130;
  const valueW = 62;
  const H = rows.length * rowH + 6;
  const barMax = W - labelW - valueW - 10;
  const max = Math.max(1, ...rows.map((d) => d.value));

  const parts: string[] = [];
  parts.push(`<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" role="img">`);

  rows.forEach((d, i) => {
    const y = 3 + i * rowH;
    const barH = Math.min(12, rowH - 8);
    const w = Math.max(1.5, (d.value / max) * barMax);
    parts.push(
      `<text x="0" y="${(y + barH - 1).toFixed(1)}" fill="${C.label}" font-size="10">${esc(clip(d.label, 20))}</text>`,
    );
    parts.push(
      `<rect x="${labelW}" y="${y.toFixed(1)}" width="${barMax}" height="${barH}" rx="3" fill="rgba(255,255,255,0.035)"/>`,
    );
    parts.push(
      `<rect x="${labelW}" y="${y.toFixed(1)}" width="${w.toFixed(1)}" height="${barH}" rx="3" fill="${i === 0 ? C.accent : C.accentDim}"/>`,
    );
    parts.push(
      `<text x="${W}" y="${(y + barH - 1).toFixed(1)}" fill="${C.label}" font-size="10" text-anchor="end">${esc(fmtNum(d.value))}</text>`,
    );
  });

  parts.push('</svg>');
  host.innerHTML = parts.join('');
}

function clip(s: string, max: number): string {
  return s.length <= max ? s : `${s.slice(0, max - 1)}…`;
}
