/**
 * CrossChainFlowViz — Hero SVG visualization for the cross-chain aggregator showcase.
 *
 * Shows tokens flowing between L1 and L2 chains through bridge portals with
 * particle effects, liquid pool containers, and glowing sci-fi aesthetics.
 */

import { useMemo } from "react";
import styles from "./CrossChainFlowViz.module.css";

/* ── Types ── */

interface CrossChainFlowVizProps {
  vizPhase: number;
  splitPercent: number;
  l1ReserveA: string | null;
  l1ReserveB: string | null;
  l2ReserveA: string | null;
  l2ReserveB: string | null;
  showRouteDuel?: boolean;
  improvement?: string | null;
}

/* ── Path data ── */

const PATHS = {
  userToAgg: "M160,80 L240,80",
  aggToL1Amm: "M360,80 L500,80",
  l1AmmToOutput: "M620,80 L760,80",
  aggToPortalDown: "M300,105 C300,150 300,170 300,190",
  portalToL2Exec: "M300,210 C300,240 320,270 340,290",
  l2ExecToL2Amm: "M400,290 L540,290",
  l2AmmToPortalUp: "M660,290 C700,260 760,230 790,210",
  portalToOutput: "M790,190 C790,150 810,120 820,105",
} as const;

/* ── Colours ── */

const COL = {
  gold: "#fbbf24",
  blue: "#3b82f6",
  cyan: "#22d3ee",
  green: "#34d399",
  white: "#ffffff",
} as const;

/* ── Node positions ── */

interface NodeDef {
  x: number;
  y: number;
  label: string;
  sublabel?: string;
  chain: "l1" | "l2";
}

const NODES: NodeDef[] = [
  { x: 100, y: 80, label: "User", sublabel: "Wallet", chain: "l1" },
  { x: 300, y: 80, label: "Aggregator", sublabel: "Split Router", chain: "l1" },
  { x: 560, y: 80, label: "L1 AMM", chain: "l1" },
  { x: 820, y: 80, label: "Output", sublabel: "Best Price", chain: "l1" },
  { x: 340, y: 290, label: "L2 Executor", sublabel: "Cross-Chain", chain: "l2" },
  { x: 600, y: 290, label: "L2 AMM", chain: "l2" },
];

/* ── Sub-components ── */

function SvgDefs() {
  return (
    <defs>
      {/* Glow filter — soft halo around particles */}
      <filter id="glow" x="-50%" y="-50%" width="200%" height="200%">
        <feGaussianBlur stdDeviation="3" />
      </filter>

      {/* Plasma filter — turbulent particle tails */}
      <filter id="plasma" x="-100%" y="-100%" width="300%" height="300%">
        <feGaussianBlur in="SourceGraphic" stdDeviation="2" result="b" />
        <feTurbulence
          type="fractalNoise"
          baseFrequency="0.6"
          numOctaves={2}
          result="n"
        />
        <feDisplacementMap in="b" in2="n" scale={4} />
      </filter>

      {/* Node glow filter */}
      <filter id="nodeGlow" x="-30%" y="-30%" width="160%" height="160%">
        <feGaussianBlur stdDeviation="3" result="b" />
        <feMerge>
          <feMergeNode in="b" />
          <feMergeNode in="SourceGraphic" />
        </feMerge>
      </filter>

      {/* Stronger glow for active portals */}
      <filter id="portalGlow" x="-60%" y="-60%" width="220%" height="220%">
        <feGaussianBlur stdDeviation="5" result="b" />
        <feMerge>
          <feMergeNode in="b" />
          <feMergeNode in="b" />
          <feMergeNode in="SourceGraphic" />
        </feMerge>
      </filter>

      {/* Bloom filter for success state */}
      <filter id="bloomFilter" x="-40%" y="-40%" width="180%" height="180%">
        <feGaussianBlur stdDeviation="4" result="b" />
        <feComponentTransfer in="b" result="bright">
          <feFuncA type="linear" slope="2" />
        </feComponentTransfer>
        <feMerge>
          <feMergeNode in="bright" />
          <feMergeNode in="SourceGraphic" />
        </feMerge>
      </filter>

      {/* Portal center radial gradient */}
      <radialGradient id="portalGrad">
        <stop offset="0%" stopColor={COL.cyan} stopOpacity={0.5} />
        <stop offset="100%" stopColor={COL.cyan} stopOpacity={0} />
      </radialGradient>

      {/* L1 lane background gradient */}
      <linearGradient id="l1LaneGrad" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stopColor="rgba(99,102,241,0.05)" />
        <stop offset="100%" stopColor="rgba(99,102,241,0.01)" />
      </linearGradient>

      {/* L2 lane background gradient */}
      <linearGradient id="l2LaneGrad" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stopColor="rgba(52,211,153,0.01)" />
        <stop offset="100%" stopColor="rgba(52,211,153,0.05)" />
      </linearGradient>

      {/* Pool liquid gradients */}
      <linearGradient id="liquidA" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stopColor="#818cf8" stopOpacity={0.9} />
        <stop offset="100%" stopColor="#6366f1" stopOpacity={0.7} />
      </linearGradient>
      <linearGradient id="liquidB" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0%" stopColor="#34d399" stopOpacity={0.8} />
        <stop offset="100%" stopColor="#10b981" stopOpacity={0.6} />
      </linearGradient>

      {/* Wave clip paths for pools */}
      <clipPath id="l1PoolClip">
        <rect x="0" y="0" width="120" height="50" rx="6" />
      </clipPath>
      <clipPath id="l2PoolClip">
        <rect x="0" y="0" width="120" height="50" rx="6" />
      </clipPath>

      {/* Scanline pattern for bridge zone */}
      <pattern id="scanlines" width="4" height="4" patternUnits="userSpaceOnUse">
        <line x1="0" y1="2" x2="4" y2="2" stroke="rgba(34,211,238,0.06)" strokeWidth="0.5" />
      </pattern>
    </defs>
  );
}

