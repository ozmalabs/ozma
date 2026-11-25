// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
/**
 * Dependency-free multi-series SVG chart widget.
 *
 * This module replaces the optional uPlot vendoring with a small, bounded
 * renderer built on the same primitives as the sparkline widget.  It exposes
 * pure helpers for tests and data modelling (`normalizeSeries`, `appendSample`,
 * `chartGeometry`, `chartPath`, `scaleValue`, `niceTicks`, `timeTicks`) plus a
 * DOM renderer (`renderChart`) and an in-place updater (`updateChart`).
 *
 * The renderer supports multiple series, line/step/area styles, gap-aware
 * paths, auto or explicit ranges, a time or index x-axis, grid/axis labels,
 * thresholds and severity bands, last-value markers, a legend, an optional
 * pointer tooltip, reduced-motion preferences, and theme tokens.  There are no
 * external dependencies, no `eval`, and no browser is required for the pure
 * helpers or the SVG string renderer.
 */

const SVG_NS = 'http://www.w3.org/2000/svg';

const DEFAULT_WIDTH = 480;
const DEFAULT_HEIGHT = 220;
const DEFAULT_SAMPLES = 4096;
const MAX_SAMPLES = 4096;
const MAX_SERIES = 16;
const MAX_TICKS = 12;
const MAX_DIMENSION = 4096;
const MAX_LABEL = 128;

const DEFAULT_PALETTE = Object.freeze([
  '#5b6fff',
  '#42d392',
  '#f0b94a',
  '#f26464',
  '#b07cff',
  '#35d0ff',
  '#ff8a5c',
  '#8d9bb3',
]);

const DEFAULT_COLORS = Object.freeze({
  palette: DEFAULT_PALETTE,
  line: DEFAULT_PALETTE[0],
  fill: DEFAULT_PALETTE[0],
  warn: '#f0b94a',
  crit: '#f26464',
  grid: 'rgba(255, 255, 255, 0.12)',
  axis: 'rgba(255, 255, 255, 0.28)',
  baseline: 'rgba(255, 255, 255, 0.28)',
  text: '#e5e7eb',
  muted: '#8d9bb3',
  background: 'rgba(10, 10, 15, 0.88)',
});

const REFERENCE_RE = /\{([^{}]+)\}/g;
const SAFE_COLOUR_RE = /^(?:#[0-9a-f]{3,8}|(?:rgb|rgba|hsl|hsla)\(\s*[-+0-9.%\s,/]+\)|[a-z]{3,24})$/i;

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

function boundedInteger(value, min, max, fallback) {
  return clampInteger(value, min, max, fallback);
}

function finite(value, fallback) {
  if (value === undefined || value === null || value === '') return fallback;
  const number = Number(value);
  return Number.isFinite(number) ? number : fallback;
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

function safeLabel(value, fallback = '') {
  return String(value ?? fallback).replace(/[\u0000-\u001f\u007f]+/g, ' ').slice(0, MAX_LABEL);
}

function idSafe(value) {
  return String(value || 'chart')
    .replace(/[^A-Za-z0-9_-]+/g, '-')
    .slice(0, 64) || 'chart';
}

function hashText(value) {
  let hash = 5381;
  const text = String(value ?? '');
  for (let index = 0; index < text.length; index += 1) {
    hash = ((hash << 5) + hash + text.charCodeAt(index)) >>> 0;
  }
  return hash.toString(36);
}

function hasOwn(value, key) {
  return Boolean(value) && Object.prototype.hasOwnProperty.call(value, key);
}

function isPlainObject(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) return false;
  const prototype = Object.getPrototypeOf(value);
  return prototype === Object.prototype || prototype === null;
}

function isTokenObject(value) {
  return isPlainObject(value) && (hasOwn(value, '$value') || hasOwn(value, 'value'));
}

/* ------------------------------------------------------------------------- */
/* Token resolution                                                           */
/* ------------------------------------------------------------------------- */

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
  if (hasOwn(source, key)) return { found: true, value: source[key] };
  let current = source;
  for (const part of String(key).split('.')) {
    if (!isPlainObject(current) || !hasOwn(current, part)) return { found: false };
    current = current[part];
  }
  return { found: true, value: current };
}

function unwrap(value) {
  if (isTokenObject(value)) return hasOwn(value, '$value') ? value.$value : value.value;
  return value;
}

function parseReference(body) {
  const pipe = body.indexOf('|');
  const comma = body.indexOf(',');
  let split = pipe;
  if (split === -1 && comma !== -1 && !body.slice(0, comma).includes('(')) split = comma;
  if (split === -1) return { key: body.trim(), fallback: '' };
  return {
    key: body.slice(0, split).trim(),
    fallback: body.slice(split + 1).trim(),
  };
}

function lookupTokenValue(key, tokens, depth = 0) {
  if (depth > 12) return undefined;
  const found = rawToken(tokens, key);
  if (!found.found) return undefined;
  const value = unwrap(found.value);
  if (value === undefined || value === null) return undefined;
  if (typeof value === 'string') return resolveTokenString(value, tokens, depth + 1);
  return value;
}

function resolveTokenString(text, tokens, depth = 0) {
  if (typeof text !== 'string') return text;
  if (depth > 12) return text;

  const full = text.match(/^\{([^{}]+)\}$/);
  if (full) {
    const { key, fallback } = parseReference(full[1]);
    const value = lookupTokenValue(key, tokens, depth + 1);
    if (value !== undefined) return value;
    return fallback ? resolveTokenString(fallback, tokens, depth + 1) : text;
  }

  return text.replace(REFERENCE_RE, (match, body) => {
    const { key, fallback } = parseReference(body);
    const value = lookupTokenValue(key, tokens, depth + 1);
    if (value !== undefined) {
      return Array.isArray(value) ? value.join(', ') : String(value);
    }
    return fallback ? String(resolveTokenString(fallback, tokens, depth + 1)) : match;
  });
}

function tokenValue(tokens, names, fallback) {
  for (const name of names) {
    if (!name) continue;
    const value = lookupTokenValue(name, tokens);
    if (value !== undefined && value !== '') return value;
  }
  return fallback;
}

function tokenString(tokens, names, fallback) {
  const value = tokenValue(tokens, names, fallback);
  if (Array.isArray(value)) return value.join(', ');
  if (isPlainObject(value)) return fallback;
  return String(value);
}

function resolveColour(value, tokens) {
  if (value === undefined || value === null || value === '') return undefined;
  if (typeof value === 'string') return String(resolveTokenString(value, tokens));
  return String(value);
}

/**
 * Validate a colour before it reaches an SVG attribute.  Only hex values,
 * functional rgb/hsl colours, and plain CSS names are accepted.  This keeps
 * arbitrary strings such as `url(...)` or `javascript:` out of the markup.
 */
export function safeChartColour(value, fallback = DEFAULT_COLORS.line) {
  const text = String(value ?? '').trim().slice(0, 64);
  if (!text) return fallback;
  return SAFE_COLOUR_RE.test(text) ? text : fallback;
}

/* ------------------------------------------------------------------------- */
/* Series model                                                               */
/* ------------------------------------------------------------------------- */

function isPointLike(value) {
  return isPlainObject(value)
    && (hasOwn(value, 'x') || hasOwn(value, 't') || hasOwn(value, 'time') || hasOwn(value, 'timestamp'))
    && (hasOwn(value, 'y') || hasOwn(value, 'value') || hasOwn(value, 'v'));
}

function isSeriesDescriptor(value) {
  return isPlainObject(value) && (
    hasOwn(value, 'values')
    || hasOwn(value, 'samples')
    || hasOwn(value, 'history')
    || hasOwn(value, 'data')
    || hasOwn(value, 'points')
    || hasOwn(value, 'metric')
    || hasOwn(value, 'field')
    || hasOwn(value, 'field_id')
    || hasOwn(value, 'fieldId')
    || hasOwn(value, 'key')
  );
}

