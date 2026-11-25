// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Dependency-free audio VU meter widget with real ballistics.
 *
 * The module is split into pure helpers (`dbToLinear`, `linearToDb`,
 * `normalizeVuConfig`, `createVuState`, `stepVuState`, `vuGeometry`) and DOM
 * renderers (`renderVuSVG`, `renderVu`, `updateVu`).  Level smoothing is
 * asymmetric, release/falloff is measured in dB per second, RMS is calculated
 * over a bounded recent-linear-sample window, and peaks are held and then
 * decayed in dB per second.
 *
 * Samples are dBFS values by default (`-60..0` dB).  Set
 * `input: 'linear'` (or `linear: true` on a definition node) when the source
 * publishes linear `0..1` amplitudes instead.
 */

const SVG_NS = 'http://www.w3.org/2000/svg';
const EPSILON = 1e-9;
const MAX_RMS_SAMPLES = 512;
const MAX_STEP_MS = 60_000;
const MAX_SEGMENTS = 128;

const DEFAULT_COLORS = Object.freeze({
  normal: '#5b6fff',
  warn: '#f0b94a',
  crit: '#f26464',
  track: 'rgba(255, 255, 255, 0.12)',
  text: '#e5e7eb',
  muted: '#8d9bb3',
  peak: '#e5e7eb',
  clip: '#f26464',
});

/* ------------------------------------------------------------------------- */
/* Small helpers                                                              */
/* ------------------------------------------------------------------------- */

export function clamp(value, min, max) {
  const lower = Number.isFinite(Number(min)) ? Number(min) : 0;
  const upper = Number.isFinite(Number(max)) ? Number(max) : 100;
  const number = Number(value);
  if (!Number.isFinite(number)) return lower;
  if (upper === lower) return lower;
  const lo = Math.min(lower, upper);
  const hi = Math.max(lower, upper);
  return Math.max(lo, Math.min(hi, number));
}

function clampInteger(value, min, max, fallback) {
  const number = Number(value);
  if (!Number.isFinite(number)) return fallback;
  return Math.max(min, Math.min(max, Math.round(number)));
}

function finite(value, fallback) {
  if (value === undefined || value === null || value === '') return fallback;
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
}

function nonNegative(value, fallback) {
  const number = finite(value, fallback);
  return number < 0 ? fallback : number;
}

function positive(value, fallback) {
  if (value === undefined || value === null || value === '') return fallback;
  const number = Number(value);
  return Number.isFinite(number) && number > 0 ? number : fallback;
}

function firstDefined(...values) {
  for (const value of values) {
    if (value !== undefined && value !== null && value !== '') return value;
  }
  return undefined;
}

function numberText(value) {
  const rounded = Math.round(Number(value) * 1000) / 1000;
  return String(Object.is(rounded, -0) ? 0 : rounded);
}

