/**
 * EQCurveChart — SVG frequency response visualisation.
 *
 * Renders up to three curves on a log-frequency / linear-dB grid:
 *   measured  — dim grey (raw phone recording)
 *   target    — dashed blue (target curve)
 *   corrected — solid emerald (predicted post-correction response)
 *
 * Uses react-native-svg. No chart library.
 */

import React, {useMemo} from 'react';
import Svg, {
  Path,
  Line,
  Text as SvgText,
  Rect,
  G,
} from 'react-native-svg';

// ── Constants ─────────────────────────────────────────────────────────────────

const FREQ_MIN = 20;
const FREQ_MAX = 20000;
const DB_MIN = -15;
const DB_MAX = 15;

// Grid lines
const FREQ_GRID = [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];
const DB_GRID = [-12, -6, 0, 6, 12];

const PAD = {top: 12, right: 16, bottom: 32, left: 36};

// ── Coordinate mapping ────────────────────────────────────────────────────────

function freqToX(freq: number, width: number): number {
  const logMin = Math.log10(FREQ_MIN);
  const logMax = Math.log10(FREQ_MAX);
  return PAD.left + ((Math.log10(freq) - logMin) / (logMax - logMin)) * width;
}

function dbToY(db: number, height: number): number {
  const clamped = Math.max(DB_MIN, Math.min(DB_MAX, db));
  return PAD.top + ((DB_MAX - clamped) / (DB_MAX - DB_MIN)) * height;
}

function curveToPath(
  points: [number, number][],
  chartW: number,
  chartH: number,
): string {
  if (points.length === 0) return '';
  const sorted = [...points].sort((a, b) => a[0] - b[0]);
  return sorted
    .map((p, i) => {
      const x = freqToX(p[0], chartW).toFixed(1);
      const y = dbToY(p[1], chartH).toFixed(1);
      return `${i === 0 ? 'M' : 'L'}${x},${y}`;
    })
    .join(' ');
}

// ── Label helpers ─────────────────────────────────────────────────────────────

function freqLabel(hz: number): string {
  if (hz >= 1000) return `${hz / 1000}k`;
  return `${hz}`;
}

// ── Component ─────────────────────────────────────────────────────────────────

interface Props {
  measured?: [number, number][];
  target?: [number, number][];
  corrected?: [number, number][];
  width?: number;
  height?: number;
}

export function EQCurveChart({
  measured,
  target,
  corrected,
  width = 320,
  height = 200,
}: Props) {
  const chartW = width - PAD.left - PAD.right;
  const chartH = height - PAD.top - PAD.bottom;

  const measuredPath = useMemo(
    () => (measured ? curveToPath(measured, chartW, chartH) : ''),
    [measured, chartW, chartH],
  );
  const targetPath = useMemo(
    () => (target ? curveToPath(target, chartW, chartH) : ''),
    [target, chartW, chartH],
  );
  const correctedPath = useMemo(
    () => (corrected ? curveToPath(corrected, chartW, chartH) : ''),
    [corrected, chartW, chartH],
  );

  return (
    <Svg width={width} height={height}>
      {/* Background */}
      <Rect
        x={PAD.left}
        y={PAD.top}
        width={chartW}
        height={chartH}
        fill="#111827"
        rx={4}
      />

      {/* Frequency grid lines + labels */}
      <G>
        {FREQ_GRID.map(freq => {
          const x = freqToX(freq, chartW);
          return (
            <G key={freq}>
              <Line
                x1={x}
                y1={PAD.top}
                x2={x}
                y2={PAD.top + chartH}
                stroke="#374151"
                strokeWidth={0.5}
              />
              <SvgText
                x={x}
                y={PAD.top + chartH + 14}
                fontSize={9}
                fill="#6B7280"
                textAnchor="middle">
                {freqLabel(freq)}
              </SvgText>
            </G>
          );
        })}
      </G>

      {/* dB grid lines + labels */}
      <G>
        {DB_GRID.map(db => {
          const y = dbToY(db, chartH);
          const isZero = db === 0;
          return (
            <G key={db}>
              <Line
                x1={PAD.left}
                y1={y}
                x2={PAD.left + chartW}
                y2={y}
                stroke={isZero ? '#4B5563' : '#374151'}
                strokeWidth={isZero ? 1 : 0.5}
              />
              <SvgText
                x={PAD.left - 4}
                y={y + 4}
                fontSize={9}
                fill="#6B7280"
                textAnchor="end">
                {db > 0 ? `+${db}` : `${db}`}
              </SvgText>
            </G>
          );
        })}
      </G>

      {/* Measured curve — dim grey */}
      {measuredPath ? (
        <Path
          d={measuredPath}
          stroke="#6B7280"
          strokeWidth={1.5}
          fill="none"
          opacity={0.7}
        />
      ) : null}

      {/* Target curve — dashed blue */}
      {targetPath ? (
        <Path
          d={targetPath}
          stroke="#3B82F6"
          strokeWidth={1.5}
          fill="none"
          strokeDasharray="4,3"
          opacity={0.85}
        />
      ) : null}

      {/* Corrected prediction — solid emerald */}
      {correctedPath ? (
        <Path
          d={correctedPath}
          stroke="#10B981"
          strokeWidth={2}
          fill="none"
        />
      ) : null}
    </Svg>
  );
}