/* ── Particle size presets ──
 * plasma radius, halo radius, core radius
 * Used by JourneyParticle for the 3-layer particle rendering. */
const PARTICLE_SIZES = {
  small:  { plasma: 5,  halo: 3, core: 1   },
  normal: { plasma: 7,  halo: 4, core: 1.5 },
  large:  { plasma: 10, halo: 6, core: 2.5 },
} as const;

/* ── Ambient idle particles — slow drifting specks ── */

function AmbientParticles() {
  return (
    <g>
      {/* Bright cyan dot drifting along L1 lane */}
      <circle r={4} fill={COL.cyan} opacity={0.4} filter="url(#glow)">
        <animateMotion dur="7s" repeatCount="indefinite" path="M100,80 L820,80" />
      </circle>
      {/* Gold dot drifting along L2 lane */}
      <circle r={3.5} fill={COL.gold} opacity={0.35} filter="url(#glow)">
        <animateMotion dur="9s" repeatCount="indefinite" path="M340,290 L600,290" />
      </circle>
      {/* Cyan dot crossing bridge zone slowly */}
      <circle r={3} fill={COL.cyan} opacity={0.3} filter="url(#glow)">
        <animateMotion dur="12s" repeatCount="indefinite" path="M250,190 C450,185 650,195 800,190" />
      </circle>
    </g>
  );
}

/* ── Journey Particle ── */

/**
 * A particle that travels along a single path segment, with a label following it.
 *
 * Drives off a master cycle: visible only between [beginOffset, beginOffset+duration]
 * within each cycle. The animation is restarted by the master cycle's `begin` event,
 * so the whole sequence loops smoothly forever.
 */
interface JourneyParticleProps {
  path: string;
  duration: number;
  beginOffset: number;
  color: string;
  label: string;
  labelColor: string;
  size?: "small" | "normal" | "large";
  /** Where to put the label relative to the particle: above (-) or below (+) */
  labelDy?: number;
}

// Cycle period in seconds — ALL particles loop in lock-step at this interval.
// The journey runs from t=0 to t=10, then there's a 2s pause before the next
// cycle starts at t=12.
const CYCLE_PERIOD = 12;

// Build a 5-cycle list of begin times so SMIL re-fires reliably even when
// the browser throttles the cycleClock animation. Indefinite repetition is
// then driven by the explicit list, not by referencing a single sync source.
function buildBeginTimes(offset: number, count = 50): string {
  return Array.from({ length: count }, (_, i) => `${offset + i * CYCLE_PERIOD}s`).join(";");
}

function JourneyParticle({
  path,
  duration,
  beginOffset,
  color,
  label,
  labelColor,
  size = "normal",
  labelDy = -12,
}: JourneyParticleProps) {
  const sz = PARTICLE_SIZES[size];
  // Each segment fires at offset, offset+CYCLE, offset+2*CYCLE, ... for many cycles.
  // No reliance on cycleClock — direct begin times survive tab throttling.
  const beginList = buildBeginTimes(beginOffset);
  const endList = buildBeginTimes(beginOffset + duration);
  const dur = `${duration}s`;

  return (
    <g opacity={0}>
      {/* Master visibility — show during the segment, hide at the end */}
      <set attributeName="opacity" to="1" begin={beginList} />
      <set attributeName="opacity" to="0" begin={endList} />

      {/* Plasma tail */}
      <circle r={sz.plasma} fill={color} opacity={0.18} filter="url(#plasma)">
        <animateMotion dur={dur} begin={beginList} path={path} fill="freeze" />
      </circle>
      {/* Color halo */}
      <circle r={sz.halo} fill={color} opacity={0.45} filter="url(#glow)">
        <animateMotion dur={dur} begin={beginList} path={path} fill="freeze" />
      </circle>
      {/* White core */}
      <circle r={sz.core} fill={COL.white} opacity={0.95}>
        <animateMotion dur={dur} begin={beginList} path={path} fill="freeze" />
      </circle>

      {/* Label — follows the particle. Thin & elegant: no stroke, no filter.
          Plain colored text relies on its own brightness against the dark
          lane backgrounds. */}
      <text
        x={0}
        y={labelDy}
        fontSize={9}
        fontWeight={500}
        fill={labelColor}
        textAnchor="middle"
        fontFamily="var(--sans)"
        letterSpacing="0.05em"
        opacity={0.95}
      >
        {label}
        <animateMotion dur={dur} begin={beginList} path={path} fill="freeze" />
      </text>
    </g>
  );
}

