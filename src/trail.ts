/**
 * 鼠标轨迹播放器（Canvas 2D，无依赖）。
 *
 * 设计取舍：
 * - 时间是「压缩后」的：相邻两点间隔超过 2 秒时，只按 2 秒推进。
 *   否则看一段 8 小时的历史，绝大部分时间都在等用户发呆。
 * - 只画三层：整段淡色路线（预览）、已播放路径（实色）、最近 160 段的高亮尾迹 + 光标。
 *   一次性画 2 万个点每帧会掉帧，分层画能让 60fps 稳住。
 * - 颜色全部来自 CSS 变量，换主题只需要 refreshTheme()。
 */

export interface TrailPoint {
  t: number;
  x: number;
  y: number;
  /**
   * 采样时前台应用所属的分类 id（v5.2.1 起才有，老数据为 null）。
   * null = 未归类 / 锁屏 / 隐私窗口期间采的点，回放时显示灰色。
   */
  cat?: string | null;
}

export interface TrailTick {
  timeMs: number;
  progress: number;
  playing: boolean;
}

/** 空闲压缩上限：两点间隔最多按这么长的虚拟时间推进。 */
const MAX_GAP_MS = 2000;
/**
 * 断线阈值：真实间隔超过它就不连线。
 *
 * 采样是 10 Hz，正常两点间隔 100 毫秒左右。中间断了几十秒，说明用户停下来了，
 * 下一个点可能是屏幕另一头——照连就是一条横穿整个屏幕的假轨迹。宁可断开。
 */
const BREAK_MS = 3000;
const TAIL_SEGMENTS = 160;

interface Palette {
  bg: string;
  grid: string;
  route: string;
  accent: string;
  accentSoft: string;
  cursor: string;
  text: string;
}

function readVar(name: string, fallback: string): string {
  const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  return v || fallback;
}

function currentPalette(): Palette {
  return {
    bg: readVar('--trail-bg', '#101217'),
    grid: readVar('--trail-grid', 'rgba(255,255,255,0.06)'),
    route: readVar('--trail-route', 'rgba(126,166,255,0.16)'),
    accent: readVar('--accent', '#7ea6ff'),
    accentSoft: readVar('--accent-soft', 'rgba(126,166,255,0.14)'),
    cursor: readVar('--accent', '#7ea6ff'),
    text: readVar('--muted', '#8b909c'),
  };
}

export class TrailPlayer {
  private readonly canvas: HTMLCanvasElement;
  private readonly ctx: CanvasRenderingContext2D;
  private points: TrailPoint[] = [];
  /** 压缩后的时间轴，与 points 一一对应，vtimes[0] === 0 */
  private vtimes: number[] = [];
  /** breaks[i] = true 表示第 i 点与第 i-1 点之间不连线（中间空了太久） */
  private breaks: boolean[] = [];
  private total = 0;
  private cursorMs = 0;
  private speed = 8;
  private playing = false;
  private raf = 0;
  private lastFrame = 0;
  private palette: Palette = currentPalette();
  private bounds = { minX: 0, maxX: 1, minY: 0, maxY: 1 };
  private resizeObserver: ResizeObserver | null = null;
  /**
   * 分类 → 颜色。参数是采样点的 cat 字段（可能为 null/undefined）。
   * statsMain 注入；不注入时维持原来的单色 accent 行为。
   * 每次绘制时才调用，分类改名/换色立即生效，不需要重载数据。
   */
  private colorFor: ((cat: string | null | undefined) => string) | null = null;

  constructor(
    canvas: HTMLCanvasElement,
    private readonly onTick?: (info: TrailTick) => void,
  ) {
    this.canvas = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('canvas 2d 不可用');
    this.ctx = ctx;
    this.palette = currentPalette();
    if (typeof ResizeObserver !== 'undefined') {
      this.resizeObserver = new ResizeObserver(() => this.draw());
      this.resizeObserver.observe(canvas);
    } else {
      window.addEventListener('resize', () => this.draw());
    }
    this.draw();
  }

  get pointCount(): number {
    return this.points.length;
  }

  /** 注入分类着色函数。传入 null 恢复单色。 */
  setCategoryColors(fn: ((cat: string | null | undefined) => string) | null): void {
    this.colorFor = fn;
    if (!this.playing) this.draw();
  }

  get isPlaying(): boolean {
    return this.playing;
  }

  get speedValue(): number {
    return this.speed;
  }