function numericSample(value) {
  if (value === undefined || value === null || value === '') return null;
  if (typeof value === 'boolean') return value ? 1 : 0;
  if (typeof value === 'bigint') {
    const number = Number(value);
    return Number.isFinite(number) ? number : null;
  }
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function timeSample(value) {
  if (value === undefined || value === null || value === '') return null;
  if (value instanceof Date) {
    const time = value.getTime();
    return Number.isFinite(time) ? time : null;
  }
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function readSample(raw) {
  if (raw === undefined || raw === null || raw === '') return { value: null, time: null };
  if (Array.isArray(raw)) {
    if (raw.length >= 2) {
      return { value: numericSample(raw[1]), time: timeSample(raw[0]) };
    }
    return { value: numericSample(raw[0]), time: null };
  }
  if (isPlainObject(raw)) {
    return {
      value: numericSample(firstDefined(raw.y, raw.value, raw.v, raw[1])),
      time: timeSample(firstDefined(raw.x, raw.t, raw.time, raw.timestamp, raw.ts, raw[0])),
    };
  }
  return { value: numericSample(raw), time: null };
}

function samplesFrom(value) {
  if (Array.isArray(value)) return value;
  if (ArrayBuffer.isView(value)) return Array.from(value);
  if (isPlainObject(value)) {
    for (const key of ['values', 'samples', 'history', 'data', 'points']) {
      if (value[key] !== undefined) return samplesFrom(value[key]);
    }
    if (isPointLike(value)) return [value];
  }
  if (typeof value === 'number' || typeof value === 'bigint' || typeof value === 'boolean') {
    return [value];
  }
  return [];
}

function normaliseStyle(value, area = false) {
  const style = String(value ?? '').toLowerCase();
  if (style === 'step' || style === 'steps') return 'step';
  if (style === 'area' || style === 'filled' || style === 'fill') return 'area';
  if (style === 'area-step' || style === 'area_step' || style === 'step-area') return 'area-step';
  if (style === 'dot' || style === 'dots' || style === 'marker') return 'dot';
  return area ? 'area' : 'line';
}

function coerceSeriesList(input) {
  if (input === undefined || input === null) return [];
  if (ArrayBuffer.isView(input)) return [{ values: Array.from(input) }];
  if (typeof input === 'number' || typeof input === 'bigint' || typeof input === 'string' || typeof input === 'boolean') {
    return [{ values: [input] }];
  }
  if (Array.isArray(input)) {
    if (!input.length) return [];
    const first = input[0];
    if (Array.isArray(first) || isPointLike(first)) return [{ values: input }];
    if (input.every((item) => isSeriesDescriptor(item))) return input;
    if (input.some((item) => isSeriesDescriptor(item))) {
      return input.map((item) => (isSeriesDescriptor(item) ? item : { values: [item] }));
    }
    return [{ values: input }];
  }
  if (isPlainObject(input)) {
    if (Array.isArray(input.series)) return input.series;
    if (['values', 'samples', 'history', 'data', 'points'].some((key) => input[key] !== undefined)) {
      return [input];
    }
    if (isPointLike(input)) return [{ values: [input] }];
    const entries = Object.entries(input);
    if (
      entries.length
      && entries.every(([, value]) => (
        Array.isArray(value) || ArrayBuffer.isView(value) || isPlainObject(value)
      ))
    ) {
      return entries.map(([key, value]) => (
        isPlainObject(value)
          ? { ...value, label: firstDefined(value.label, value.name, key) }
          : { label: key, values: value }
      ));
    }
  }
  return [];
}

function seriesCapacity(settings = {}, fallback = DEFAULT_SAMPLES) {
  return boundedInteger(
    firstDefined(settings.capacity, settings.points, settings.limit, settings.historyPoints),
    1,
    MAX_SAMPLES,
    Math.max(1, Math.min(MAX_SAMPLES, Math.floor(Number(fallback) || DEFAULT_SAMPLES))),
  );
}

/**
 * Normalise one or more series into a bounded, gap-aware model.
 *
 * Accepts a numeric/typed array, a single series object
 * (`{label, values|samples|history|data|points}`), an array of series objects,
 * an object map (`{cpu: [...], mem: [...]}`), or `{series: [...]}`.  Missing and
 * non-finite samples become `null` gaps.  Series and sample counts are capped.
 *
 * @param {*} input
 * @param {{capacity?:number, maxSeries?:number, palette?:string[], labels?:string[]}} [options]
 * @returns {Array<object>}
 */
export function normalizeSeries(input, options = {}) {
  const settings = typeof options === 'number' ? { capacity: options } : (options || {});
  const capacity = seriesCapacity(settings);
  const maxSeries = boundedInteger(settings.maxSeries, 1, MAX_SERIES, MAX_SERIES);
  const palette = Array.isArray(settings.palette) && settings.palette.length
    ? settings.palette
    : DEFAULT_PALETTE;
  const list = coerceSeriesList(input).slice(0, maxSeries);

  return list.map((item, index) => {
    const descriptor = isPlainObject(item) ? item : { values: item };
    const values = [];
    const times = [];
    let hasTimes = false;

    for (const raw of samplesFrom(descriptor)) {
      const sample = readSample(raw);
      values.push(sample.value);
      times.push(sample.time);
      if (sample.time !== null) hasTimes = true;
    }

    const boundedValues = values.slice(-capacity);
    const boundedTimes = hasTimes ? times.slice(-capacity) : null;
    const defined = boundedValues.filter((value) => value !== null);
    const labelText = safeLabel(
      firstDefined(
        descriptor.label,
        descriptor.name,
        descriptor.title,
        settings.labels?.[index],
        `Series ${index + 1}`,
      ),
      `Series ${index + 1}`,
    );
    const paletteColour = palette[index % palette.length];
    const colour = safeChartColour(
      firstDefined(descriptor.color, descriptor.colour, descriptor.stroke, paletteColour),
      paletteColour,
    );
    const area = Boolean(
      firstDefined(descriptor.area, descriptor.areaFill, descriptor.area_fill, descriptor.fill, false),
    );
    const style = normaliseStyle(firstDefined(descriptor.style, descriptor.mark, descriptor.type), area);
    const idText = idSafe(firstDefined(
      descriptor.id,
      descriptor.key,
      descriptor.metric,
      descriptor.field,
      `series-${index + 1}`,
    ));

    return {
      ...descriptor,
      id: idText,
      label: labelText,
      color: colour,
      unit: safeLabel(firstDefined(descriptor.unit, ''), ''),
      style,
      area: area || style === 'area' || style === 'area-step',
      fillOpacity: clamp(
        finite(firstDefined(descriptor.fillOpacity, descriptor.fill_opacity, descriptor.opacity), 0.16),
        0,
        1,
      ),
      values: boundedValues,
      times: boundedTimes,
      capacity,
      index,
      definedCount: defined.length,
      gapCount: boundedValues.length - defined.length,
      min: defined.length ? Math.min(...defined) : null,
      max: defined.length ? Math.max(...defined) : null,
      last: defined.length ? defined[defined.length - 1] : null,
    };
  });
}

function ringValues(target) {
  if (Array.isArray(target)) return target;
  if (ArrayBuffer.isView(target)) return Array.from(target);
  if (isPlainObject(target)) {
    for (const key of ['values', 'samples', 'history', 'data', 'points']) {
      if (Array.isArray(target[key])) return target[key];
      if (ArrayBuffer.isView(target[key])) return Array.from(target[key]);
    }
  }
  return [];
}

/**
 * Append a sample to a bounded ring without mutating the input.
 *
 * `target` may be a numeric array, a series/ring object, or an array of series
 * objects.  A scalar applied to a list is broadcast to every series.  Gaps are
 * represented by `null`, and both sample and time windows are bounded by
 * `capacity` (defaults to the ring's capacity, then 4096).
 *
 * @param {*} target
 * @param {*} sample
 * @param {{capacity?:number, all?:boolean}} [options]
 * @returns {*}
 */
export function appendSample(target, sample, options = {}) {
  const settings = typeof options === 'number' ? { capacity: options } : (options || {});
  const requested = firstDefined(settings.capacity, target?.capacity);
  const capacity = seriesCapacity({ capacity: requested }, DEFAULT_SAMPLES);

  if (Array.isArray(target) && target.length && isSeriesDescriptor(target[0])) {
    const maxSeries = boundedInteger(settings.maxSeries, 1, MAX_SERIES, MAX_SERIES);
    return target.slice(0, maxSeries).map((item, index) => {
      let chosen = sample;
      if (Array.isArray(sample)) {
        chosen = sample[index];
      } else if (isPlainObject(sample) && !isPointLike(sample)) {
        const keys = [item.id, item.key, item.label, item.name].filter(
          (key) => key !== undefined && key !== null && key !== '',
        );
        const found = keys.find((key) => hasOwn(sample, key));
        if (found !== undefined) chosen = sample[found];
        else if (hasOwn(sample, index)) chosen = sample[index];
        else chosen = null;
      }
      return appendSample(item, chosen, { ...settings, capacity, maxSeries });
    });
  }

  if (isPlainObject(target) && !ArrayBuffer.isView(target)) {
    const rawValues = ringValues(target);
    const sampleValue = readSample(sample);
    const previousTimes = Array.isArray(target.times)
      ? target.times.slice()
      : rawValues.map(() => null);
    const nextValues = rawValues.concat([sampleValue.value]).slice(-capacity);
    const nextTimesRaw = previousTimes.concat([sampleValue.time]);
    const nextTimes = nextTimesRaw.slice(-capacity);
    const keys = ['values', 'samples', 'history', 'data', 'points'];
    const key = keys.find((name) => hasOwn(target, name)) || 'values';
    const next = { ...target, capacity, [key]: nextValues };

    if (hasOwn(target, 'samples') && key !== 'samples') next.samples = nextValues;
    if (hasOwn(target, 'values') && key !== 'values') next.values = nextValues;
    if (Array.isArray(target.times) || sampleValue.time !== null) next.times = nextTimes;
    return next;
  }

  const rawValues = ringValues(target);
  const sampleValue = readSample(sample);
  return rawValues.concat([sampleValue.value]).slice(-capacity);
}

/* ------------------------------------------------------------------------- */
/* Scales and ticks                                                           */
/* ------------------------------------------------------------------------- */

/**
 * Map `value` from a domain to a range.  Array arguments are also accepted:
 * `scaleValue(value, [min, max], [rangeMin, rangeMax])`.
 */
export function scaleValue(value, min, max, rangeMin = 0, rangeMax = 1) {
  let domainMin = min;
  let domainMax = max;
  let outMin = rangeMin;
  let outMax = rangeMax;
  if (Array.isArray(min)) {
    domainMin = min[0];
    domainMax = min[1];
    if (Array.isArray(max)) {
      outMin = max[0];
      outMax = max[1];
    }
  }
  const number = Number(value);
  const lo = Number(domainMin);
  const hi = Number(domainMax);
  const rLo = Number(outMin);
  const rHi = Number(outMax);
  if (!Number.isFinite(number) || !Number.isFinite(lo) || !Number.isFinite(hi)) return rLo;
  if (!Number.isFinite(rLo) || !Number.isFinite(rHi)) return 0;
  if (hi === lo) return rLo + (rHi - rLo) / 2;
  const ratio = (number - lo) / (hi - lo);
  return rLo + ratio * (rHi - rLo);
}

function niceStep(value) {
  const raw = Math.abs(Number(value));
  if (!Number.isFinite(raw) || raw <= 0) return 1;
  const exponent = Math.floor(Math.log10(raw));
  const fraction = raw / (10 ** exponent);
  let nice = 10;
  if (fraction <= 1) nice = 1;
  else if (fraction <= 2) nice = 2;
  else if (fraction <= 5) nice = 5;
  return nice * (10 ** exponent);
}

/**
 * Produce a bounded list of visually "nice" tick values inside `[min, max]`.
 * Handles negative, zero-width, and non-finite domains safely.
 */
export function niceTicks(min, max, count = 5) {
  const lower = finite(min, 0);
  const upper = finite(max, 1);
  const ticks = boundedInteger(count, 2, MAX_TICKS, 5);
  if (upper === lower) return [lower];
  const lo = Math.min(lower, upper);
  const hi = Math.max(lower, upper);
  const step = niceStep((hi - lo) / (ticks - 1));
  const values = [];
  const start = Math.ceil(lo / step) * step;
  for (let value = start; value <= hi + step * 1e-9 && values.length <= MAX_TICKS; value += step) {
    const rounded = Math.round(value * 1e9) / 1e9;
    if (!values.length || rounded !== values[values.length - 1]) values.push(rounded);
  }
  if (!values.length) values.push(lo, hi);
  return values.slice(0, MAX_TICKS);
}

const TIME_STEPS = Object.freeze([
  1000,
  2000,
  5000,
  10_000,
  15_000,
  30_000,
  60_000,
  2 * 60_000,
  5 * 60_000,
  10 * 60_000,
  15 * 60_000,
  30 * 60_000,
  3_600_000,
  2 * 3_600_000,
  3 * 3_600_000,
  6 * 3_600_000,
  12 * 3_600_000,
  86_400_000,
  2 * 86_400_000,
  7 * 86_400_000,
  14 * 86_400_000,
  30 * 86_400_000,
  90 * 86_400_000,
  365 * 86_400_000,
]);

/**
 * Produce aligned time ticks between two timestamps (milliseconds).
 */
export function timeTicks(start, end, count = 5) {
  const from = finite(start, 0);
  const to = finite(end, from + 1);
  const ticks = boundedInteger(count, 2, MAX_TICKS, 5);
  if (to === from) return [from];
  const lo = Math.min(from, to);
  const hi = Math.max(from, to);
  const span = hi - lo;
  const target = span / (ticks - 1);
  const step = TIME_STEPS.find((candidate) => candidate >= target) ?? TIME_STEPS[TIME_STEPS.length - 1];
  const values = [];
  const first = Math.ceil(lo / step) * step;
  for (let value = first; value <= hi + step * 1e-9 && values.length <= MAX_TICKS; value += step) {
    values.push(value);
  }
  if (!values.length) values.push(lo, hi);
  return values.slice(0, MAX_TICKS);
}

/* ------------------------------------------------------------------------- */
/* Configuration and colours                                                  */
/* ------------------------------------------------------------------------- */

function normalizePadding(value) {
  const fallback = { top: 22, right: 16, bottom: 30, left: 46 };
  if (typeof value === 'number' && Number.isFinite(value) && value >= 0) {
    return { top: value, right: value, bottom: value, left: value };
  }
  if (!isPlainObject(value)) return fallback;
  return {
    top: Math.max(0, finite(value.top, fallback.top)),
    right: Math.max(0, finite(value.right, fallback.right)),
    bottom: Math.max(0, finite(value.bottom, fallback.bottom)),
    left: Math.max(0, finite(value.left, fallback.left)),
  };
}

/**
 * Normalise chart configuration.  Unknown fields are retained so callers can
 * pass through widget-specific metadata.
 */
export function normalizeChartConfig(config = {}) {
  const width = clamp(
    positive(firstDefined(config.width, config.w), DEFAULT_WIDTH),
    16,
    MAX_DIMENSION,
  );
  const height = clamp(
    positive(firstDefined(config.height, config.h), DEFAULT_HEIGHT),
    16,
    MAX_DIMENSION,
  );
  const area = config.area === true
    || config.areaFill === true
    || config.area_fill === true
    || config.fill === true
    || typeof config.fill === 'string';
  const style = normaliseStyle(
    firstDefined(config.style, config.mark, config.chartMark, config.chart_mark, config.chart),
    area,
  );
  const windowMs = finite(
    firstDefined(config.timeWindow, config.time_window, config.windowMs, config.window_ms),
    null,
  );

  return {
    ...config,
    width,
    height,
    min: finite(firstDefined(config.min, config.yMin, config.y_min), null),
    max: finite(firstDefined(config.max, config.yMax, config.y_max), null),
    auto: config.auto === true,
    xMin: finite(firstDefined(config.xMin, config.x_min, config.timeMin, config.time_min), null),
    xMax: finite(firstDefined(config.xMax, config.x_max, config.timeMax, config.time_max), null),
    timeWindow: windowMs !== null && windowMs > 0 ? windowMs : null,
    sampleMs: positive(firstDefined(config.sampleMs, config.sample_ms, config.intervalMs), 1000),
    padding: normalizePadding(config.padding),
    style,
    area: area || style === 'area' || style === 'area-step',
    step: style === 'step' || style === 'area-step',
    lineWidth: positive(
      firstDefined(config.lineWidth, config.line_width, config.strokeWidth, config.stroke_width),
      2,
    ),
    areaOpacity: clamp(
      finite(firstDefined(config.areaOpacity, config.fillOpacity, config.fill_opacity), 0.16),
      0,
      1,
    ),
    tickCount: boundedInteger(
      firstDefined(config.yTicks, config.ticks, config.tickCount, config.tick_count),
      2,
      MAX_TICKS,
      5,
    ),
    xTickCount: boundedInteger(
      firstDefined(config.xTicks, config.x_ticks, config.timeTicks, config.time_ticks),
      2,
      MAX_TICKS,
      5,
    ),
    decimals: Number.isInteger(config.decimals)
      ? clampInteger(config.decimals, 0, 6, 2)
      : undefined,
    unit: safeLabel(firstDefined(config.unit, ''), ''),
    label: safeLabel(firstDefined(config.label, config.title, config.name, ''), ''),
    xLabel: safeLabel(firstDefined(config.xLabel, config.x_label, config.timeLabel), ''),
    yLabel: safeLabel(firstDefined(config.yLabel, config.y_label, config.valueLabel), ''),
    showAxes: config.showAxes !== false && config.show_axes !== false,
    showGrid: config.showGrid !== false && config.show_grid !== false,
    showLegend: config.showLegend !== false && config.show_legend !== false,
    showLast: config.showLast !== false
      && config.show_last !== false
      && config.lastValue !== false
      && config.last_value !== false,
    showBands: config.showBands !== false && config.show_bands !== false,
    showThresholds: config.showThresholds !== false && config.show_thresholds !== false,
    showValue: config.showValue === true || config.show_value === true,
    tooltip: config.tooltip !== false,
    baseline: firstDefined(config.baseline, config.zeroLine, config.zero_line, 'zero'),
    animate: Boolean(firstDefined(config.animate, config.animated, false)),
    warn: finite(firstDefined(config.warn, config.warn_at, config.warnAt), null),
    crit: finite(
      firstDefined(config.crit, config.critical, config.crit_at, config.critAt),
      null,
    ),
    thresholds: config.thresholds,
    bands: config.bands,
    colors: config.colors,
    palette: config.palette,
    capacity: seriesCapacity(config, DEFAULT_SAMPLES),
  };
}

function colourList(value, tokens, fallback = DEFAULT_PALETTE) {
  let list = value;
  if (typeof list === 'string') list = list.split(',');
  if (isPlainObject(list)) {
    list = list.series || list.colors || list.values || list.palette;
  }
  if (!Array.isArray(list) || !list.length) list = fallback;
  return list.slice(0, MAX_SERIES).map((colour, index) => safeChartColour(
    resolveColour(unwrap(colour), tokens),
    fallback[index % fallback.length],
  ));
}

/**
 * Resolve chart colours from explicit config, theme tokens, then defaults.
 */
export function resolveChartColors(config = {}, tokens = {}) {
  const cfg = normalizeChartConfig(config);
  const explicit = isPlainObject(cfg.colors) ? cfg.colors : {};
  const paletteSource = firstDefined(
    explicit.palette,
    cfg.palette,
    tokenValue(tokens, ['chart.palette', 'palette.series', 'chart.colors']),
  );
  const palette = colourList(paletteSource, tokens, DEFAULT_PALETTE);
  const line = safeChartColour(
    resolveColour(firstDefined(
      explicit.line,
      explicit.stroke,
      cfg.lineColor,
      cfg.line_color,
      cfg.color,
      tokenValue(tokens, ['chart.line', 'sparkline.line', 'gauge.fill', 'color.accent']),
    ), tokens),
    palette[0],
  );
  const seriesPalette = paletteSource === undefined
    ? [line, ...palette.slice(1)]
    : palette;

  return {
    palette: seriesPalette,
    line,
    fill: safeChartColour(
      resolveColour(firstDefined(
        explicit.fill,
        typeof cfg.fill === 'string' ? cfg.fill : undefined,
        tokenValue(tokens, ['chart.fill', 'chart.area', 'sparkline.fill', 'color.accent']),
      ), tokens),
      line,
    ),
    warn: safeChartColour(
      resolveColour(firstDefined(
        explicit.warn,
        cfg.warnColor,
        tokenValue(tokens, ['chart.warn', 'sparkline.warn', 'gauge.warn', 'color.warn']),
      ), tokens),
      DEFAULT_COLORS.warn,
    ),
    crit: safeChartColour(
      resolveColour(firstDefined(
        explicit.crit,
        explicit.critical,
        cfg.critColor,
        tokenValue(tokens, ['chart.crit', 'sparkline.crit', 'gauge.crit', 'color.crit', 'color.error']),
      ), tokens),
      DEFAULT_COLORS.crit,
    ),
    grid: safeChartColour(
      resolveColour(firstDefined(
        explicit.grid,
        tokenValue(tokens, ['chart.grid', 'sparkline.grid', 'color.grid', 'color.track']),
      ), tokens),
      DEFAULT_COLORS.grid,
    ),
    axis: safeChartColour(
      resolveColour(firstDefined(
        explicit.axis,
        tokenValue(tokens, ['chart.axis', 'chart.grid', 'color.border', 'color.dim']),
      ), tokens),
      DEFAULT_COLORS.axis,
    ),
    baseline: safeChartColour(
      resolveColour(firstDefined(
        explicit.baseline,
        tokenValue(tokens, ['chart.baseline', 'sparkline.baseline', 'color.dim', 'color.muted']),
      ), tokens),
      DEFAULT_COLORS.baseline,
    ),
    text: safeChartColour(
      resolveColour(firstDefined(
        explicit.text,
        tokenValue(tokens, ['chart.text', 'sparkline.text', 'gauge.text', 'color.fg', 'color.text']),
      ), tokens),
      DEFAULT_COLORS.text,
    ),
    muted: safeChartColour(
      resolveColour(firstDefined(
        explicit.muted,
        tokenValue(tokens, ['chart.muted', 'sparkline.muted', 'color.muted', 'color.dim']),
      ), tokens),
      DEFAULT_COLORS.muted,
    ),
    background: safeChartColour(
      resolveColour(firstDefined(
        explicit.background,
        tokenValue(tokens, ['chart.background', 'color.panel', 'color.bg']),
      ), tokens),
      DEFAULT_COLORS.background,
    ),
  };
}

function severityFor(value, config = {}) {
  const number = Number(value);
  if (!Number.isFinite(number)) return 'normal';
  const crit = finite(firstDefined(config.crit, config.critical, config.crit_at, config.critAt), null);
  const warn = finite(firstDefined(config.warn, config.warn_at, config.warnAt), null);
  if (crit !== null && number >= crit) return 'crit';
  if (warn !== null && number >= warn) return 'warn';
  return 'normal';
}

function severityColour(severity, colours) {
  if (severity === 'crit') return colours.crit;
  if (severity === 'warn') return colours.warn;
  return colours.line;
}

function resolveThresholds(config, colors) {
  const entries = [];
  const added = new Set();
  const push = (value, severity, colour, label) => {
    const number = finite(value, null);
    if (number === null) return;
    const level = severity === 'crit' ? 'crit' : 'warn';
    const key = `${number}|${level}`;
    if (added.has(key)) return;
    added.add(key);
    entries.push({
      value: number,
      severity: level,
      color: safeChartColour(colour, severityColour(level, colors)),
      label: safeLabel(label, `${level === 'crit' ? 'Critical' : 'Warning'} ${numberText(number)}`),
    });
  };

  push(firstDefined(config.warn, config.warn_at, config.warnAt), 'warn');
  push(
    firstDefined(config.crit, config.critical, config.crit_at, config.critAt),
    'crit',
  );

  const raw = config.thresholds;
  if (Array.isArray(raw)) {
    for (const item of raw.slice(0, MAX_TICKS * 2)) {
      if (isPlainObject(item)) {
        push(
          firstDefined(item.value, item.at, item.level_at, item.threshold),
          firstDefined(item.severity, item.level, item.kind),
          item.color,
          item.label,
        );
      } else {
        push(item, 'warn');
      }
    }
  } else if (isPlainObject(raw)) {
    for (const [key, value] of Object.entries(raw).slice(0, MAX_TICKS * 2)) {
      if (isPlainObject(value)) {
        push(
          firstDefined(value.value, value.at, key),
          firstDefined(value.severity, value.level, /crit/i.test(key) ? 'crit' : 'warn'),
          value.color,
          value.label,
        );
      } else {
        push(value, /crit/i.test(key) ? 'crit' : 'warn');
      }
    }
  }

  return entries;
}

function resolveBands(config, colors, thresholds) {
  const bands = [];
  if (Array.isArray(config.bands) && config.bands.length) {
    for (const item of config.bands.slice(0, MAX_TICKS * 2)) {
      if (!isPlainObject(item)) continue;
      const from = finite(firstDefined(item.from, item.start, item.value, item.min), null);
      const to = finite(firstDefined(item.to, item.end, item.max), null);
      if (from === null || to === null) continue;
      const severity = /crit/i.test(String(item.severity || item.level || '')) ? 'crit' : 'warn';
      bands.push({
        from: Math.min(from, to),
        to: Math.max(from, to),
        severity,
        color: safeChartColour(item.color, severityColour(severity, colors)),
        opacity: clamp(finite(item.opacity, 0.08), 0, 1),
        label: safeLabel(item.label, ''),
      });
    }
    return bands;
  }
  if (config.showBands === false) return bands;

  const warn = thresholds.find((entry) => entry.severity === 'warn');
  const crit = thresholds.find((entry) => entry.severity === 'crit');
  if (warn) {
    bands.push({
      from: warn.value,
      to: crit ? Math.max(warn.value, crit.value) : null,
      severity: 'warn',
      color: colors.warn,
      opacity: 0.08,
      label: warn.label,
    });
  }
  if (crit) {
    bands.push({
      from: crit.value,
      to: null,
      severity: 'crit',
      color: colors.crit,
      opacity: 0.1,
      label: crit.label,
    });
  }
  return bands;
}

/* ------------------------------------------------------------------------- */
/* Geometry                                                                   */
/* ------------------------------------------------------------------------- */

function baselineValue(value, min, max) {
  if (value === undefined || value === null || value === false) return null;
  if (value === true || value === 'min') return min;
  if (value === 'max') return max;
  if (value === 'zero' || value === 'origin') return clamp(0, min, max);
  const number = Number(value);
  return Number.isFinite(number) ? clamp(number, min, max) : min;
}

function formatNumber(value, config = {}) {
  const number = Number(value);
  if (!Number.isFinite(number)) return '—';
  if (Number.isInteger(config.decimals)) {
    return number.toFixed(clampInteger(config.decimals, 0, 6, 0));
  }
  if (Number.isInteger(number)) return String(number);
  const absolute = Math.abs(number);
  if (absolute !== 0 && (absolute >= 1000 || absolute < 0.01)) {
    return number.toPrecision(3).replace(/0+$/, '').replace(/\.$/, '');
  }
  return number.toFixed(2).replace(/0+$/, '').replace(/\.$/, '');
}

function formatTimeTick(value, span) {
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return formatNumber(value);
  const pad = (number) => String(number).padStart(2, '0');
  if (span <= 90_000) {
    return `${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())}:${pad(date.getUTCSeconds())}`;
  }
  if (span <= 2 * 86_400_000) {
    return `${pad(date.getUTCMonth() + 1)}-${pad(date.getUTCDate())} ${pad(date.getUTCHours())}:${pad(date.getUTCMinutes())}`;
  }
  return `${date.getUTCFullYear()}-${pad(date.getUTCMonth() + 1)}-${pad(date.getUTCDate())}`;
}

function mergeDescriptor(series, descriptor) {
  if (descriptor === undefined || descriptor === null) return series;
  if (typeof descriptor === 'string' || typeof descriptor === 'number') {
    return { ...series, label: safeLabel(String(descriptor), series.label) };
  }
  if (!isPlainObject(descriptor)) return series;
  return {
    ...series,
    label: safeLabel(firstDefined(descriptor.label, descriptor.name, descriptor.title, series.label), series.label),
    color: safeChartColour(
      firstDefined(descriptor.color, descriptor.colour, descriptor.stroke, series.color),
      series.color,
    ),
    unit: safeLabel(firstDefined(descriptor.unit, series.unit, ''), ''),
    style: normaliseStyle(
      firstDefined(descriptor.style, descriptor.mark, series.style),
      Boolean(firstDefined(descriptor.area, descriptor.fill, series.area)),
    ),
    area: Boolean(firstDefined(descriptor.area, descriptor.fill, series.area)),
  };
}

/**
 * Build chart geometry: viewport, ranges, series points, gap-aware segments,
 * ticks, thresholds, severity bands, baseline, legend, and colours.
 */
export function chartGeometry(config = {}, series = [], tokens = {}) {
  const cfg = normalizeChartConfig(config);
  const colors = resolveChartColors(cfg, tokens);
  const descriptors = Array.isArray(cfg.series) ? cfg.series : [];
  const normalized = normalizeSeries(series, {
    capacity: cfg.capacity,
    maxSeries: MAX_SERIES,
    palette: colors.palette,
  }).map((item, index) => {
    const merged = mergeDescriptor(item, descriptors[index]);
    return { ...merged, unit: merged.unit || cfg.unit };
  });

  const allTimes = [];
  let maxLength = 0;
  for (const item of normalized) {
    maxLength = Math.max(maxLength, item.values.length);
    if (Array.isArray(item.times)) {
      for (const time of item.times) {
        if (time !== null) allTimes.push(time);
      }
    }
  }

  let xMin;
  let xMax;
  let xIsTime = false;
  if (cfg.timeWindow !== null) {
    xIsTime = true;
    const availableMax = allTimes.length
      ? Math.max(...allTimes)
      : Math.max(0, maxLength - 1) * cfg.sampleMs;
    xMax = cfg.xMax ?? availableMax;
    xMin = cfg.xMin ?? (xMax - cfg.timeWindow);
  } else if (allTimes.length) {
    xIsTime = true;
    xMin = cfg.xMin ?? Math.min(...allTimes);
    xMax = cfg.xMax ?? Math.max(...allTimes);
  } else {
    xMin = cfg.xMin ?? 0;
    xMax = cfg.xMax ?? Math.max(1, maxLength - 1);
  }
  if (!Number.isFinite(xMin)) xMin = 0;
  if (!Number.isFinite(xMax)) xMax = xMin + 1;
  if (xMax <= xMin) xMax = xMin + (xIsTime ? Math.max(1, cfg.sampleMs) : 1);

  const defined = [];
  for (const item of normalized) {
    for (const value of item.values) {
      if (value !== null) defined.push(value);
    }
  }
  const autoMin = defined.length ? Math.min(...defined) : 0;
  const autoMax = defined.length ? Math.max(...defined) : 1;
  let min = cfg.auto || !Number.isFinite(cfg.min) ? autoMin : cfg.min;
  let max = cfg.auto || !Number.isFinite(cfg.max) ? autoMax : cfg.max;
  if (!Number.isFinite(min)) min = 0;
  if (!Number.isFinite(max)) max = 1;
  if (max <= min) {
    max = min === 0 ? 1 : min + Math.max(Math.abs(min) * 0.1, 1);
  }

  const { width, height, padding } = cfg;
  const left = Math.min(padding.left, width - 1);
  const right = Math.max(left + 1, width - padding.right);
  const top = Math.min(padding.top, height - 1);
  const bottom = Math.max(top + 1, height - padding.bottom);
  const plotWidth = Math.max(1, right - left);
  const plotHeight = Math.max(1, bottom - top);

  const xScale = (value) => scaleValue(value, xMin, xMax, left, right);
  const yScale = (value) => scaleValue(value, min, max, bottom, top);

  const seriesGeometry = normalized.map((item) => {
    const points = item.values.map((value, index) => {
      const time = Array.isArray(item.times) ? item.times[index] : null;
      const xValue = xIsTime
        ? (time ?? (
          maxLength <= 1
            ? xMin
            : xMin + (index / Math.max(1, maxLength - 1)) * (xMax - xMin)
        ))
        : index;
      const x = xScale(xValue);
      if (value === null) {
        return {
          index,
          value: null,
          clamped: null,
          defined: false,
          x,
          y: null,
          xValue,
          time,
          severity: 'normal',
        };
      }
      const clamped = cfg.clampValues === false ? value : clamp(value, min, max);
      return {
        index,
        value,
        clamped,
        defined: true,
        x,
        y: yScale(clamped),
        xValue,
        time,
        severity: severityFor(clamped, cfg),
      };
    });

    const segments = [];
    let current = [];
    for (const point of points) {
      if (point.defined) {
        current.push(point);
      } else if (current.length) {
        segments.push(current);
        current = [];
      }
    }
    if (current.length) segments.push(current);
    const definedPoints = points.filter((point) => point.defined);
    const latest = definedPoints.length ? definedPoints[definedPoints.length - 1] : null;

    return {
      ...item,
      points,
      segments,
      definedPoints,
      lastPoint: latest,
      last: latest ? latest.value : null,
      definedCount: definedPoints.length,
      gapCount: points.length - definedPoints.length,
    };
  });

  const thresholds = resolveThresholds(cfg, colors).map((entry) => ({
    ...entry,
    y: yScale(clamp(entry.value, min, max)),
  }));
  const bands = resolveBands(cfg, colors, thresholds)
    .map((band) => {
      const from = clamp(band.from, min, max);
      const to = clamp(band.to === null ? max : band.to, min, max);
      const yA = yScale(from);
      const yB = yScale(to);
      return {
        ...band,
        from,
        to: band.to === null ? max : to,
        y: Math.min(yA, yB),
        height: Math.max(1, Math.abs(yB - yA)),
      };
    })
    .filter((band) => band.height > 0);

  const baseline = baselineValue(cfg.baseline, min, max);
  const baselineY = baseline === null ? null : yScale(baseline);
  const zeroY = min <= 0 && max >= 0 ? yScale(0) : null;

  const yTicks = niceTicks(min, max, cfg.tickCount)
    .filter((value) => value >= min - 1e-9 && value <= max + 1e-9)
    .map((value) => ({ value, y: yScale(value), label: formatNumber(value, cfg) }));

  const xValues = xIsTime
    ? timeTicks(xMin, xMax, cfg.xTickCount)
    : niceTicks(xMin, xMax, cfg.xTickCount);
  const xTicks = xValues.map((value) => ({
    value,
    x: xScale(value),
    label: xIsTime ? formatTimeTick(value, xMax - xMin) : formatNumber(value, cfg),
  }));

  const legend = seriesGeometry.map((item) => ({
    id: item.id,
    label: item.label,
    color: item.color,
    unit: item.unit,
    last: item.last,
    style: item.style,
  }));

  return {
    ...cfg,
    series: seriesGeometry,
    descriptors,
    min,
    max,
    autoMin,
    autoMax,
    xMin,
    xMax,
    xIsTime,
    width,
    height,
    left,
    right,
    top,
    bottom,
    plotWidth,
    plotHeight,
    xScale,
    yScale,
    yTicks,
    xTicks,
    thresholds,
    bands,
    baseline,
    baselineY,
    zeroY,
    legend,
    colors,
    definedCount: defined.length,
    gapCount: seriesGeometry.reduce((total, item) => total + item.gapCount, 0),
    maxLength,
  };
}

function pointsToPath(points, options = {}) {
  const clean = (Array.isArray(points) ? points : []).filter(
    (point) => point && Number.isFinite(point.x) && Number.isFinite(point.y),
  );
  if (!clean.length) return '';
  const parts = [`M ${numberText(clean[0].x)} ${numberText(clean[0].y)}`];
  for (let index = 1; index < clean.length; index += 1) {
    const point = clean[index];
    const previous = clean[index - 1];
    if (options.step) parts.push(`L ${numberText(point.x)} ${numberText(previous.y)}`);
    parts.push(`L ${numberText(point.x)} ${numberText(point.y)}`);
  }
  return parts.join(' ');
}

function chartSegments(input, options = {}) {
  if (!input) return [];
  if (Array.isArray(input)) {
    if (!input.length) return [];
    if (Array.isArray(input[0])) return input.filter(Array.isArray);
    if (input[0] && Array.isArray(input[0].segments)) {
      return input.flatMap((item) => item.segments || []);
    }
    if (input[0] && Array.isArray(input[0].points)) {
      return input.flatMap((item) => item.segments || (item.points ? [item.points] : []));
    }
    return [input];
  }
  if (Array.isArray(input.series)) {
    let list = input.series;
    if (options.series !== undefined) {
      if (typeof options.series === 'number') list = [list[options.series]].filter(Boolean);
      else list = list.filter((item) => item.id === options.series || item.label === options.series);
    }
    return list.flatMap((item) => item.segments || (item.points ? [item.points] : []));
  }
  if (Array.isArray(input.segments)) return input.segments;
  if (Array.isArray(input.points)) return [input.points];
  return [];
}

/**
 * Build one SVG path containing every segment of every selected series.
 * Gaps remain separate subpaths.  Pass `{step:true}` or `{series:id|index}` to
 * select a series or use step interpolation.
 */
export function chartPath(geometryOrSeries, options = {}) {
  const settings = typeof options === 'boolean'
    ? { step: options }
    : options === 'step'
      ? { step: true }
      : (options || {});
  const step = settings.step
    ?? geometryOrSeries?.step
    ?? geometryOrSeries?.style === 'step'
    ?? geometryOrSeries?.style === 'area-step';
  return chartSegments(geometryOrSeries, settings)
    .map((segment) => pointsToPath(segment, { step }))
    .filter(Boolean)
    .join(' ');
}

/**
 * Build an area path for the selected series, closing each segment against a
 * baseline.  `options.baseline` is a y coordinate; otherwise the geometry's
 * baseline or plot bottom is used.
 */
export function chartAreaPath(geometryOrSeries, options = {}) {
  const settings = typeof options === 'number' ? { baseline: options } : (options || {});
  const segments = chartSegments(geometryOrSeries, settings);
  const baseline = Number.isFinite(settings.baseline)
    ? settings.baseline
    : Number.isFinite(geometryOrSeries?.baselineY)
      ? geometryOrSeries.baselineY
      : Number.isFinite(geometryOrSeries?.bottom)
        ? geometryOrSeries.bottom
        : 0;

  return segments.map((segment) => {
    const clean = segment.filter(
      (point) => point && Number.isFinite(point.x) && Number.isFinite(point.y),
    );
    if (!clean.length) return '';
    const parts = [
      `M ${numberText(clean[0].x)} ${numberText(baseline)}`,
      `L ${numberText(clean[0].x)} ${numberText(clean[0].y)}`,
    ];
    for (let index = 1; index < clean.length; index += 1) {
      parts.push(`L ${numberText(clean[index].x)} ${numberText(clean[index].y)}`);
    }
    parts.push(`L ${numberText(clean[clean.length - 1].x)} ${numberText(baseline)} Z`);
    return parts.join(' ');
  }).filter(Boolean).join(' ');
}

/* ------------------------------------------------------------------------- */
/* Render tree                                                                */
/* ------------------------------------------------------------------------- */

function detectReducedMotion(config = {}, options = {}) {
  if (typeof options.reducedMotion === 'boolean') return options.reducedMotion;
  if (typeof config.reducedMotion === 'boolean') return config.reducedMotion;
  if (typeof globalThis.matchMedia === 'function') {
    try {
      return Boolean(globalThis.matchMedia('(prefers-reduced-motion: reduce)').matches);
    } catch {
      return false;
    }
  }
  return false;
}

function node(tag, attrs = {}, children = [], text = undefined) {
  return { tag, attrs, children, text };
}

function textNode(attrs, text) {
  return node('text', attrs, [], text);
}

function chartAriaLabel(geometry, hasData) {
  const title = geometry.label || 'Chart';
  if (!hasData) return `${title}: no data`;
  const parts = geometry.series.slice(0, 4).map((item) => {
    const value = item.last === null ? 'no data' : `${formatNumber(item.last, geometry)}${item.unit ? ` ${item.unit}` : ''}`;
    return `${item.label}: ${value}`;
  });
  if (geometry.series.length > 4) parts.push(`and ${geometry.series.length - 4} more`);
  return `${title}: ${parts.join(', ')}`;
}

function chartDescription(geometry, hasData) {
  const title = geometry.label || 'Chart';
  if (!hasData) return `${title}: no data.`;
  const sampleCount = geometry.maxLength;
  const range = `range ${formatNumber(geometry.min, geometry)} to ${formatNumber(geometry.max, geometry)}${geometry.unit ? ` ${geometry.unit}` : ''}`;
  const seriesText = `${geometry.series.length} series, ${sampleCount} sample${sampleCount === 1 ? '' : 's'}`;
  const gapText = geometry.gapCount
    ? `; ${geometry.gapCount} missing sample${geometry.gapCount === 1 ? '' : 's'}`
    : '';
  const thresholdText = geometry.thresholds.length
    ? `; thresholds ${geometry.thresholds.map((item) => `${item.label} ${formatNumber(item.value, geometry)}`).join(', ')}`
    : '';
  return `${title}: ${seriesText}; ${range}${gapText}${thresholdText}.`;
}

function buildChartTree(config = {}, series = [], tokens = {}, options = {}) {
  const geometry = chartGeometry(config, series, tokens);
  const reducedMotion = detectReducedMotion(geometry, options);
  const fontFamily = tokenString(tokens, ['font.family', 'fontFamily', 'typography.family'], 'system-ui, sans-serif');
  const hasData = geometry.definedCount > 0;
  const title = geometry.label || 'Chart';
  const uid = idSafe(options.id ?? geometry.id ?? `chart-${hashText(`${title}|${geometry.min}|${geometry.max}|${geometry.width}x${geometry.height}`)}`);
  const ariaLabel = chartAriaLabel(geometry, hasData);
  const description = chartDescription(geometry, hasData);
  const palette = geometry.colors.palette;
  const className = [
    'ozma-chart',
    `ozma-chart-style-${geometry.style || 'line'}`,
    reducedMotion ? 'ozma-chart-reduced-motion' : '',
  ].filter(Boolean).join(' ');

  const children = [
    node('title', { id: `${uid}-title`, class: 'ozma-chart-title' }, [], title),
    node('desc', { id: `${uid}-desc`, class: 'ozma-chart-desc' }, [], description),
  ];
  const content = [];

  const bandNodes = geometry.bands.map((band) => node('rect', {
    class: 'ozma-chart-band',
    x: numberText(geometry.left),
    y: numberText(band.y),
    width: numberText(geometry.plotWidth),
    height: numberText(band.height),
    fill: band.color,
    'fill-opacity': numberText(band.opacity),
    'data-severity': band.severity,
  }));
  if (bandNodes.length) content.push(node('g', { class: 'ozma-chart-bands' }, bandNodes));

  const gridNodes = [];
  if (geometry.showGrid) {
    for (const tick of geometry.yTicks) {
      gridNodes.push(node('line', {
        class: 'ozma-chart-grid-line',
        x1: numberText(geometry.left),
        y1: numberText(tick.y),
        x2: numberText(geometry.right),
        y2: numberText(tick.y),
        stroke: geometry.colors.grid,
        'stroke-width': 1,
        'stroke-opacity': 0.7,
      }));
    }
    for (const tick of geometry.xTicks) {
      gridNodes.push(node('line', {
        class: 'ozma-chart-grid-line',
        x1: numberText(tick.x),
        y1: numberText(geometry.top),
        x2: numberText(tick.x),
        y2: numberText(geometry.bottom),
        stroke: geometry.colors.grid,
        'stroke-width': 1,
        'stroke-opacity': 0.55,
      }));
    }
  }
  if (gridNodes.length) content.push(node('g', { class: 'ozma-chart-grid' }, gridNodes));

  const axisNodes = [];
  if (geometry.showAxes) {
    axisNodes.push(node('line', {
      class: 'ozma-chart-axis-line',
      x1: numberText(geometry.left),
      y1: numberText(geometry.top),
      x2: numberText(geometry.left),
      y2: numberText(geometry.bottom),
      stroke: geometry.colors.axis,
      'stroke-width': 1,
    }));
    axisNodes.push(node('line', {
      class: 'ozma-chart-axis-line',
      x1: numberText(geometry.left),
      y1: numberText(geometry.bottom),
      x2: numberText(geometry.right),
      y2: numberText(geometry.bottom),
      stroke: geometry.colors.axis,
      'stroke-width': 1,
    }));
    for (const tick of geometry.yTicks) {
      axisNodes.push(textNode({
        class: 'ozma-chart-axis-label ozma-chart-y-label',
        x: numberText(geometry.left - 6),
        y: numberText(tick.y + 3),
        'text-anchor': 'end',
        fill: geometry.colors.muted,
        'font-family': fontFamily,
        'font-size': 9,
      }, tick.label));
    }
    for (const tick of geometry.xTicks) {
      axisNodes.push(textNode({
        class: 'ozma-chart-axis-label ozma-chart-x-label',
        x: numberText(tick.x),
        y: numberText(geometry.bottom + 14),
        'text-anchor': 'middle',
        fill: geometry.colors.muted,
        'font-family': fontFamily,
        'font-size': 9,
      }, tick.label));
    }
    if (geometry.xLabel) {
      axisNodes.push(textNode({
        class: 'ozma-chart-axis-title ozma-chart-x-title',
        x: numberText((geometry.left + geometry.right) / 2),
        y: numberText(geometry.height - 3),
        'text-anchor': 'middle',
        fill: geometry.colors.muted,
        'font-family': fontFamily,
        'font-size': 9,
      }, geometry.xLabel));
    }
    if (geometry.yLabel) {
      axisNodes.push(textNode({
        class: 'ozma-chart-axis-title ozma-chart-y-title',
        x: 10,
        y: numberText((geometry.top + geometry.bottom) / 2),
        'text-anchor': 'middle',
        fill: geometry.colors.muted,
        'font-family': fontFamily,
        'font-size': 9,
        transform: `rotate(-90 10 ${numberText((geometry.top + geometry.bottom) / 2)})`,
      }, geometry.yLabel));
    }
  }
  if (axisNodes.length) content.push(node('g', { class: 'ozma-chart-axes' }, axisNodes));

  if (geometry.showThresholds && geometry.thresholds.length) {
    const thresholdNodes = [];
    for (const threshold of geometry.thresholds) {
      thresholdNodes.push(node('line', {
        class: 'ozma-chart-threshold',
        x1: numberText(geometry.left),
        y1: numberText(threshold.y),
        x2: numberText(geometry.right),
        y2: numberText(threshold.y),
        stroke: threshold.color,
        'stroke-width': 1,
        'stroke-opacity': 0.65,
        'stroke-dasharray': '3 4',
        'data-severity': threshold.severity,
      }));
      if (geometry.showAxes) {
        thresholdNodes.push(textNode({
          class: 'ozma-chart-threshold-label',
          x: numberText(geometry.right - 2),
          y: numberText(Math.max(9, threshold.y - 3)),
          'text-anchor': 'end',
          fill: threshold.color,
          'font-family': fontFamily,
          'font-size': 8,
        }, threshold.label));
      }
    }
    content.push(node('g', { class: 'ozma-chart-thresholds' }, thresholdNodes));
  }

  const seriesNodes = [];
  for (const item of geometry.series) {
    if ((item.area || geometry.area) && item.segments.length) {
      seriesNodes.push(node('path', {
        class: 'ozma-chart-area',
        d: chartAreaPath(item, { baseline: geometry.baselineY ?? geometry.bottom }),
        fill: item.color,
        'fill-opacity': numberText(
          Number.isFinite(item.fillOpacity) ? item.fillOpacity : geometry.areaOpacity,
        ),
        stroke: 'none',
        'data-series': item.id,
      }));
    }
    if (item.segments.length) {
      seriesNodes.push(node('path', {
        class: 'ozma-chart-line',
        d: chartPath(item, { step: geometry.step || item.style === 'step' || item.style === 'area-step' }),
        fill: 'none',
        stroke: item.color,
        'stroke-width': numberText(geometry.lineWidth),
        'stroke-linecap': 'round',
        'stroke-linejoin': 'round',
        'vector-effect': 'non-scaling-stroke',
        'data-series': item.id,
      }));
    }
    if (item.style === 'dot' && item.definedPoints.length) {
      for (const point of item.definedPoints) {
        seriesNodes.push(node('circle', {
          class: 'ozma-chart-dot',
          cx: numberText(point.x),
          cy: numberText(point.y),
          r: numberText(Math.max(1.5, geometry.lineWidth / 2)),
          fill: item.color,
          'data-series': item.id,
        }));
      }
    }
    if (geometry.showLast && item.lastPoint) {
      seriesNodes.push(node('circle', {
        class: 'ozma-chart-last',
        cx: numberText(item.lastPoint.x),
        cy: numberText(item.lastPoint.y),
        r: 3,
        fill: item.color,
        stroke: geometry.colors.background,
        'stroke-width': 1,
        'data-series': item.id,
      }));
      if (geometry.showValue && geometry.series.length <= 4) {
        seriesNodes.push(textNode({
          class: 'ozma-chart-last-value',
          x: numberText(Math.min(geometry.right - 2, item.lastPoint.x + 6)),
          y: numberText(Math.max(geometry.top + 8, item.lastPoint.y - 5)),
          'text-anchor': item.lastPoint.x > geometry.right - 40 ? 'end' : 'start',
          fill: item.color,
          'font-family': fontFamily,
          'font-size': 9,
          'font-weight': 600,
          'data-series': item.id,
        }, `${formatNumber(item.last, geometry)}${item.unit ? ` ${item.unit}` : ''}`));
      }
    }
  }
  if (seriesNodes.length) content.push(node('g', { class: 'ozma-chart-series' }, seriesNodes));

  if (Number.isFinite(geometry.baselineY)) {
    content.push(node('line', {
      class: 'ozma-chart-baseline',
      x1: numberText(geometry.left),
      y1: numberText(geometry.baselineY),
      x2: numberText(geometry.right),
      y2: numberText(geometry.baselineY),
      stroke: geometry.colors.baseline,
      'stroke-width': 1,
      'stroke-opacity': 0.8,
      'stroke-dasharray': '2 4',
    }));
  }

  if (geometry.showLegend && geometry.legend.length) {
    const legendNodes = [];
    let x = geometry.left;
    let y = 12;
    for (const entry of geometry.legend) {
      const value = entry.last === null
        ? 'no data'
        : `${formatNumber(entry.last, geometry)}${entry.unit ? ` ${entry.unit}` : ''}`;
      const label = `${entry.label} ${value}`;
      const width = Math.min(180, Math.max(52, label.length * 5.4 + 20));
      if (x + width > geometry.right && x > geometry.left) {
        x = geometry.left;
        y += 14;
      }
      legendNodes.push(node('g', { class: 'ozma-chart-legend-item', 'data-series': entry.id }));
      legendNodes.push(node('rect', {
        class: 'ozma-chart-legend-swatch',
        x: numberText(x),
        y: numberText(y - 8),
        width: 8,
        height: 8,
        rx: 2,
        fill: entry.color,
      }));
      legendNodes.push(textNode({
        class: 'ozma-chart-legend-label',
        x: numberText(x + 12),
        y: numberText(y),
        fill: geometry.colors.text,
        'font-family': fontFamily,
        'font-size': 9,
      }, label));
      x += width;
      if (y + 14 > geometry.bottom) break;
    }
    content.push(node('g', { class: 'ozma-chart-legend' }, legendNodes));
  }

  if (geometry.tooltip) {
    const tooltipHeight = 30;
    content.push(node('g', {
      class: 'ozma-chart-tooltip',
      opacity: 0,
      'aria-hidden': 'true',
      'data-tooltip': 'true',
    }, [
      node('rect', {
        class: 'ozma-chart-tooltip-box',
        x: 0,
        y: 0,
        width: 120,
        height: tooltipHeight,
        rx: 3,
        fill: geometry.colors.background,
        stroke: geometry.colors.grid,
        'stroke-width': 1,
      }),
      textNode({
        class: 'ozma-chart-tooltip-text',
        x: 6,
        y: 12,
        fill: geometry.colors.text,
        'font-family': fontFamily,
        'font-size': 10,
      }, ''),
    ]));
  }

  const root = node('svg', {
    xmlns: SVG_NS,
    viewBox: `0 0 ${numberText(geometry.width)} ${numberText(geometry.height)}`,
    class: className,
    role: 'img',
    'aria-label': ariaLabel,
    'aria-describedby': `${uid}-desc`,
    'data-reduced-motion': reducedMotion ? 'true' : 'false',
    'data-animate': geometry.animate ? 'true' : null,
    focusable: 'false',
    preserveAspectRatio: 'xMidYMid meet',
    style: geometry.animate && !reducedMotion
      ? 'transition: opacity 200ms ease'
      : null,
  }, children.concat(content));

  return { root, geometry, reducedMotion, uid };
}

function serializeNode(item) {
  if (!item) return '';
  const attrs = Object.entries(item.attrs || {})
    .filter(([, value]) => value !== undefined && value !== null)
    .map(([key, value]) => ` ${key}="${escapeXml(value)}"`)
    .join('');
  const children = item.children || [];
  if (item.text !== undefined && item.text !== null && !children.length) {
    return `<${item.tag}${attrs}>${escapeXml(item.text)}</${item.tag}>`;
  }
  if (children.length) {
    return `<${item.tag}${attrs}>${children.map(serializeNode).join('')}</${item.tag}>`;
  }
  return `<${item.tag}${attrs}/>`;
}

/**
 * Render a chart to an SVG string.  Pure: no DOM access, no mutation.
 */
export function renderChartSVG(config = {}, series = [], tokens = {}, options = {}) {
  return serializeNode(buildChartTree(config, series, tokens, options).root);
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

function clearElement(element) {
  if (!element) return;
  if (typeof element.replaceChildren === 'function') {
    element.replaceChildren();
    return;
  }
  while (element.firstChild) element.removeChild(element.firstChild);
}

function setAttributes(element, attributes) {
  if (!element || typeof element.setAttribute !== 'function') return;
  for (const [name, value] of Object.entries(attributes)) {
    if (value === undefined || value === null) continue;
    element.setAttribute(name, String(value));
  }
}

function createDomTree(doc, item) {
  const element = doc.createElementNS(SVG_NS, item.tag);
  setAttributes(element, item.attrs || {});
  if (item.text !== undefined && item.text !== null) element.textContent = String(item.text);
  for (const child of item.children || []) element.appendChild(createDomTree(doc, child));
  return element;
}

function hideTooltip(svg) {
  const tooltip = byClass(svg, 'ozma-chart-tooltip');
  if (tooltip) tooltip.setAttribute('opacity', '0');
}

function showTooltip(svg, event) {
  const state = svg.__ozmaChart;
  if (!state || !state.geometry || !state.geometry.tooltip) {
    hideTooltip(svg);
    return;
  }
  const geometry = state.geometry;
  const tooltip = byClass(svg, 'ozma-chart-tooltip');
  if (!tooltip) return;

  let relativeX;
  if (typeof svg.getBoundingClientRect === 'function') {
    try {
      const rect = svg.getBoundingClientRect();
      if (rect && rect.width > 0) {
        relativeX = ((Number(event.clientX) - rect.left) / rect.width) * geometry.width;
      }
    } catch {
      // Fall back to offsetX below.
    }
  }
  if (!Number.isFinite(relativeX)) relativeX = Number(event.offsetX);
  if (!Number.isFinite(relativeX)) return;

  let nearest = null;
  for (const item of geometry.series) {
    for (const point of item.points) {
      if (!point.defined) continue;
      const distance = Math.abs(point.x - relativeX);
      if (!nearest || distance < nearest.distance) {
        nearest = { point, distance, index: point.index };
      }
    }
  }
  if (!nearest) {
    hideTooltip(svg);
    return;
  }

  const lines = geometry.series.map((item) => {
    const point = item.points[nearest.index];
    const value = point && point.defined
      ? `${formatNumber(point.value, geometry)}${item.unit ? ` ${item.unit}` : ''}`
      : '—';
    return `${item.label}: ${value}`;
  }).slice(0, 4);

  const text = byClass(tooltip, 'ozma-chart-tooltip-text');
  const box = byClass(tooltip, 'ozma-chart-tooltip-box');
  if (!text) return;
  if (typeof text.replaceChildren === 'function') text.replaceChildren();
  else while (text.firstChild) text.removeChild(text.firstChild);
  const doc = text.ownerDocument;
  if (!doc) return;
  lines.forEach((line, index) => {
    const tspan = doc.createElementNS(SVG_NS, 'tspan');
    tspan.setAttribute('x', '6');
    tspan.setAttribute('dy', index === 0 ? '0' : '13');
    tspan.textContent = line;
    text.appendChild(tspan);
  });

  const lineHeight = 13;
  const width = Math.min(
    geometry.width - 8,
    Math.max(70, Math.max(...lines.map((line) => line.length)) * 5.6 + 12),
  );
  const height = lines.length * lineHeight + 8;
  if (box) {
    setAttributes(box, { width: numberText(width), height: numberText(height) });
  }
  const x = clamp(nearest.point.x + 8, 2, Math.max(2, geometry.width - width - 2));
  const y = clamp(nearest.point.y - height - 6, 2, Math.max(2, geometry.height - height - 2));
  setAttributes(tooltip, {
    transform: `translate(${numberText(x)} ${numberText(y)})`,
    opacity: 1,
  });
}

function attachInteraction(svg) {
  if (typeof svg.addEventListener !== 'function' || svg.__ozmaChartInteractive) return;
  svg.__ozmaChartInteractive = true;
  svg.addEventListener('pointermove', (event) => showTooltip(svg, event));
  svg.addEventListener('pointerleave', () => hideTooltip(svg));
  svg.addEventListener('pointercancel', () => hideTooltip(svg));
}

function applyChart(svg, config, series, tokens, options) {
  const doc = svg.ownerDocument;
  if (!doc) return;
  const tree = buildChartTree(config, series, tokens, options);
  setAttributes(svg, tree.root.attrs || {});
  clearElement(svg);
  for (const child of tree.root.children || []) svg.appendChild(createDomTree(doc, child));
  svg.__ozmaChart = { geometry: tree.geometry, reducedMotion: tree.reducedMotion };
  attachInteraction(svg);
}

function findChartSvg(container) {
  const first = container.firstElementChild;
  if (first && String(first.tagName || '').toLowerCase() === 'svg' && hasClass(first, 'ozma-chart')) {
    return first;
  }
  if (typeof container.querySelector === 'function') {
    return container.querySelector('svg.ozma-chart');
  }
  return null;
}

/**
 * Render an accessible multi-series chart into a container.  Returns the SVG
 * element in DOM environments, otherwise the SVG string.
 */
export function renderChart(container, config = {}, series = [], tokens = {}, options = {}) {
  if (!container || typeof container !== 'object') {
    throw new TypeError('renderChart requires a container element');
  }

  const doc = container.ownerDocument
    ?? (typeof globalThis.document !== 'undefined' ? globalThis.document : null);
  if (doc && typeof doc.createElementNS === 'function' && typeof container.appendChild === 'function') {
    clearElement(container);
    const svg = doc.createElementNS(SVG_NS, 'svg');
    container.appendChild(svg);
    applyChart(svg, config, series, tokens, options);
    return svg;
  }

  const markup = renderChartSVG(config, series, tokens, options);
  container.innerHTML = markup;
  return container.firstElementChild ?? markup;
}

/**
 * Update an existing chart in place.  The SVG element is reused; its
 * attributes and rendered children are replaced.  Falls back to a fresh render
 * when the container does not contain a chart yet.
 */
export function updateChart(container, series = [], config = {}, tokens = {}, options = {}) {
  if (!container || typeof container !== 'object') {
    throw new TypeError('updateChart requires a container element');
  }
  const svg = findChartSvg(container);
  if (!svg) return renderChart(container, config, series, tokens, options);
  applyChart(svg, config, series, tokens, options);
  return svg;
}

export const __private__ = Object.freeze({
  DEFAULT_COLORS,
  DEFAULT_HEIGHT,
  DEFAULT_PALETTE,
  DEFAULT_SAMPLES,
  DEFAULT_WIDTH,
  MAX_DIMENSION,
  MAX_SAMPLES,
  MAX_SERIES,
  MAX_TICKS,
  formatNumber,
  niceStep,
  normalizeChartConfig,
  resolveBands,
  resolveThresholds,
  safeLabel,
  severityFor,
});