/* ── Journey path constants ──
 *
 * Combined SVG path strings for each leg of the cross-chain story.
 * The remote leg is broken into multiple segments to allow per-segment
 * label changes (WETH → wWETH → wUSDC → USDC).
 *
 * Cycle layout (4s total):
 *   t=0.0 → t=1.0  Pre-split:  User → Aggregator
 *   t=1.0 → t=4.0  Local:      Aggregator → L1 AMM → Output (3s)
 *   t=1.0 → t=4.0  Remote:     Aggregator → Portal → L2 Exec → L2 AMM → Portal → Output (3s)
 *
 * Both branches end at t=4.0 (Output) so they arrive simultaneously.
 */
const JOURNEY_PATHS = {
  preSplit: "M100,80 L300,80",
  // Local leg
  localToL1Amm: "M300,80 L560,80",
  localToOutput: "M560,80 L820,80",
  // Remote leg (broken into 5 short segments to support per-segment label swaps)
  remoteAggToPortalDown: "M300,90 L300,180",
  remotePortalToL2Exec: "M300,200 C300,240 320,270 340,290",
  remoteL2ExecToL2Amm: "M340,290 L600,290",
  remoteL2AmmToPortalUp: "M600,290 C700,260 760,230 790,200",
  remotePortalToOutput: "M790,180 L820,90",
} as const;

/* ── Coordinated Journey ──
 *
 * Renders the full cross-chain journey: a single particle splits at the Aggregator
 * into local and remote branches that arrive at Output simultaneously. Loops forever.
 */
function CoordinatedJourney() {
  // 10-second cycle (CYCLE_PERIOD). Layout:
  //   t=0   → 2.4   Pre-split: User → Aggregator
  //   t=2.4 → 10.0  Local:  Agg → L1 AMM (3.8s WETH) → Output (3.8s USDC)
  //   t=2.4 → 10.0  Remote: Agg → Portal Down → L2 Exec → L2 AMM → Portal Up → Output
  //
  // No master cycleClock — each segment has its own explicit list of begin times
  // (built by buildBeginTimes), so the loop survives browser tab throttling.
  return (
    <g>
      {/* ── Phase A: Pre-split (t=0 → 2.4) ── */}
      <JourneyParticle
        path={JOURNEY_PATHS.preSplit}
        duration={2.4}
        beginOffset={0}
        color={COL.gold}
        label="WETH"
        labelColor={COL.gold}
        size="large"
        labelDy={-15}
      />

      {/* ── Phase B Local: Agg → L1 AMM → Output (t=2.4 → 10.0) ── */}
      <JourneyParticle
        path={JOURNEY_PATHS.localToL1Amm}
        duration={3.8}
        beginOffset={2.4}
        color={COL.gold}
        label="WETH"
        labelColor={COL.gold}
        labelDy={-14}
      />
      <JourneyParticle
        path={JOURNEY_PATHS.localToOutput}
        duration={3.8}
        beginOffset={6.2}
        color={COL.blue}
        label="USDC"
        labelColor={COL.blue}
        labelDy={-14}
      />

      {/* ── Phase B Remote: Agg → Portal → L2 Exec → L2 AMM → Portal → Output ──
          5 segments totaling 7.6s, same wall-clock as local branch. */}
      <JourneyParticle
        path={JOURNEY_PATHS.remoteAggToPortalDown}
        duration={1.3}
        beginOffset={2.4}
        color={COL.gold}
        label="WETH"
        labelColor={COL.gold}
        labelDy={-12}
      />
      <JourneyParticle
        path={JOURNEY_PATHS.remotePortalToL2Exec}
        duration={1.3}
        beginOffset={3.7}
        color={COL.cyan}
        label="wWETH"
        labelColor={COL.cyan}
        labelDy={-12}
      />
      <JourneyParticle
        path={JOURNEY_PATHS.remoteL2ExecToL2Amm}
        duration={1.6}
        beginOffset={5.0}
        color={COL.cyan}
        label="wWETH"
        labelColor={COL.cyan}
        labelDy={16}
      />
      <JourneyParticle
        path={JOURNEY_PATHS.remoteL2AmmToPortalUp}
        duration={1.7}
        beginOffset={6.6}
        color={COL.cyan}
        label="wUSDC"
        labelColor={COL.cyan}
        labelDy={-12}
      />
      <JourneyParticle
        path={JOURNEY_PATHS.remotePortalToOutput}
        duration={1.7}
        beginOffset={8.3}
        color={COL.blue}
        label="USDC"
        labelColor={COL.blue}
        labelDy={-12}
      />
    </g>
  );
}

/* ── Bridge Portal ── */

interface PortalProps {
  x: number;
  y: number;
  active: boolean;
}