  setPoints(points: TrailPoint[]): void {
    this.stop();
    this.points = points
      .filter((p) => Number.isFinite(p.t) && Number.isFinite(p.x) && Number.isFinite(p.y))
      .sort((a, b) => a.t - b.t);
    this.vtimes = new Array(this.points.length);
    this.breaks = new Array(this.points.length).fill(false);
    let virtual = 0;
    let minX = Infinity;
    let maxX = -Infinity;
    let minY = Infinity;
    let maxY = -Infinity;
    for (let i = 0; i < this.points.length; i++) {
      const p = this.points[i];
      if (i > 0) {
        const realGap = Math.max(0, p.t - this.points[i - 1].t);
        virtual += Math.min(MAX_GAP_MS, realGap);
        if (realGap > BREAK_MS) this.breaks[i] = true;
      }
      this.vtimes[i] = virtual;
      if (p.x < minX) minX = p.x;
      if (p.x > maxX) maxX = p.x;
      if (p.y < minY) minY = p.y;
      if (p.y > maxY) maxY = p.y;
    }
    this.total = virtual;
    if (!Number.isFinite(minX)) {
      this.bounds = { minX: 0, maxX: 1, minY: 0, maxY: 1 };
    } else {
      // 单点或一条直线时给一点厚度，避免除零后画出屏幕
      const padX = Math.max(24, (maxX - minX) * 0.04);
      const padY = Math.max(24, (maxY - minY) * 0.04);
      this.bounds = {
        minX: minX - padX,
        maxX: maxX + padX,
        minY: minY - padY,
        maxY: maxY + padY,
      };
    }
    this.cursorMs = 0;
    this.draw();
    this.emitTick();
  }

  setSpeed(speed: number): void {
    this.speed = Math.min(240, Math.max(0.25, speed));
  }

  refreshTheme(): void {
    this.palette = currentPalette();
    this.draw();
  }

  play(): void {
    if (this.points.length < 2) return;
    if (this.cursorMs >= this.total) this.cursorMs = 0;
    if (this.playing) return;
    this.playing = true;
    this.lastFrame = 0;
    this.raf = requestAnimationFrame(this.tick);
    this.emitTick();
  }

  pause(): void {
    if (!this.playing) return;
    this.playing = false;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    this.emitTick();
  }

  stop(): void {
    this.pause();
    this.cursorMs = 0;
    this.draw();
    this.emitTick();
  }

  /** fraction: 0..1 */
  seek(fraction: number): void {
    const f = Math.min(1, Math.max(0, fraction));
    this.cursorMs = this.total * f;
    this.draw();
    this.emitTick();
  }

  get progress(): number {
    return this.total > 0 ? this.cursorMs / this.total : 0;
  }

  private tick = (now: number): void => {
    if (!this.playing) return;
    const dt = this.lastFrame ? Math.min(64, now - this.lastFrame) : 16;
    this.lastFrame = now;
    this.cursorMs = Math.min(this.total, this.cursorMs + dt * this.speed);
    this.draw();
    this.emitTick();
    if (this.cursorMs >= this.total) {
      this.playing = false;
      this.raf = 0;
      return;
    }
    this.raf = requestAnimationFrame(this.tick);
  };

  private emitTick(): void {
    if (!this.onTick) return;
    this.onTick({
      timeMs: this.timeAt(this.cursorMs),
      progress: this.progress,
      playing: this.playing,
    });
  }

  /** 虚拟时间 → 真实时间戳（取所在区间的起点，足够精确到秒）。 */
  private timeAt(virtual: number): number {
    const idx = this.indexAt(virtual);
    return this.points[Math.min(idx, this.points.length - 1)]?.t ?? 0;
  }