function escapeXml(value) {
  return String(value ?? '')
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function isPlainObject(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function isTokenObject(value) {
  return isPlainObject(value) && (
    Object.prototype.hasOwnProperty.call(value, '$value')
    || Object.prototype.hasOwnProperty.call(value, 'value')
  );
}

function tokenSource(tokens) {
  if (!tokens || typeof tokens !== 'object') return {};
  if (
    tokens.tokens
    && typeof tokens.tokens === 'object'
    && (tokens.schema || tokens.canvas || tokens.id)
  ) {
    return tokens.tokens;
  }
  return tokens;
}

function rawToken(tokens, key) {
  const source = tokenSource(tokens);
  if (!source || typeof source !== 'object') return { found: false };
  if (Object.prototype.hasOwnProperty.call(source, key)) {
    return { found: true, value: source[key] };
  }
  let current = source;
  for (const part of String(key).split('.')) {
    if (!isPlainObject(current) || !Object.prototype.hasOwnProperty.call(current, part)) {
      return { found: false };
    }
    current = current[part];
  }
  return { found: true, value: current };
}

function unwrap(value) {
  if (isTokenObject(value)) {
    return value.$value !== undefined ? value.$value : value.value;
  }
  return value;
}

function resolveString(text, tokens, depth = 0) {
  if (text === undefined || text === null) return undefined;
  const source = String(text);
  if (!source.includes('{') || depth > 8) return source;
  return source.replace(/\{([^{}]+)\}/g, (match, body) => {
    const resolved = tokenValue(tokens, [body.trim()], undefined, depth + 1);
    return resolved === undefined ? match : String(resolved);
  });
}

function tokenValue(tokens, names, fallback, depth = 0) {
  for (const name of names) {
    const found = rawToken(tokens, name);
    if (!found.found) continue;
    const value = unwrap(found.value);
    if (value === undefined || value === null) continue;
    return typeof value === 'string'
      ? resolveString(value, tokens, depth)
      : value;
  }
  return fallback;
}

function tokenString(tokens, names, fallback) {
  const value = tokenValue(tokens, names, fallback);
  if (Array.isArray(value)) return value.filter(Boolean).join(', ') || fallback;
  return value === undefined || value === null ? fallback : String(value);
}

function resolveColour(value, tokens) {
  if (value === undefined || value === null || value === '') return undefined;
  if (typeof value === 'string') return String(resolveString(value, tokens));
  return String(value);
}

/* ------------------------------------------------------------------------- */
/* Decibel conversions                                                        */
/* ------------------------------------------------------------------------- */

/**
 * Convert dBFS to linear amplitude.  The optional floor/ceiling clamp the
 * input before conversion.  `-Infinity` is true silence and becomes 0; NaN is
 * treated as silence as well.  `Infinity` becomes the ceiling amplitude.
 */
export function dbToLinear(db, floor = -Infinity, ceiling = Infinity) {
  let low = Number(floor);
  let high = Number(ceiling);
  if (Number.isNaN(low)) low = -Infinity;
  if (Number.isNaN(high)) high = Infinity;
  if (low > high) {
    const swap = low;
    low = high;
    high = swap;
  }

  const number = Number(db);
  if (Number.isNaN(number) || number === -Infinity) return 0;
  if (number === Infinity) {
    return Number.isFinite(high) ? 10 ** (high / 20) : Infinity;
  }

  const bounded = Math.max(low, Math.min(high, number));
  return 10 ** (bounded / 20);
}

/**
 * Convert linear amplitude to dBFS.  Zero, negative, and NaN amplitudes map to
 * the floor (which may be `-Infinity`).  The optional floor/ceiling clamp the
 * resulting dB value.
 */
export function linearToDb(linear, floor = -Infinity, ceiling = Infinity) {
  let low = Number(floor);
  let high = Number(ceiling);
  if (Number.isNaN(low)) low = -Infinity;
  if (Number.isNaN(high)) high = Infinity;
  if (low > high) {
    const swap = low;
    low = high;
    high = swap;
  }

  const number = Number(linear);
  if (Number.isNaN(number) || number <= 0) {
    return Number.isFinite(low) ? low : -Infinity;
  }
  if (number === Infinity) {
    return Number.isFinite(high) ? high : Infinity;
  }

  const db = 20 * Math.log10(number);
  return Math.max(low, Math.min(high, db));
}

/**
 * Format a dB reading for display.  Non-finite readings (including
 * `-Infinity`) are shown as an em dash.
 */
export function formatVuDb(value, config = {}) {
  const number = Number(value);
  if (!Number.isFinite(number)) return '—';
  const requested = Number(config.decimals);
  const decimals = Number.isFinite(requested)
    ? Math.max(0, Math.min(6, Math.round(requested)))
    : 1;
  return number.toFixed(decimals);
}

/* ------------------------------------------------------------------------- */
/* Configuration                                                              */
/* ------------------------------------------------------------------------- */

function normalizedChannels(config) {
  const raw = firstDefined(
    config.channels,
    config.channel_mode,
    config.channelMode,
    config.channel,
    config.right_metric,
    config.rightMetric,
    config.right_field,
    config.rightField,
    config.right,
  );
  if (raw === undefined || raw === null || raw === '') return 'mono';
  const numeric = Number(raw);
  if (Number.isFinite(numeric) && numeric >= 2) return 'stereo';
  return /stereo|dual|lr|left.?right/i.test(String(raw)) ? 'stereo' : 'mono';
}

function normalizedInput(config) {
  const raw = firstDefined(
    config.input,
    config.scale,
    config.value_scale,
    config.valueScale,
    config.linear === true ? 'linear' : undefined,
  );
  if (raw === undefined || raw === null || raw === '') return 'db';
  return /lin|amp|normalized/i.test(String(raw)) ? 'linear' : 'db';
}

/**
 * Normalise a VU configuration.  Snake_case and camelCase aliases are
 * accepted.  Defaults are chosen for a standard `-60..0` dBFS audio meter.
 */
export function normalizeVuConfig(config = {}) {
  const raw = isPlainObject(config) ? config : {};
  const channels = normalizedChannels(raw);

  let min = finite(firstDefined(raw.min, raw.minDb, raw.min_db, raw.floorDb, raw.floor_db), -60);
  let max = finite(firstDefined(raw.max, raw.maxDb, raw.max_db, raw.ceilingDb, raw.ceiling_db), 0);
  if (max < min) {
    const swap = min;
    min = max;
    max = swap;
  }
  if (max === min) max = min + 1;

  const width = positive(firstDefined(raw.width, raw.w), 240);
  const height = positive(
    firstDefined(raw.height, raw.h),
    channels === 'stereo' ? 84 : 64,
  );

  const reducedRaw = firstDefined(raw.reducedMotion, raw.reduced_motion, raw.reduced);

  return {
    ...raw,
    channels,
    input: normalizedInput(raw),
    min,
    max,
    width,
    height,
    padding: nonNegative(firstDefined(raw.padding, raw.inset), 8),
    gap: nonNegative(firstDefined(raw.gap, raw.row_gap, raw.rowGap), 6),
    segmentGap: nonNegative(firstDefined(raw.segmentGap, raw.segment_gap), 1),
    segments: clampInteger(
      firstDefined(raw.segments, raw.segment_count, raw.segmentCount),
      1,
      MAX_SEGMENTS,
      24,
    ),
    orientation: /vert|column|up/i.test(String(firstDefined(raw.orientation, raw.direction, 'horizontal')))
      ? 'vertical'
      : 'horizontal',
    attackMs: nonNegative(firstDefined(raw.attackMs, raw.attack_ms), 10),
    releaseMs: nonNegative(firstDefined(raw.releaseMs, raw.release_ms), 300),
    falloffDbPerSec: nonNegative(
      firstDefined(raw.falloffDbPerSec, raw.falloff_db_per_sec, raw.falloff),
      24,
    ),
    peakHoldMs: nonNegative(firstDefined(raw.peakHoldMs, raw.peak_hold_ms), 1500),
    peakFalloffDbPerSec: nonNegative(
      firstDefined(raw.peakFalloffDbPerSec, raw.peak_falloff_db_per_sec),
      12,
    ),
    rmsWindowMs: nonNegative(firstDefined(raw.rmsWindowMs, raw.rms_window_ms), 300),
    clipThresholdDb: finite(
      firstDefined(
        raw.clipThresholdDb,
        raw.clip_threshold_db,
        raw.clipDb,
        raw.clip_db,
        raw.clipAt,
        raw.clip_at,
      ),
      max,
    ),
    clipAutoClearMs: nonNegative(
      firstDefined(
        raw.clipAutoClearMs,
        raw.clip_auto_clear_ms,
        raw.autoClearClipMs,
        raw.auto_clear_clip_ms,
        raw.autoClearMs,
        raw.auto_clear_ms,
      ),
      0,
    ),
    warn: finite(firstDefined(raw.warn, raw.warn_at, raw.warnAt, raw.thresholds?.warn), null),
    crit: finite(firstDefined(raw.crit, raw.critical, raw.crit_at, raw.critAt, raw.thresholds?.crit), null),
    unit: String(firstDefined(raw.unit, raw.units, '') ?? ''),
    label: String(firstDefined(raw.label, raw.title, raw.name, '') ?? ''),
    decimals: Number.isInteger(Number(firstDefined(raw.decimals, raw.precision)))
      ? clampInteger(firstDefined(raw.decimals, raw.precision), 0, 6, 1)
      : 1,
    showValue: firstDefined(raw.showValue, raw.show_value) !== false,
    showLabels: firstDefined(raw.showLabels, raw.show_labels)
      ?? channels === 'stereo',
    showPeak: firstDefined(raw.showPeak, raw.show_peak) !== false,
    showClip: firstDefined(raw.showClip, raw.show_clip) !== false,
    animate: firstDefined(raw.animate, raw.animated) !== false,
    reducedMotion: reducedRaw === true || reducedRaw === 'true' || reducedRaw === 1 || reducedRaw === '1',
    reducedMotionSet: reducedRaw !== undefined,
    leftLabel: String(firstDefined(raw.leftLabel, raw.left_label, 'L') ?? 'L'),
    rightLabel: String(firstDefined(raw.rightLabel, raw.right_label, 'R') ?? 'R'),
  };
}

/* ------------------------------------------------------------------------- */
/* State and ballistics                                                       */
/* ------------------------------------------------------------------------- */

function newChannelState(config) {
  return {
    displayDb: config.min,
    targetDb: config.min,
    rmsDb: config.min,
    peakDb: config.min,
    peakHoldUntilMs: 0,
    clip: false,
    clipAtMs: null,
    clipLastAtMs: null,
    hasSample: false,
    rms: [],
  };
}

/**
 * Create fresh per-widget VU state.  Both channel slots exist; `channels`
 * selects which one the renderer consumes.
 */
export function createVuState(config = {}) {
  const cfg = normalizeVuConfig(config);
  const left = newChannelState(cfg);
  const right = newChannelState(cfg);
  return {
    elapsedMs: 0,
    initialized: false,
    channels: cfg.channels,
    l: left,
    r: right,
    left,
    right,
  };
}

function readRawSample(sample) {
  if (Array.isArray(sample)) {
    return { left: sample[0], right: sample[1] };
  }
  if (sample && typeof sample === 'object') {
    if (Array.isArray(sample.levels)) {
      return { left: sample.levels[0], right: sample.levels[1] };
    }
    const left = firstDefined(
      sample.l,
      sample.left,
      sample.value,
      sample.level_l,
      sample.levelL,
      sample.db,
      sample.level,
    );
    const right = firstDefined(
      sample.r,
      sample.right,
      sample.value2,
      sample.second,
      sample.level_r,
      sample.levelR,
      sample.db2,
      sample.level2,
    );
    return { left, right };
  }
  return { left: sample, right: undefined };
}

function normalizeDbValue(raw, config, fallback) {
  if (raw === undefined || raw === null || raw === '') return fallback;
  const number = Number(raw);
  if (Number.isNaN(number)) return fallback;
  if (number === -Infinity) return config.min;
  if (number === Infinity) return config.max;
  const db = config.input === 'linear'
    ? (number <= 0 ? config.min : linearToDb(number, config.min, config.max))
    : number;
  return clamp(db, config.min, config.max);
}

function pushRmsSample(channel, timeMs, linear, config) {
  const window = channel.rms;
  const last = window[window.length - 1];
  if (last && Math.abs(last.t - timeMs) < EPSILON) {
    last.linear = linear;
  } else {
    window.push({ t: timeMs, linear });
  }

  const cutoff = timeMs - config.rmsWindowMs;
  while (window.length && window[0].t < cutoff) window.shift();
  while (window.length > MAX_RMS_SAMPLES) window.shift();
  if (!window.length) window.push({ t: timeMs, linear });

  let energy = 0;
  for (const sample of window) energy += sample.linear * sample.linear;
  const rmsLinear = Math.sqrt(energy / window.length);
  return linearToDb(rmsLinear, config.min, config.max);
}

/**
 * Advance one VU state by one sample.  The state is mutated and returned.
 * `dtMs` can be 0 for the first sample (which snaps to the target); a missing,
 * negative, or non-finite delta is treated as zero.  Repeated samples at the
 * same timestamp are collapsed in the RMS window.
 */
export function stepVuState(state, sample, config = {}, dtMs = 0) {
  const cfg = normalizeVuConfig(config);
  let target = state;
  if (!target || typeof target !== 'object') target = createVuState(cfg);
  if (!target.l || typeof target.l !== 'object') target.l = newChannelState(cfg);
  if (!target.r || typeof target.r !== 'object') target.r = newChannelState(cfg);
  target.left = target.l;
  target.right = target.r;

  const parsed = Number(dtMs);
  const delta = !Number.isFinite(parsed) || parsed < 0
    ? 0
    : Math.min(parsed, MAX_STEP_MS);

  const previousElapsed = finite(target.elapsedMs, 0);
  const now = previousElapsed + delta;
  target.elapsedMs = now;
  target.channels = cfg.channels;

  const raw = readRawSample(sample);
  const leftRaw = raw.left ?? raw.right;
  const rightRaw = raw.right ?? raw.left;
  const active = cfg.channels === 'stereo'
    ? [['l', leftRaw], ['r', rightRaw]]
    : [['l', leftRaw]];

  for (const [key, rawValue] of active) {
    const channel = target[key];
    if (!Array.isArray(channel.rms)) channel.rms = [];
    const fallback = channel.hasSample ? channel.targetDb : cfg.min;
    const targetDb = normalizeDbValue(rawValue, cfg, fallback);
    const current = clamp(channel.displayDb, cfg.min, cfg.max);
    let next;

    if (!target.initialized || !channel.hasSample) {
      next = targetDb;
    } else if (Math.abs(targetDb - current) <= EPSILON) {
      next = current;
    } else {
      const timeConstant = targetDb > current ? cfg.attackMs : cfg.releaseMs;
      next = timeConstant <= 0
        ? targetDb
        : current + (targetDb - current) * (1 - Math.exp(-delta / timeConstant));

      if (targetDb < current) {
        if (cfg.falloffDbPerSec > 0) {
          next = Math.max(next, current - cfg.falloffDbPerSec * (delta / 1000));
        }
        next = Math.max(next, targetDb);
      }
    }

    next = clamp(next, cfg.min, cfg.max);
    channel.displayDb = next;
    channel.targetDb = targetDb;

    channel.rmsDb = pushRmsSample(
      channel,
      now,
      dbToLinear(targetDb, cfg.min, cfg.max),
      cfg,
    );

    if (!channel.hasSample || targetDb >= channel.peakDb) {
      channel.peakDb = targetDb;
      channel.peakHoldUntilMs = now + cfg.peakHoldMs;
    } else {
      const decayStart = Math.max(previousElapsed, channel.peakHoldUntilMs);
      const decayMs = Math.max(0, now - decayStart);
      if (decayMs > 0 && cfg.peakFalloffDbPerSec > 0) {
        channel.peakDb -= cfg.peakFalloffDbPerSec * (decayMs / 1000);
      }
      channel.peakDb = Math.max(next, channel.peakDb);
      channel.peakDb = clamp(channel.peakDb, cfg.min, cfg.max);
    }

    if (targetDb >= cfg.clipThresholdDb - EPSILON) {
      if (!channel.clip) {
        channel.clip = true;
        channel.clipAtMs = now;
      }
      channel.clipLastAtMs = now;
    }
    if (
      channel.clip
      && cfg.clipAutoClearMs > 0
      && now - (channel.clipLastAtMs ?? channel.clipAtMs ?? now) >= cfg.clipAutoClearMs
    ) {
      channel.clip = false;
      channel.clipAtMs = null;
      channel.clipLastAtMs = null;
    }

    channel.hasSample = true;
  }

  target.initialized = true;
  return target;
}

/**
 * Clear a latched clip indicator.  Omit `channel` to clear both channels.
 */
export function clearClip(state, channel = null) {
  if (!state || typeof state !== 'object') return state;
  const keys = channel === undefined || channel === null
    ? ['l', 'r']
    : [String(channel).toLowerCase().startsWith('r') ? 'r' : 'l'];
  for (const key of keys) {
    if (state[key]) {
      state[key].clip = false;
      state[key].clipAtMs = null;
      state[key].clipLastAtMs = null;
    }
  }
  return state;
}

export const clearVuClip = clearClip;

/* ------------------------------------------------------------------------- */
/* Colours and severity                                                       */
/* ------------------------------------------------------------------------- */

export function resolveVuColors(config = {}, tokens = {}) {
  const explicit = config.colors && typeof config.colors === 'object' ? config.colors : {};
  return {
    normal: resolveColour(firstDefined(
      explicit.normal,
      explicit.fill,
      config.fill,
      config.color,
      tokenValue(tokens, ['vu.fill', 'vu.normal', 'gauge.fill', 'gauge.normal', 'color.accent']),
    ), tokens) ?? DEFAULT_COLORS.normal,
    warn: resolveColour(firstDefined(
      explicit.warn,
      config.warnColor,
      config.warn_color,
      tokenValue(tokens, ['vu.warn', 'gauge.warn', 'color.warn']),
    ), tokens) ?? DEFAULT_COLORS.warn,
    crit: resolveColour(firstDefined(
      explicit.crit,
      explicit.critical,
      config.critColor,
      config.crit_color,
      tokenValue(tokens, ['vu.crit', 'gauge.crit', 'color.crit', 'color.error']),
    ), tokens) ?? DEFAULT_COLORS.crit,
    track: resolveColour(firstDefined(
      explicit.track,
      config.track,
      tokenValue(tokens, ['vu.track', 'gauge.track', 'color.track']),
    ), tokens) ?? DEFAULT_COLORS.track,
    text: resolveColour(firstDefined(
      explicit.text,
      tokenValue(tokens, ['vu.text', 'gauge.text', 'color.fg', 'color.text']),
    ), tokens) ?? DEFAULT_COLORS.text,
    muted: resolveColour(firstDefined(
      explicit.muted,
      tokenValue(tokens, ['vu.muted', 'gauge.muted', 'color.muted', 'color.dim']),
    ), tokens) ?? DEFAULT_COLORS.muted,
    peak: resolveColour(firstDefined(
      explicit.peak,
      config.peakColor,
      config.peak_color,
      tokenValue(tokens, ['vu.peak', 'gauge.peak', 'color.fg', 'color.text']),
    ), tokens) ?? DEFAULT_COLORS.peak,
    clip: resolveColour(firstDefined(
      explicit.clip,
      config.clipColor,
      config.clip_color,
      tokenValue(tokens, ['vu.clip', 'gauge.clip', 'color.crit', 'color.error']),
    ), tokens) ?? DEFAULT_COLORS.clip,
  };
}

function severityForConfig(value, config) {
  const number = Number(value);
  if (!Number.isFinite(number)) return 'normal';
  if (config.crit !== null && number >= config.crit) return 'crit';
  if (config.warn !== null && number >= config.warn) return 'warn';
  return 'normal';
}

export function severityFor(value, config = {}) {
  return severityForConfig(value, normalizeVuConfig(config));
}

function severityWord(severity) {
  if (severity === 'crit') return 'critical';
  if (severity === 'warn') return 'warning';
  return 'normal';
}

function worstSeverity(severities) {
  if (severities.includes('crit')) return 'crit';
  if (severities.includes('warn')) return 'warn';
  return 'normal';
}

/* ------------------------------------------------------------------------- */
/* Geometry                                                                   */
/* ------------------------------------------------------------------------- */

function channelKeys(config) {
  return config.channels === 'stereo' ? ['l', 'r'] : ['l'];
}

function dbRatio(db, config) {
  return config.max === config.min
    ? 0
    : clamp((db - config.min) / (config.max - config.min), 0, 1);
}

function channelLabel(config, key, index) {
  if (config.channels !== 'stereo') return config.label;
  return key === 'r' ? config.rightLabel : config.leftLabel;
}

function layoutBars(config) {
  const keys = channelKeys(config);
  const count = keys.length;
  const horizontal = config.orientation !== 'vertical';
  const padding = config.padding;
  const gap = config.gap;
  const showLabels = Boolean(config.showLabels) && (count > 1 || Boolean(config.label));
  const labelSize = horizontal ? 14 : 12;
  const valueSize = horizontal ? 46 : 14;
  const clipSize = config.showClip ? (horizontal ? 8 : 6) : 0;
  const rows = [];

  if (horizontal) {
    const labelWidth = showLabels ? labelSize : 0;
    const valueWidth = config.showValue ? valueSize : 0;
    const barX = padding + labelWidth;
    const barWidth = Math.max(
      1,
      config.width - padding * 2 - labelWidth - valueWidth - clipSize - gap * 2,
    );
    const contentHeight = Math.max(1, config.height - padding * 2);
    const rowHeight = Math.max(1, (contentHeight - gap * (count - 1)) / count);
    for (let index = 0; index < count; index += 1) {
      rows.push({
        index,
        key: keys[index],
        label: channelLabel(config, keys[index], index),
        x: barX,
        y: padding + index * (rowHeight + gap),
        width: barWidth,
        height: rowHeight,
      });
    }
    return {
      horizontal,
      rows,
      barX,
      barWidth,
      labelWidth,
      valueWidth,
      clipSize,
      clipX: barX + barWidth + gap,
      valueX: config.width - padding,
      labelX: padding,
      labelSize,
      valueSize,
    };
  }

  const labelHeight = showLabels ? labelSize : 0;
  const valueHeight = config.showValue ? valueSize : 0;
  const barY = padding + labelHeight + (clipSize ? clipSize + gap : 0);
  const barHeight = Math.max(
    1,
    config.height - padding * 2 - labelHeight - valueHeight - clipSize - gap * 2,
  );
  const columnWidth = Math.max(1, (config.width - padding * 2 - gap * (count - 1)) / count);
  for (let index = 0; index < count; index += 1) {
    rows.push({
      index,
      key: keys[index],
      label: channelLabel(config, keys[index], index),
      x: padding + index * (columnWidth + gap),
      y: barY,
      width: columnWidth,
      height: barHeight,
    });
  }
  return {
    horizontal,
    rows,
    barY,
    barHeight,
    labelHeight,
    valueHeight,
    clipSize,
    clipY: padding + labelHeight,
    valueY: config.height - padding,
    labelY: padding + labelHeight - 2,
    labelSize,
    valueSize,
  };
}

function channelGeometry(config, layout, row, state, colors) {
  const source = state && typeof state === 'object' ? state : {};
  const displayDb = clamp(finite(source.displayDb, config.min), config.min, config.max);
  const rmsDb = clamp(finite(source.rmsDb, config.min), config.min, config.max);
  const peakDb = clamp(finite(source.peakDb, displayDb), config.min, config.max);
  const ratio = dbRatio(displayDb, config);
  const peakRatio = dbRatio(peakDb, config);
  const step = (config.max - config.min) / config.segments;
  const segmentGap = config.segmentGap;
  const segments = [];

  for (let index = 0; index < config.segments; index += 1) {
    const fromDb = config.min + step * index;
    const toDb = index === config.segments - 1 ? config.max : config.min + step * (index + 1);
    const severity = severityForConfig((fromDb + toDb) / 2, config);
    let x;
    let y;
    let width;
    let height;

    if (layout.horizontal) {
      const segmentWidth = Math.max(
        1,
        (row.width - segmentGap * (config.segments - 1)) / config.segments,
      );
      x = row.x + index * (segmentWidth + segmentGap);
      y = row.y;
      width = segmentWidth;
      height = row.height;
    } else {
      const segmentHeight = Math.max(
        1,
        (row.height - segmentGap * (config.segments - 1)) / config.segments,
      );
      x = row.x;
      y = row.y + row.height - (index + 1) * segmentHeight - index * segmentGap;
      width = row.width;
      height = segmentHeight;
    }

    segments.push({
      index,
      fromDb,
      toDb,
      severity,
      color: colors[severity],
      active: toDb <= displayDb + EPSILON,
      x,
      y,
      width,
      height,
      rx: Math.min(2, Math.min(width, height) / 2),
    });
  }

  const severity = source.clip ? 'crit' : severityForConfig(displayDb, config);

  const peak = layout.horizontal
    ? {
      x1: row.x + row.width * peakRatio,
      y1: row.y - 1,
      x2: row.x + row.width * peakRatio,
      y2: row.y + row.height + 1,
    }
    : {
      x1: row.x - 1,
      y1: row.y + row.height * (1 - peakRatio),
      x2: row.x + row.width + 1,
      y2: row.y + row.height * (1 - peakRatio),
    };

  const clipRect = layout.horizontal
    ? {
      x: layout.clipX,
      y: row.y,
      width: layout.clipSize,
      height: row.height,
      rx: Math.min(2, layout.clipSize / 2),
    }
    : {
      x: row.x,
      y: layout.clipY,
      width: row.width,
      height: layout.clipSize,
      rx: Math.min(2, layout.clipSize / 2),
    };

  const labelPos = layout.horizontal
    ? { x: layout.labelX, y: row.y + row.height / 2, anchor: 'start' }
    : { x: row.x + row.width / 2, y: layout.labelY, anchor: 'middle' };
  const valuePos = layout.horizontal
    ? { x: layout.valueX, y: row.y + row.height / 2, anchor: 'end' }
    : { x: row.x + row.width / 2, y: layout.valueY, anchor: 'middle' };

  return {
    key: row.key,
    index: row.index,
    label: row.label,
    displayDb,
    value: displayDb,
    rawValue: finite(source.displayDb, config.min),
    percent: ratio * 100,
    rmsDb,
    peakDb,
    peakPercent: peakRatio * 100,
    ratio,
    peakRatio,
    severity,
    color: colors[severity],
    clip: Boolean(source.clip),
    segments,
    peak,
    clipRect,
    labelPos,
    valuePos,
    readout: `${formatVuDb(displayDb, config)}${config.unit ? ` ${config.unit}` : ''}`,
    track: {
      x: row.x,
      y: row.y,
      width: row.width,
      height: row.height,
      rx: Math.min(4, Math.min(row.width, row.height) / 2),
    },
  };
}

/**
 * Pure geometry for the segmented VU meter.  The returned object contains one
 * entry per active channel with segment rectangles, threshold colours, a peak
 * marker, a clip indicator, and label/readout positions.
 */
export function vuGeometry(state, config = {}, tokens = {}) {
  const cfg = normalizeVuConfig(config);
  const colors = resolveVuColors(cfg, tokens);
  const layout = layoutBars(cfg);
  const channels = layout.rows.map((row) => channelGeometry(
    cfg,
    layout,
    row,
    state?.[row.key],
    colors,
  ));
  const severity = channels.some((channel) => channel.clip)
    ? 'crit'
    : worstSeverity(channels.map((channel) => channel.severity));

  return {
    ...cfg,
    colors,
    width: cfg.width,
    height: cfg.height,
    channelMode: cfg.channels,
    channels,
    rows: channels,
    severity,
    clip: channels.some((channel) => channel.clip),
    displayDb: channels[0]?.displayDb ?? cfg.min,
    value: channels[0]?.displayDb ?? cfg.min,
    percent: channels[0]?.percent ?? 0,
    peakDb: channels.reduce(
      (highest, channel) => Math.max(highest, channel.peakDb),
      cfg.min,
    ),
  };
}

/* ------------------------------------------------------------------------- */
/* SVG rendering                                                              */
/* ------------------------------------------------------------------------- */

function idSafe(value) {
  return String(value || 'vu').replace(/[^A-Za-z0-9_-]+/g, '-').slice(0, 64) || 'vu';
}

function hashText(value) {
  let hash = 5381;
  const text = String(value ?? '');
  for (let index = 0; index < text.length; index += 1) {
    hash = ((hash << 5) + hash + text.charCodeAt(index)) >>> 0;
  }
  return hash.toString(36);
}

function detectReducedMotion(config, options = {}) {
  if (typeof options.reducedMotion === 'boolean') return options.reducedMotion;
  if (config.reducedMotion === true) return true;
  if (config.reducedMotionSet) return false;
  if (typeof globalThis.matchMedia === 'function') {
    try {
      return Boolean(globalThis.matchMedia('(prefers-reduced-motion: reduce)').matches);
    } catch {
      return false;
    }
  }
  return false;
}

function vuClass(geometry, reducedMotion) {
  const classes = [
    'ozma-vu',
    `ozma-vu-${geometry.orientation}`,
    `ozma-vu-${geometry.channelMode || 'mono'}`,
  ];
  if (reducedMotion) classes.push('ozma-vu-reduced-motion');
  return classes.join(' ');
}

function transitionStyle(geometry, reducedMotion) {
  return geometry.animate && !reducedMotion
    ? 'transition: opacity 80ms linear, fill 160ms linear'
    : null;
}

function ariaLabelFor(geometry) {
  const readings = geometry.channels.map((channel) => {
    const prefix = geometry.channels.length > 1 && channel.label
      ? `${channel.label} `
      : '';
    return `${prefix}${channel.readout}`;
  });
  const base = `${geometry.label || 'VU meter'}: ${readings.join(', ')}`;
  if (geometry.clip) return `${base}, clipping`;
  if (geometry.severity === 'crit') return `${base}, critical`;
  if (geometry.severity === 'warn') return `${base}, warning`;
  return base;
}

function descriptionFor(geometry) {
  const range = `${formatVuDb(geometry.min, geometry)} to ${formatVuDb(geometry.max, geometry)}${geometry.unit ? ` ${geometry.unit}` : ''}`;
  const readings = geometry.channels.map((channel) => {
    const prefix = geometry.channels.length > 1 && channel.label
      ? `${channel.label} `
      : '';
    const peak = formatVuDb(channel.peakDb, geometry);
    const rms = formatVuDb(channel.rmsDb, geometry);
    return `${prefix}${channel.readout}; peak ${peak}${geometry.unit ? ` ${geometry.unit}` : ''}; RMS ${rms}${geometry.unit ? ` ${geometry.unit}` : ''}${channel.clip ? '; clipping' : ''}`;
  });
  return `${geometry.label || 'VU meter'}: ${readings.join('; ')}; range ${range}; ${severityWord(geometry.severity)}.`;
}

function textAttributes(colors, fontFamily, size, weight = 400) {
  return `fill="${escapeXml(colors.text)}" font-family="${escapeXml(fontFamily)}" font-size="${numberText(size)}" font-weight="${weight}"`;
}

/**
 * Render accessible VU SVG markup.  Pure: no DOM access, no mutation.
 * Accepts `(config, state, tokens, options)`; the first two arguments may be
 * swapped for convenience.
 */
export function renderVuSVG(config = {}, state = null, tokens = {}, options = {}) {
  const args = pairRenderArgs(config, state);
  const geometry = vuGeometry(args.state, args.config, tokens);
  const reducedMotion = detectReducedMotion(geometry, options);
  const fontFamily = tokenString(
    tokens,
    ['font.family', 'fontFamily', 'font.mono', 'typography.family'],
    'system-ui, sans-serif',
  );
  const uid = idSafe(
    options.id
      ?? geometry.id
      ?? `vu-${hashText(`${geometry.label}|${geometry.min}|${geometry.max}|${geometry.segments}`)}`,
  );
  const style = transitionStyle(geometry, reducedMotion);
  const parts = [];

  parts.push(`<svg xmlns="${SVG_NS}" viewBox="0 0 ${numberText(geometry.width)} ${numberText(geometry.height)}" class="${escapeXml(vuClass(geometry, reducedMotion))}" role="img" aria-label="${escapeXml(ariaLabelFor(geometry))}" aria-describedby="${uid}-desc" data-reduced-motion="${reducedMotion ? 'true' : 'false'}" data-severity="${geometry.severity}" data-clip="${geometry.clip ? 'true' : 'false'}"${geometry.animate ? ' data-animate="true"' : ''}${style ? ` style="${escapeXml(style)}"` : ''} focusable="false" preserveAspectRatio="xMidYMid meet">`);
  parts.push(`<title id="${uid}-title">${escapeXml(geometry.label || 'VU meter')}</title>`);
  parts.push(`<desc id="${uid}-desc">${escapeXml(descriptionFor(geometry))}</desc>`);

  for (const channel of geometry.channels) {
    parts.push(`<g class="ozma-vu-channel" data-channel="${escapeXml(channel.key)}" data-severity="${channel.severity}" data-clip="${channel.clip ? 'true' : 'false'}">`);
    for (const segment of channel.segments) {
      parts.push(`<rect class="ozma-vu-segment-track" x="${numberText(segment.x)}" y="${numberText(segment.y)}" width="${numberText(segment.width)}" height="${numberText(segment.height)}" rx="${numberText(segment.rx)}" fill="${escapeXml(geometry.colors.track)}" fill-opacity="0.28"/>`);
      parts.push(`<rect class="ozma-vu-segment" x="${numberText(segment.x)}" y="${numberText(segment.y)}" width="${numberText(segment.width)}" height="${numberText(segment.height)}" rx="${numberText(segment.rx)}" fill="${escapeXml(segment.color)}" fill-opacity="${segment.active ? '1' : '0.16'}" data-severity="${segment.severity}" data-active="${segment.active ? 'true' : 'false'}"/>`);
    }
    if (geometry.showPeak) {
      parts.push(`<line class="ozma-vu-peak" x1="${numberText(channel.peak.x1)}" y1="${numberText(channel.peak.y1)}" x2="${numberText(channel.peak.x2)}" y2="${numberText(channel.peak.y2)}" stroke="${escapeXml(geometry.colors.peak)}" stroke-width="2" stroke-linecap="round"/>`);
    }
    if (geometry.showClip) {
      parts.push(`<rect class="ozma-vu-clip" x="${numberText(channel.clipRect.x)}" y="${numberText(channel.clipRect.y)}" width="${numberText(channel.clipRect.width)}" height="${numberText(channel.clipRect.height)}" rx="${numberText(channel.clipRect.rx)}" fill="${escapeXml(geometry.colors.clip)}" fill-opacity="${channel.clip ? '1' : '0.16'}" data-active="${channel.clip ? 'true' : 'false'}"/>`);
    }
    if (geometry.showLabels && channel.label) {
      parts.push(`<text class="ozma-vu-label" x="${numberText(channel.labelPos.x)}" y="${numberText(channel.labelPos.y)}" text-anchor="${channel.labelPos.anchor}" dominant-baseline="middle" ${textAttributes(geometry.colors, fontFamily, geometry.orientation === 'vertical' ? 10 : 11, 600)}>${escapeXml(channel.label)}</text>`);
    }
    if (geometry.showValue) {
      parts.push(`<text class="ozma-vu-value" x="${numberText(channel.valuePos.x)}" y="${numberText(channel.valuePos.y)}" text-anchor="${channel.valuePos.anchor}" dominant-baseline="middle" ${textAttributes(geometry.colors, fontFamily, geometry.orientation === 'vertical' ? 9 : 11, 650)}>${escapeXml(channel.readout)}</text>`);
    }
    parts.push('</g>');
  }

  parts.push('</svg>');
  return parts.join('');
}

/* ------------------------------------------------------------------------- */
/* DOM helpers                                                                */
/* ------------------------------------------------------------------------- */

function classList(value) {
  return String(value || '').split(/\s+/).filter(Boolean);
}

function hasClass(element, name) {
  if (!element || typeof element.getAttribute !== 'function') return false;
  return classList(element.getAttribute('class')).includes(name);
}

function byClass(root, name) {
  if (!root) return null;
  if (hasClass(root, name)) return root;
  const children = Array.isArray(root.children)
    ? root.children
    : root.childNodes
      ? Array.from(root.childNodes)
      : [];
  for (const child of children) {
    const found = byClass(child, name);
    if (found) return found;
  }
  return null;
}

function allByClass(root, name) {
  const found = [];
  if (!root) return found;
  if (hasClass(root, name)) found.push(root);
  const children = Array.isArray(root.children)
    ? root.children
    : root.childNodes
      ? Array.from(root.childNodes)
      : [];
  for (const child of children) {
    found.push(...allByClass(child, name));
  }
  return found;
}

function byTag(root, tag) {
  if (!root) return null;
  const name = String(tag || '').toLowerCase();
  if (String(root.tagName || '').toLowerCase() === name) return root;
  const children = Array.isArray(root.children)
    ? root.children
    : root.childNodes
      ? Array.from(root.childNodes)
      : [];
  for (const child of children) {
    const found = byTag(child, name);
    if (found) return found;
  }
  return null;
}

function setAttributes(element, attributes) {
  if (!element || typeof element.setAttribute !== 'function') return;
  for (const [name, value] of Object.entries(attributes)) {
    if (value === undefined) continue;
    if (value === null) {
      if (typeof element.removeAttribute === 'function') element.removeAttribute(name);
      continue;
    }
    element.setAttribute(name, String(value));
  }
}

function clearElement(element) {
  if (!element) return;
  if (typeof element.replaceChildren === 'function') {
    element.replaceChildren();
    return;
  }
  while (element.firstChild) element.removeChild(element.firstChild);
}

function createSvgElement(doc, tag, className) {
  const element = doc.createElementNS(SVG_NS, tag);
  if (className) element.setAttribute('class', className);
  return element;
}

function findVuSvg(container) {
  const first = container.firstElementChild;
  if (
    first
    && String(first.tagName || '').toLowerCase() === 'svg'
    && hasClass(first, 'ozma-vu')
  ) {
    return first;
  }
  if (typeof container.querySelector === 'function') {
    return container.querySelector('svg.ozma-vu');
  }
  return null;
}

function signatureFor(geometry) {
  return [
    geometry.segments,
    geometry.orientation,
    geometry.channelMode || 'mono',
    geometry.width,
    geometry.height,
    geometry.showValue ? 1 : 0,
    geometry.showPeak ? 1 : 0,
    geometry.showClip ? 1 : 0,
    geometry.showLabels ? 1 : 0,
    geometry.padding,
    geometry.gap,
    geometry.segmentGap,
  ].join('|');
}

function buildStructure(svg, geometry, doc) {
  clearElement(svg);

  const title = createSvgElement(doc, 'title', 'ozma-vu-title');
  const desc = createSvgElement(doc, 'desc', 'ozma-vu-desc');
  svg.appendChild(title);
  svg.appendChild(desc);

  for (const channel of geometry.channels) {
    const group = createSvgElement(doc, 'g', 'ozma-vu-channel');
    group.setAttribute('data-channel', channel.key);
    svg.appendChild(group);

    for (const segment of channel.segments) {
      group.appendChild(createSvgElement(doc, 'rect', 'ozma-vu-segment-track'));
      group.appendChild(createSvgElement(doc, 'rect', 'ozma-vu-segment'));
    }
    if (geometry.showPeak) group.appendChild(createSvgElement(doc, 'line', 'ozma-vu-peak'));
    if (geometry.showClip) group.appendChild(createSvgElement(doc, 'rect', 'ozma-vu-clip'));
    if (geometry.showLabels) group.appendChild(createSvgElement(doc, 'text', 'ozma-vu-label'));
    if (geometry.showValue) group.appendChild(createSvgElement(doc, 'text', 'ozma-vu-value'));
  }
}

function updateSegmentElements(group, channel, geometry) {
  const tracks = allByClass(group, 'ozma-vu-segment-track');
  const lights = allByClass(group, 'ozma-vu-segment');
  for (let index = 0; index < channel.segments.length; index += 1) {
    const segment = channel.segments[index];
    const track = tracks[index];
    const light = lights[index];
    if (track) {
      setAttributes(track, {
        x: numberText(segment.x),
        y: numberText(segment.y),
        width: numberText(segment.width),
        height: numberText(segment.height),
        rx: numberText(segment.rx),
        fill: geometry.colors.track,
        'fill-opacity': 0.28,
      });
    }
    if (light) {
      setAttributes(light, {
        x: numberText(segment.x),
        y: numberText(segment.y),
        width: numberText(segment.width),
        height: numberText(segment.height),
        rx: numberText(segment.rx),
        fill: segment.color,
        'fill-opacity': segment.active ? 1 : 0.16,
        'data-severity': segment.severity,
        'data-active': segment.active ? 'true' : 'false',
      });
    }
  }
}

function applyVu(svg, config, state, tokens, options) {
  const geometry = vuGeometry(state, config, tokens);
  const reducedMotion = detectReducedMotion(geometry, options);
  const doc = svg.ownerDocument;
  const signature = signatureFor(geometry);

  if (svg.getAttribute('data-vu-signature') !== signature) {
    if (!doc || typeof doc.createElementNS !== 'function') return;
    buildStructure(svg, geometry, doc);
    svg.setAttribute('data-vu-signature', signature);
  }

  const fontFamily = tokenString(
    tokens,
    ['font.family', 'fontFamily', 'font.mono', 'typography.family'],
    'system-ui, sans-serif',
  );
  const uid = idSafe(
    options.id
      ?? geometry.id
      ?? `vu-${hashText(`${geometry.label}|${geometry.min}|${geometry.max}|${geometry.segments}`)}`,
  );
  const style = transitionStyle(geometry, reducedMotion);

  setAttributes(svg, {
    viewBox: `0 0 ${numberText(geometry.width)} ${numberText(geometry.height)}`,
    class: vuClass(geometry, reducedMotion),
    role: 'img',
    'aria-label': ariaLabelFor(geometry),
    'aria-describedby': `${uid}-desc`,
    'data-reduced-motion': reducedMotion ? 'true' : 'false',
    'data-severity': geometry.severity,
    'data-clip': geometry.clip ? 'true' : 'false',
    'data-animate': geometry.animate ? 'true' : null,
    focusable: 'false',
    preserveAspectRatio: 'xMidYMid meet',
    style,
  });

  const title = byTag(svg, 'title');
  if (title) {
    title.setAttribute('id', `${uid}-title`);
    title.textContent = geometry.label || 'VU meter';
  }
  const desc = byTag(svg, 'desc');
  if (desc) {
    desc.setAttribute('id', `${uid}-desc`);
    desc.textContent = descriptionFor(geometry);
  }

  const groups = allByClass(svg, 'ozma-vu-channel');
  for (let index = 0; index < geometry.channels.length; index += 1) {
    const channel = geometry.channels[index];
    const group = groups[index];
    if (!group) continue;
    group.setAttribute('data-channel', channel.key);
    group.setAttribute('data-severity', channel.severity);
    group.setAttribute('data-clip', channel.clip ? 'true' : 'false');

    updateSegmentElements(group, channel, geometry);

    if (geometry.showPeak) {
      const peak = byClass(group, 'ozma-vu-peak');
      if (peak) {
        setAttributes(peak, {
          x1: numberText(channel.peak.x1),
          y1: numberText(channel.peak.y1),
          x2: numberText(channel.peak.x2),
          y2: numberText(channel.peak.y2),
          stroke: geometry.colors.peak,
          'stroke-width': 2,
          'stroke-linecap': 'round',
        });
      }
    }

    if (geometry.showClip) {
      const clip = byClass(group, 'ozma-vu-clip');
      if (clip) {
        setAttributes(clip, {
          x: numberText(channel.clipRect.x),
          y: numberText(channel.clipRect.y),
          width: numberText(channel.clipRect.width),
          height: numberText(channel.clipRect.height),
          rx: numberText(channel.clipRect.rx),
          fill: geometry.colors.clip,
          'fill-opacity': channel.clip ? 1 : 0.16,
          'data-active': channel.clip ? 'true' : 'false',
        });
      }
    }

    if (geometry.showLabels) {
      const label = byClass(group, 'ozma-vu-label');
      if (label) {
        label.textContent = channel.label || '';
        setAttributes(label, {
          x: numberText(channel.labelPos.x),
          y: numberText(channel.labelPos.y),
          'text-anchor': channel.labelPos.anchor,
          'dominant-baseline': 'middle',
          fill: geometry.colors.text,
          'font-family': fontFamily,
          'font-size': geometry.orientation === 'vertical' ? 10 : 11,
          'font-weight': 600,
        });
      }
    }

    if (geometry.showValue) {
      const value = byClass(group, 'ozma-vu-value');
      if (value) {
        value.textContent = channel.readout;
        setAttributes(value, {
          x: numberText(channel.valuePos.x),
          y: numberText(channel.valuePos.y),
          'text-anchor': channel.valuePos.anchor,
          'dominant-baseline': 'middle',
          fill: geometry.colors.text,
          'font-family': fontFamily,
          'font-size': geometry.orientation === 'vertical' ? 9 : 11,
          'font-weight': 650,
        });
      }
    }
  }
}

function pairRenderArgs(first, second) {
  const looksLikeState = (value) => Boolean(
    value
    && typeof value === 'object'
    && !Array.isArray(value)
    && (
      Object.prototype.hasOwnProperty.call(value, 'elapsedMs')
      || (value.l && value.r && typeof value.l === 'object')
    ),
  );

  if (looksLikeState(first) && !looksLikeState(second)) {
    return {
      config: second && typeof second === 'object' ? second : {},
      state: first,
    };
  }
  if (looksLikeState(second) && !looksLikeState(first)) {
    return {
      config: first && typeof first === 'object' ? first : {},
      state: second,
    };
  }
  return {
    config: first && typeof first === 'object' ? first : {},
    state: second && typeof second === 'object' ? second : null,
  };
}

/**
 * Render a VU meter into a container.  Returns the SVG element in DOM
 * environments, otherwise the SVG markup string.
 */
export function renderVu(container, config = {}, state = null, tokens = {}, options = {}) {
  if (!container || typeof container !== 'object') {
    throw new TypeError('renderVu requires a container element');
  }

  const args = pairRenderArgs(config, state);
  const resolvedConfig = args.config;
  const resolvedState = args.state ?? createVuState(resolvedConfig);
  const doc = container.ownerDocument
    ?? (typeof globalThis.document !== 'undefined' ? globalThis.document : null);

  if (doc && typeof doc.createElementNS === 'function' && typeof container.appendChild === 'function') {
    clearElement(container);
    const svg = doc.createElementNS(SVG_NS, 'svg');
    container.appendChild(svg);
    applyVu(svg, resolvedConfig, resolvedState, tokens, options);
    return svg;
  }

  const markup = renderVuSVG(resolvedConfig, resolvedState, tokens, options);
  container.innerHTML = markup;
  return container.firstElementChild ?? markup;
}

/**
 * Update an existing VU meter in place.  The SVG element is reused; only
 * attributes, segment fills, markers, and labels are updated.  Falls back to a
 * fresh render when the container has no VU SVG yet.
 */
export function updateVu(container, state = null, config = {}, tokens = {}, options = {}) {
  if (!container || typeof container !== 'object') {
    throw new TypeError('updateVu requires a container element');
  }

  const args = pairRenderArgs(state, config);
  const resolvedConfig = args.config;
  const resolvedState = args.state ?? createVuState(resolvedConfig);
  const svg = findVuSvg(container);
  if (!svg) return renderVu(container, resolvedConfig, resolvedState, tokens, options);
  applyVu(svg, resolvedConfig, resolvedState, tokens, options);
  return svg;
}

export const __private__ = Object.freeze({
  DEFAULT_COLORS,
  MAX_RMS_SAMPLES,
  MAX_STEP_MS,
});