function Portal({ x, y, active }: PortalProps) {
  const baseOpacity = active ? 0.9 : 0.5;
  const filterAttr = active ? "url(#portalGlow)" : undefined;
  return (
    <g filter={filterAttr}>
      {/* Outer ring — clockwise */}
      <circle
        cx={x}
        cy={y}
        r={32}
        stroke={COL.cyan}
        strokeWidth={1.5}
        strokeDasharray="8 4"
        fill="none"
        opacity={baseOpacity}
      >
        <animateTransform
          attributeName="transform"
          type="rotate"
          from={`0 ${x} ${y}`}
          to={`360 ${x} ${y}`}
          dur="8s"
          repeatCount="indefinite"
        />
      </circle>
      {/* Middle ring — counter-clockwise */}
      <circle
        cx={x}
        cy={y}
        r={24}
        stroke={COL.cyan}
        strokeWidth={1.2}
        strokeDasharray="5 3"
        fill="none"
        opacity={baseOpacity * 0.7}
      >
        <animateTransform
          attributeName="transform"
          type="rotate"
          from={`360 ${x} ${y}`}
          to={`0 ${x} ${y}`}
          dur="12s"
          repeatCount="indefinite"
        />
      </circle>
      {/* Inner ring — clockwise fast */}
      <circle
        cx={x}
        cy={y}
        r={16}
        stroke={COL.cyan}
        strokeWidth={1}
        strokeDasharray="3 2"
        fill="none"
        opacity={baseOpacity * 0.5}
      >
        <animateTransform
          attributeName="transform"
          type="rotate"
          from={`0 ${x} ${y}`}
          to={`360 ${x} ${y}`}
          dur="6s"
          repeatCount="indefinite"
        />
      </circle>
      {/* Radial glow center */}
      <circle
        cx={x}
        cy={y}
        r={10}
        fill="url(#portalGrad)"
        opacity={active ? 0.8 : 0.3}
      />
      {/* Bright core dot */}
      <circle
        cx={x}
        cy={y}
        r={3}
        fill={COL.cyan}
        opacity={active ? 0.9 : 0.4}
        filter="url(#glow)"
      />
    </g>
  );
}

/* ── Liquid Pool ── */

interface LiquidPoolProps {
  x: number;
  y: number;
  reserveA: string | null;
  reserveB: string | null;
  chain: "l1" | "l2";
  active: boolean;
}