  private indexAt(virtual: number): number {
    if (!this.vtimes.length) return 0;
    let lo = 0;
    let hi = this.vtimes.length - 1;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (this.vtimes[mid] < virtual) lo = mid + 1;
      else hi = mid;
    }
    return lo;
  }

  private project(w: number, h: number) {
    const pad = 18;
    const sw = Math.max(1, w - pad * 2);
    const sh = Math.max(1, h - pad * 2);
    const bw = Math.max(1, this.bounds.maxX - this.bounds.minX);
    const bh = Math.max(1, this.bounds.maxY - this.bounds.minY);
    const scale = Math.min(sw / bw, sh / bh);
    const ox = pad + (sw - bw * scale) / 2 - this.bounds.minX * scale;
    const oy = pad + (sh - bh * scale) / 2 - this.bounds.minY * scale;
    return {
      x: (v: number) => ox + v * scale,
      y: (v: number) => oy + v * scale,
      scale,
    };
  }

  /** 沿 points[from..=to] 描一条折线，遇到 breaks 处断开（不起笔，直接 moveTo）。 */
  private tracePath(
    from: number,
    to: number,
    proj: { x: (v: number) => number; y: (v: number) => number },
  ): void {
    const ctx = this.ctx;
    ctx.beginPath();
    let started = false;
    for (let i = from; i <= to; i++) {
      const p = this.points[i];
      if (!started || this.breaks[i]) {
        ctx.moveTo(proj.x(p.x), proj.y(p.y));
        started = true;
      } else {
        ctx.lineTo(proj.x(p.x), proj.y(p.y));
      }
    }
  }

  /** 两个采样点是否属于同一分类（null 视为同一类，段间不因它断开）。 */
  private sameCat(a: number, b: number): boolean {
    return (this.points[a].cat ?? null) === (this.points[b].cat ?? null);
  }

  /**
   * 分类着色的描线：按 cat 变化 / breaks 把 [from..to] 切成小段，
   * 每段用段首点的分类颜色单独 stroke。未注入 colorFor 时退化为单色。
   */
  private strokeColoredPath(
    from: number,
    to: number,
    proj: { x: (v: number) => number; y: (v: number) => number },
    alpha: number,
    width: number,
  ): void {
    const ctx = this.ctx;
    if (!this.colorFor) {
      ctx.beginPath();
      this.tracePath(from, to, proj);
      ctx.globalAlpha = alpha;
      ctx.lineWidth = width;
      ctx.strokeStyle = this.palette.accent;
      ctx.stroke();
      ctx.globalAlpha = 1;
      return;
    }
    ctx.lineJoin = 'round';
    let segStart = from;
    const flush = (a: number, b: number): void => {
      if (b <= a) return;
      ctx.beginPath();
      this.tracePath(a, b, proj);
      ctx.globalAlpha = alpha;
      ctx.lineWidth = width;
      ctx.strokeStyle = this.colorFor!(this.points[a].cat);
      ctx.stroke();
    };
    for (let i = from + 1; i <= to; i++) {
      if (this.breaks[i] || !this.sameCat(i - 1, i)) {
        flush(segStart, i - 1);
        segStart = i;
      }
    }
    flush(segStart, to);
    ctx.globalAlpha = 1;
  }

  private draw(): void {
    const canvas = this.canvas;
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth || 640;
    const h = canvas.clientHeight || 360;
    if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
      canvas.width = Math.round(w * dpr);
      canvas.height = Math.round(h * dpr);
    }
    const ctx = this.ctx;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);

    // 背景 + 网格
    ctx.fillStyle = this.palette.bg;
    ctx.fillRect(0, 0, w, h);
    ctx.strokeStyle = this.palette.grid;
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let i = 1; i < 8; i++) {
      const x = (w * i) / 8;
      ctx.moveTo(x, 0);
      ctx.lineTo(x, h);
    }
    for (let i = 1; i < 5; i++) {
      const y = (h * i) / 5;
      ctx.moveTo(0, y);
      ctx.lineTo(w, y);
    }
    ctx.stroke();

    if (this.points.length === 0) {
      ctx.fillStyle = this.palette.text;
      ctx.font = '12px "Segoe UI", system-ui, sans-serif';
      ctx.textAlign = 'center';
      ctx.fillText('这段时间没有轨迹采样', w / 2, h / 2);
      return;
    }

    const proj = this.project(w, h);

    // 1) 整段淡色路线，让用户先看到形状
    if (this.points.length > 1) {
      ctx.strokeStyle = this.palette.route;
      ctx.lineWidth = 1;
      this.tracePath(0, this.points.length - 1, proj);
      ctx.stroke();
    }

    // 2) 已播放部分（分类着色：每段用该段采样时前台应用的分类色）
    const head = this.indexAt(this.cursorMs);
    if (head > 0) {
      ctx.globalAlpha = 0.45;
      this.strokeColoredPath(0, head, proj, 0.45, 1.2);
      ctx.globalAlpha = 1;
    }

    // 3) 尾迹：分段递增透明度，比逐段渐变便宜得多；颜色跟随块首的分类
    const tailStart = Math.max(1, head - TAIL_SEGMENTS);
    const chunks = 16;
    const per = Math.max(1, Math.ceil((head - tailStart) / chunks));
    for (let c = 0; c < chunks; c++) {
      const from = tailStart + c * per;
      const to = Math.min(head, from + per);
      if (to <= from) break;
      const alpha = 0.06 + (0.9 * (c + 1)) / chunks;
      ctx.strokeStyle = this.colorFor
        ? this.colorFor(this.points[Math.max(0, from - 1)].cat)
        : this.palette.accent;
      ctx.globalAlpha = alpha;
      ctx.lineWidth = 1.4 + (1.4 * (c + 1)) / chunks;
      ctx.lineCap = 'round';
      this.tracePath(from - 1, to, proj);
      ctx.stroke();
    }
    ctx.globalAlpha = 1;

    // 4) 光标
    const cur = this.points[Math.min(head, this.points.length - 1)];
    const cx = proj.x(cur.x);
    const cy = proj.y(cur.y);
    ctx.beginPath();
    ctx.fillStyle = this.palette.accentSoft;
    ctx.arc(cx, cy, 12, 0, Math.PI * 2);
    ctx.fill();
    ctx.beginPath();
    ctx.fillStyle = this.palette.cursor;
    ctx.arc(cx, cy, 4.2, 0, Math.PI * 2);
    ctx.fill();
    ctx.beginPath();
    ctx.strokeStyle = this.palette.cursor;
    ctx.globalAlpha = 0.5;
    ctx.lineWidth = 1;
    ctx.arc(cx, cy, 8, 0, Math.PI * 2);
    ctx.stroke();
    ctx.globalAlpha = 1;

    // 5) 左下角比例尺提示（1 屏宽约等于多少像素）
    ctx.fillStyle = this.palette.text;
    ctx.font = '10px "Cascadia Mono", Consolas, monospace';
    ctx.textAlign = 'left';
    ctx.fillText(`${this.points.length} 个采样点 · 缩放 1:${(1 / proj.scale).toFixed(1)}`, 10, h - 10);
  }
}