function LiquidPool({ x, y, reserveA, reserveB, chain, active }: LiquidPoolProps) {
  // In a Uniswap V2 AMM, both sides always have equal VALUE (that's the
  // invariant). So the visual split is always 50/50 — the actual reserve
  // amounts are shown as labels inside each half. Liquidity is conveyed
  // through the labels, not the bar widths.
  //
  // Liquid fills 70% of the inner height from the bottom — the top 30% is
  // empty headroom, like a real glass container with liquid inside.
  const poolX = x - 60;
  const poolY = y - 25;
  const poolW = 120;
  const poolH = 50;
  const padding = 2;
  const innerW = poolW - padding * 2;
  const innerH = poolH - padding * 2;

  const halfW = innerW / 2;
  const leftWidth = halfW;
  const rightWidth = halfW;
  const dividerX = poolX + padding + halfW;

  // Liquid level: 70% fill from the bottom of the inner area
  const fillRatio = 0.7;
  const liquidH = innerH * fillRatio;
  const liquidTop = poolY + padding + (innerH - liquidH);
  const liquidBottom = poolY + padding + innerH;

  const borderColor = chain === "l1"
    ? (active ? "rgba(99,102,241,0.6)" : "rgba(99,102,241,0.25)")
    : (active ? "rgba(52,211,153,0.6)" : "rgba(52,211,153,0.25)");

  const tokenALabel = chain === "l1" ? "WETH" : "wWETH";
  const tokenBLabel = chain === "l1" ? "USDC" : "wUSDC";

  // Visibility thresholds — hide labels on very narrow sides
  const showLeftLabel = leftWidth >= 25;
  const showRightLabel = rightWidth >= 25;

  // Surface wave Y — sits at the top of the liquid (not the top of the container)
  const surfaceY = liquidTop;

  return (
    <g>
      {/* Glass container outer */}
      <rect
        x={poolX}
        y={poolY}
        width={poolW}
        height={poolH}
        rx={6}
        fill="rgba(18,18,28,0.6)"
        stroke={borderColor}
        strokeWidth={active ? 1.4 : 0.8}
      />

      {/* Left side — Token A liquid fill (70% from bottom) */}
      <rect
        x={poolX + padding}
        y={liquidTop}
        width={leftWidth}
        height={liquidH}
        rx={3}
        fill="url(#liquidA)"
        opacity={0.38}
      />

      {/* Right side — Token B liquid fill (70% from bottom) */}
      <rect
        x={dividerX}
        y={liquidTop}
        width={rightWidth}
        height={liquidH}
        rx={3}
        fill="url(#liquidB)"
        opacity={0.38}
      />

      {/* Vertical divider line — only spans the liquid portion */}
      <line
        x1={dividerX}
        y1={liquidTop}
        x2={dividerX}
        y2={liquidBottom}
        stroke={chain === "l1" ? "rgba(129,140,248,0.7)" : "rgba(52,211,153,0.7)"}
        strokeWidth={1}
        opacity={0.8}
      />

      {/* Glass reflection — subtle white gradient at top */}
      <rect
        x={poolX}
        y={poolY}
        width={poolW}
        height={4}
        rx={2}
        fill="white"
        opacity={0.07}
      />

      {/* Horizontal wave on top of LEFT side surface */}
      {leftWidth > 12 && (() => {
        const segCount = Math.max(1, Math.floor((leftWidth - 4) / 8));
        const wavePath = `M${poolX + padding + 2},${surfaceY} q4,-1.5 8,0` +
          " t8,0".repeat(Math.max(0, segCount - 1));
        return (
          <path
            d={wavePath}
            fill="none"
            stroke="#a5b4fc"
            strokeWidth={0.7}
            opacity={0.6}
          >
            <animateTransform
              attributeName="transform"
              type="translate"
              values="0,0; -8,0; 0,0"
              dur="3s"
              repeatCount="indefinite"
            />
          </path>
        );
      })()}

      {/* Horizontal wave on top of RIGHT side surface */}
      {rightWidth > 12 && (() => {
        const segCount = Math.max(1, Math.floor((rightWidth - 4) / 8));
        const wavePath = `M${dividerX + 2},${surfaceY} q4,-1.5 8,0` +
          " t8,0".repeat(Math.max(0, segCount - 1));
        return (
          <path
            d={wavePath}
            fill="none"
            stroke="#6ee7b7"
            strokeWidth={0.7}
            opacity={0.6}
          >
            <animateTransform
              attributeName="transform"
              type="translate"
              values="0,0; -8,0; 0,0"
              dur="3.6s"
              repeatCount="indefinite"
            />
          </path>
        );
      })()}

      {/* Left token label + amount (centered in left half, inside the liquid) */}
      {showLeftLabel && (
        <>
          <text
            x={poolX + padding + leftWidth / 2}
            y={liquidTop + liquidH / 2 - 1}
            textAnchor="middle"
            fill="#c7d2fe"
            fontSize={8}
            fontWeight={700}
            fontFamily="var(--mono)"
            opacity={0.95}
          >
            {tokenALabel}
          </text>
          {reserveA !== null && (
            <text
              x={poolX + padding + leftWidth / 2}
              y={liquidTop + liquidH / 2 + 9}
              textAnchor="middle"
              fill="#a5b4fc"
              fontSize={7}
              fontFamily="var(--mono)"
              opacity={0.75}
            >
              {formatReserve(reserveA)}
            </text>
          )}
        </>
      )}

      {/* Right token label + amount (centered in right half, inside the liquid) */}
      {showRightLabel && (
        <>
          <text
            x={dividerX + rightWidth / 2}
            y={liquidTop + liquidH / 2 - 1}
            textAnchor="middle"
            fill="#a7f3d0"
            fontSize={8}
            fontWeight={700}
            fontFamily="var(--mono)"
            opacity={0.95}
          >
            {tokenBLabel}
          </text>
          {reserveB !== null && (
            <text
              x={dividerX + rightWidth / 2}
              y={liquidTop + liquidH / 2 + 9}
              textAnchor="middle"
              fill="#6ee7b7"
              fontSize={7}
              fontFamily="var(--mono)"
              opacity={0.75}
            >
              {formatReserve(reserveB)}
            </text>
          )}
        </>
      )}

      {/* Shimmer overlay when active */}
      {active && (
        <rect
          x={poolX + padding}
          y={poolY + padding}
          width={innerW}
          height={innerH}
          rx={4}
          fill="none"
          stroke={chain === "l1" ? "rgba(99,102,241,0.4)" : "rgba(52,211,153,0.4)"}
          strokeWidth={0.6}
        >
          <animate
            attributeName="opacity"
            values="0.3;0.8;0.3"
            dur="2s"
            repeatCount="indefinite"
          />
        </rect>
      )}
    </g>
  );
}

function formatReserve(v: string): string {
  const n = parseFloat(v);
  if (isNaN(n)) return v;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)}K`;
  return n.toFixed(1);
}

/* ── Flow Node ── */

interface FlowNodeProps {
  x: number;
  y: number;
  label: string;
  sublabel?: string;
  chain: "l1" | "l2";
  active: boolean;
}

function FlowNode({ x, y, label, sublabel, chain, active }: FlowNodeProps) {
  const stroke = chain === "l1"
    ? "rgba(99,102,241,0.5)"
    : "rgba(52,211,153,0.5)";
  const activeStroke = chain === "l1"
    ? "var(--accent)"
    : "var(--green)";

  return (
    <g>
      <rect
        x={x - 60}
        y={y - 25}
        width={120}
        height={50}
        rx={8}
        fill="var(--bg-card)"
        stroke={active ? activeStroke : stroke}
        strokeWidth={active ? 1.5 : 0.8}
        filter={active ? "url(#nodeGlow)" : undefined}
      />
      <text
        x={x}
        y={y - 4}
        textAnchor="middle"
        fill="var(--text)"
        fontSize={11}
        fontWeight={600}
        fontFamily="var(--sans)"
      >
        {label}
      </text>
      {sublabel && (
        <text
          x={x}
          y={y + 12}
          textAnchor="middle"
          fill="var(--text-dim)"
          fontSize={9}
          fontFamily="var(--mono)"
        >
          {sublabel}
        </text>
      )}
    </g>
  );
}

/* ── Route Path ── */

interface RoutePathProps {
  d: string;
  active: boolean;
  complete: boolean;
  width: number;
  dashed?: boolean;
  color?: string;
}

function RoutePath({ d, active, complete, width, dashed, color }: RoutePathProps) {
  const strokeColor = complete ? COL.green
    : active ? (color ?? COL.cyan)
    : "rgba(130, 140, 255, 0.7)";
  const effectiveWidth = Math.max(2.5, width);
  const op = complete ? 0.9 : active ? 1.0 : 0.6;
  return (
    <path
      d={d}
      fill="none"
      stroke={strokeColor}
      strokeWidth={effectiveWidth}
      strokeDasharray={dashed ? "6 4" : undefined}
      strokeLinecap="round"
      opacity={op}
      className={complete ? styles.successPath : undefined}
    />
  );
}

/* ── Main Component ── */

export function CrossChainFlowViz({
  vizPhase,
  splitPercent,
  l1ReserveA,
  l1ReserveB,
  l2ReserveA,
  l2ReserveB,
  showRouteDuel,
  improvement,
}: CrossChainFlowVizProps) {
  const isComplete = vizPhase === 8;
  const localWidth = (splitPercent / 100) * 3 + 0.5;
  const remoteWidth = ((100 - splitPercent) / 100) * 3 + 0.5;

  // Determine which nodes are active based on phase
  const activeNodes = useMemo(() => {
    const set = new Set<string>();
    if (vizPhase >= 1) { set.add("User"); set.add("Aggregator"); }
    if (vizPhase >= 2) set.add("L1 AMM");
    if (vizPhase >= 3) { set.add("L2 Executor"); }
    if (vizPhase >= 4) set.add("Output");
    if (vizPhase >= 5) set.add("L2 AMM");
    if (vizPhase >= 7) set.add("Output");
    return set;
  }, [vizPhase]);

  // Determine portal activity
  const portalDownActive = vizPhase === 3 || vizPhase === 6;
  const portalUpActive = vizPhase === 6;

  return (
    <div className={styles.container}>
      <svg
        className={styles.svg}
        viewBox="0 0 960 370"
        xmlns="http://www.w3.org/2000/svg"
        preserveAspectRatio="xMidYMid meet"
      >
        <SvgDefs />

        {/* ══════════════ Layer 1: Background ══════════════ */}

        {/* L1 lane */}
        <rect
          x={0}
          y={0}
          width={960}
          height={155}
          fill="url(#l1LaneGrad)"
          rx={0}
        />

        {/* L2 lane */}
        <rect
          x={0}
          y={215}
          width={960}
          height={155}
          fill="url(#l2LaneGrad)"
          rx={0}
        />

        {/* Bridge zone */}
        <rect
          x={0}
          y={155}
          width={960}
          height={60}
          fill="url(#scanlines)"
          opacity={0.8}
        />

        {/* Bridge zone dashed borders */}
        <line
          x1={40}
          y1={155}
          x2={920}
          y2={155}
          stroke={COL.cyan}
          strokeWidth={0.5}
          strokeDasharray="12 8"
          opacity={0.2}
        />
        <line
          x1={40}
          y1={215}
          x2={920}
          y2={215}
          stroke={COL.cyan}
          strokeWidth={0.5}
          strokeDasharray="12 8"
          opacity={0.2}
        />

        {/* Lane labels */}
        <text
          x={22}
          y={24}
          fill="rgba(99,102,241,0.4)"
          fontSize={10}
          fontFamily="var(--mono)"
          fontWeight={700}
          letterSpacing="0.12em"
        >
          L1
        </text>
        <text
          x={22}
          y={242}
          fill="rgba(52,211,153,0.4)"
          fontSize={10}
          fontFamily="var(--mono)"
          fontWeight={700}
          letterSpacing="0.12em"
        >
          L2
        </text>

        {/* ══════════════ Layer 2: Paths ══════════════ */}

        {/* Local route: User -> Aggregator -> L1 AMM -> Output */}
        <RoutePath
          d={PATHS.userToAgg}
          active={vizPhase >= 1}
          complete={isComplete}
          width={2.5}
        />
        <RoutePath
          d={PATHS.aggToL1Amm}
          active={vizPhase >= 2}
          complete={isComplete}
          width={localWidth}
        />
        <RoutePath
          d={PATHS.l1AmmToOutput}
          active={vizPhase >= 4}
          complete={isComplete}
          width={localWidth}
        />

        {/* Remote route: Aggregator -> Portal -> L2 Executor -> L2 AMM -> Portal -> Output */}
        <RoutePath
          d={PATHS.aggToPortalDown}
          active={vizPhase >= 3}
          complete={isComplete}
          width={remoteWidth}
        />
        <RoutePath
          d={PATHS.portalToL2Exec}
          active={vizPhase >= 3}
          complete={isComplete}
          width={remoteWidth}
        />
        <RoutePath
          d={PATHS.l2ExecToL2Amm}
          active={vizPhase >= 5}
          complete={isComplete}
          width={remoteWidth}
        />
        <RoutePath
          d={PATHS.l2AmmToPortalUp}
          active={vizPhase >= 6}
          complete={isComplete}
          width={remoteWidth}
        />
        <RoutePath
          d={PATHS.portalToOutput}
          active={vizPhase >= 6}
          complete={isComplete}
          width={remoteWidth}
        />

        {/* Route duel ghost path -- direct L1-only route shown as red dashed */}
        {showRouteDuel && (
          <RoutePath
            d="M160,65 L760,65"
            active={true}
            complete={false}
            width={1.5}
            dashed={true}
            color="#f87171"
          />
        )}

        {/* ══════════════ Layer 3: Liquid Pools ══════════════ */}

        <LiquidPool
          x={560}
          y={80}
          reserveA={l1ReserveA}
          reserveB={l1ReserveB}
          chain="l1"
          active={vizPhase >= 2 && vizPhase <= 4}
        />
        <LiquidPool
          x={600}
          y={290}
          reserveA={l2ReserveA}
          reserveB={l2ReserveB}
          chain="l2"
          active={vizPhase >= 5 && vizPhase <= 6}
        />

        {/* ══════════════ Layer 4: Bridge Portals ══════════════ */}

        <Portal x={300} y={190} active={portalDownActive} />
        <Portal x={790} y={190} active={portalUpActive} />

        {/* ══════════════ Layer 5: Particles (background ambient only) ══════════════ */}

        {/* Merge pulse at Output node when both streams converge */}
        {vizPhase >= 6 && !isComplete && (
          <g>
            <circle cx={820} cy={80} r={8} fill={COL.blue} opacity={0.4} filter="url(#glow)">
              <animate attributeName="r" values="6;12;6" dur="1.5s" repeatCount="indefinite" />
              <animate attributeName="opacity" values="0.3;0.6;0.3" dur="1.5s" repeatCount="indefinite" />
            </circle>
            <circle cx={820} cy={80} r={3} fill={COL.white} opacity={0.8}>
              <animate attributeName="r" values="2;4;2" dur="1.5s" repeatCount="indefinite" />
            </circle>
          </g>
        )}

        {/* Ambient idle particles -- always visible regardless of phase */}
        <AmbientParticles />

        {/* ══════════════ Layer 6: Nodes ══════════════ */}

        {/* Render non-pool nodes (pools are drawn by LiquidPool) */}
        {NODES.filter(
          (n) => n.label !== "L1 AMM" && n.label !== "L2 AMM"
        ).map((node) => (
          <FlowNode
            key={node.label}
            x={node.x}
            y={node.y}
            label={node.label}
            sublabel={node.sublabel}
            chain={node.chain}
            active={activeNodes.has(node.label)}
          />
        ))}

        {/* Pool labels on top of LiquidPool */}
        <text
          x={560}
          y={57}
          textAnchor="middle"
          fill="var(--text)"
          fontSize={11}
          fontWeight={600}
          fontFamily="var(--sans)"
        >
          L1 AMM
        </text>
        <text
          x={600}
          y={267}
          textAnchor="middle"
          fill="var(--text)"
          fontSize={11}
          fontWeight={600}
          fontFamily="var(--sans)"
        >
          L2 AMM
        </text>

        {/* Aggregator breathing effect when idle */}
        {vizPhase === 0 && (
          <rect
            x={240}
            y={55}
            width={120}
            height={50}
            rx={8}
            fill="none"
            stroke="var(--accent)"
            strokeWidth={0.8}
            className={styles.breathing}
          />
        )}

        {/* ══════════════ Layer 7: Overlays ══════════════ */}

        {/* Split shockwave at vizPhase 2 */}
        {vizPhase === 2 && (
          <g>
            <circle
              cx={300}
              cy={80}
              r={10}
              fill="none"
              stroke={COL.cyan}
              strokeWidth={1.5}
              opacity={0.8}
            >
              <animate
                attributeName="r"
                from="10"
                to="60"
                dur="0.4s"
                fill="freeze"
              />
              <animate
                attributeName="opacity"
                from="0.8"
                to="0"
                dur="0.4s"
                fill="freeze"
              />
            </circle>
            {/* Second ring, delayed */}
            <circle
              cx={300}
              cy={80}
              r={10}
              fill="none"
              stroke={COL.cyan}
              strokeWidth={1}
              opacity={0.5}
            >
              <animate
                attributeName="r"
                from="10"
                to="45"
                dur="0.5s"
                begin="0.1s"
                fill="freeze"
              />
              <animate
                attributeName="opacity"
                from="0.5"
                to="0"
                dur="0.5s"
                begin="0.1s"
                fill="freeze"
              />
            </circle>
          </g>
        )}

        {/* Depth counter during bridge crossing phases */}
        {vizPhase >= 3 && vizPhase < 8 && (
          <g>
            <text
              x={545}
              y={192}
              textAnchor="middle"
              fill={COL.cyan}
              fontSize={10}
              fontFamily="var(--mono)"
              opacity={0.8}
              fontWeight={600}
              letterSpacing="0.15em"
            >
              DEPTH: {Math.min(vizPhase + 1, 7)}
            </text>
            {/* Animated dots */}
            <circle cx={490} cy={190} r={1.5} fill={COL.cyan} opacity={0.5}>
              <animate
                attributeName="opacity"
                values="0.3;0.7;0.3"
                dur="1.5s"
                repeatCount="indefinite"
              />
            </circle>
            <circle cx={600} cy={190} r={1.5} fill={COL.cyan} opacity={0.5}>
              <animate
                attributeName="opacity"
                values="0.3;0.7;0.3"
                dur="1.5s"
                begin="0.5s"
                repeatCount="indefinite"
              />
            </circle>
          </g>
        )}

        {/* Route duel improvement badge */}
        {showRouteDuel && improvement && (
          <g>
            <rect
              x={770}
              y={43}
              width={60}
              height={20}
              rx={4}
              fill="rgba(52,211,153,0.15)"
              stroke={COL.green}
              strokeWidth={0.8}
            />
            <text
              x={800}
              y={57}
              textAnchor="middle"
              fill={COL.green}
              fontSize={10}
              fontWeight={700}
              fontFamily="var(--mono)"
            >
              {improvement}
            </text>
          </g>
        )}

        {/* Success state -- ATOMIC badge */}
        {isComplete && (
          <g>
            {/* Glowing success rectangle */}
            <rect
              x={480}
              y={167}
              width={130}
              height={32}
              rx={6}
              fill="rgba(52,211,153,0.08)"
              stroke={COL.green}
              strokeWidth={1.2}
              strokeDasharray="200"
              strokeDashoffset="200"
              filter="url(#bloomFilter)"
              className={styles.atomicBadge}
            />
            <text
              x={545}
              y={188}
              textAnchor="middle"
              fill={COL.green}
              fontSize={12}
              fontWeight={700}
              fontFamily="var(--mono)"
              letterSpacing="0.2em"
              opacity={0}
            >
              ATOMIC
              <animate
                attributeName="opacity"
                from="0"
                to="1"
                dur="0.4s"
                begin="0.3s"
                fill="freeze"
              />
            </text>
            {/* Checkmark */}
            <path
              d="M581,182 L585,187 L592,178"
              fill="none"
              stroke={COL.green}
              strokeWidth={1.8}
              strokeLinecap="round"
              strokeLinejoin="round"
              opacity={0}
            >
              <animate
                attributeName="opacity"
                from="0"
                to="1"
                dur="0.3s"
                begin="0.5s"
                fill="freeze"
              />
            </path>

            {/* Completion pulse rings emanating from center */}
            <circle
              cx={545}
              cy={185}
              r={5}
              fill="none"
              stroke={COL.green}
              strokeWidth={0.8}
              opacity={0}
            >
              <animate
                attributeName="r"
                from="5"
                to="80"
                dur="1.2s"
                fill="freeze"
              />
              <animate
                attributeName="opacity"
                values="0;0.4;0"
                dur="1.2s"
                fill="freeze"
              />
            </circle>
            <circle
              cx={545}
              cy={185}
              r={5}
              fill="none"
              stroke={COL.green}
              strokeWidth={0.5}
              opacity={0}
            >
              <animate
                attributeName="r"
                from="5"
                to="120"
                dur="1.5s"
                begin="0.2s"
                fill="freeze"
              />
              <animate
                attributeName="opacity"
                values="0;0.3;0"
                dur="1.5s"
                begin="0.2s"
                fill="freeze"
              />
            </circle>
          </g>
        )}

        {/* Split percentage indicator near aggregator */}
        {vizPhase >= 2 && vizPhase < 8 && (
          <g>
            {/* Local percentage */}
            <text
              x={430}
              y={66}
              textAnchor="middle"
              fill="rgba(99,102,241,0.7)"
              fontSize={9}
              fontFamily="var(--mono)"
              fontWeight={700}
            >
              {splitPercent}%
            </text>
            {/* Remote percentage */}
            <text
              x={300}
              y={138}
              textAnchor="middle"
              fill="rgba(52,211,153,0.7)"
              fontSize={9}
              fontFamily="var(--mono)"
              fontWeight={700}
            >
              {100 - splitPercent}%
            </text>
          </g>
        )}

        {/* Direction arrows on paths for flow clarity */}
        {vizPhase >= 1 && (
          <g opacity={0.5}>
            {/* Arrow on user->agg path */}
            <polygon points="230,76 224,80 230,84" fill={COL.cyan} opacity={vizPhase >= 1 ? 0.7 : 0.3} />
            {/* Arrow on agg->L1AMM path */}
            <polygon points="490,76 484,80 490,84" fill={COL.cyan} opacity={vizPhase >= 2 ? 0.7 : 0.3} />
            {/* Arrow on L1AMM->output */}
            <polygon points="750,76 744,80 750,84" fill={COL.cyan} opacity={vizPhase >= 4 ? 0.7 : 0.3} />
          </g>
        )}

        {/* Decorative grid dots in bridge zone */}
        {Array.from({ length: 14 }, (_, i) => (
          <circle
            key={`grid-${i}`}
            cx={80 + i * 60}
            cy={185}
            r={1}
            fill={COL.cyan}
            opacity={0.15}
          />
        ))}

        {/* ══════════════ Layer 8: Coordinated journey (TOPMOST) ══════════════
            Particles + labels render LAST so they always paint on top of nodes,
            pools, and pool labels. */}
        <CoordinatedJourney />
      </svg>
    </div>
  );
}

export default CrossChainFlowViz;
