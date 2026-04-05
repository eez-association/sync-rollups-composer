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
  userToAgg: "M140,70 L180,70",
  aggToL1Amm: "M300,70 L420,70",
  l1AmmToOutput: "M540,70 L660,70",
  aggToPortalDown: "M240,95 C240,140 240,170 240,190",
  portalToL2Exec: "M240,190 C240,230 270,280 300,300",
  l2ExecToL2Amm: "M360,300 L460,300",
  l2AmmToPortalUp: "M580,300 C600,270 680,230 720,190",
  portalToOutput: "M720,190 C720,160 720,120 720,95",
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
  { x: 80, y: 70, label: "User", sublabel: "Wallet", chain: "l1" },
  { x: 240, y: 70, label: "Aggregator", sublabel: "Split Router", chain: "l1" },
  { x: 480, y: 70, label: "L1 AMM", sublabel: "Uniswap V2", chain: "l1" },
  { x: 720, y: 70, label: "Output", sublabel: "Best Price", chain: "l1" },
  { x: 300, y: 300, label: "L2 Executor", sublabel: "Cross-Chain", chain: "l2" },
  { x: 520, y: 300, label: "L2 AMM", sublabel: "Remote Pool", chain: "l2" },
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

/* ── Particle Stream ── */

interface ParticleStreamProps {
  pathData: string;
  color: string;
  active: boolean;
  count?: number;
  speed?: number;
}

function ParticleStream({
  pathData,
  color,
  active,
  count = 5,
  speed = 1.0,
}: ParticleStreamProps) {
  if (!active) return null;
  return (
    <g>
      {Array.from({ length: count }, (_, i) => {
        const dur = `${speed + i * 0.15}s`;
        const begin = `${i * 0.18}s`;
        return (
          <g key={i}>
            {/* Plasma tail — outermost, soft, displaced */}
            <circle r={7} fill={color} opacity={0.12} filter="url(#plasma)">
              <animateMotion
                dur={dur}
                begin={begin}
                repeatCount="indefinite"
                path={pathData}
              />
            </circle>
            {/* Color halo — mid-layer glow */}
            <circle r={4} fill={color} opacity={0.35} filter="url(#glow)">
              <animateMotion
                dur={dur}
                begin={begin}
                repeatCount="indefinite"
                path={pathData}
              />
            </circle>
            {/* White core — crisp center */}
            <circle r={1.5} fill={COL.white} opacity={0.9}>
              <animateMotion
                dur={dur}
                begin={begin}
                repeatCount="indefinite"
                path={pathData}
              />
            </circle>
          </g>
        );
      })}
    </g>
  );
}

/* ── Ambient idle particles — slow drifting specks ── */

function AmbientParticles() {
  return (
    <g>
      {/* Slow drifter along reversed L1 path */}
      <circle r={2.5} fill={COL.cyan} opacity={0.2} filter="url(#glow)">
        <animateMotion
          dur="6s"
          repeatCount="indefinite"
          path="M660,70 L300,70"
        />
      </circle>
      {/* Slow drifter along reversed L2 path */}
      <circle r={2} fill={COL.green} opacity={0.18} filter="url(#glow)">
        <animateMotion
          dur="8s"
          repeatCount="indefinite"
          path="M460,300 L360,300"
        />
      </circle>
      {/* Floating bridge zone particle */}
      <circle r={1.5} fill={COL.cyan} opacity={0.25} filter="url(#glow)">
        <animateMotion
          dur="10s"
          repeatCount="indefinite"
          path="M200,190 C400,180 600,200 760,190"
        />
      </circle>
      {/* Extra drifter along portal-to-portal arc */}
      <circle r={1.5} fill={COL.cyan} opacity={0.15} filter="url(#glow)">
        <animateMotion
          dur="9s"
          repeatCount="indefinite"
          path="M720,190 C600,160 400,200 240,190"
        />
      </circle>
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
  const a = reserveA ? parseFloat(reserveA) : 0;
  const b = reserveB ? parseFloat(reserveB) : 0;
  const total = a + b;
  const ratioA = total > 0 ? a / total : 0.5;

  // Pool inner height is 46px (50 - 4px for padding visual), liquid fills from bottom
  const innerH = 46;
  const liquidAHeight = ratioA * innerH;
  const liquidBHeight = (1 - ratioA) * innerH;

  const borderColor = chain === "l1"
    ? (active ? "rgba(99,102,241,0.6)" : "rgba(99,102,241,0.25)")
    : (active ? "rgba(52,211,153,0.6)" : "rgba(52,211,153,0.25)");

  // Wave path for the liquid surface
  const waveY = y - 25 + 2 + liquidBHeight;

  return (
    <g>
      {/* Glass container outer */}
      <rect
        x={x - 60}
        y={y - 25}
        width={120}
        height={50}
        rx={6}
        fill="rgba(18,18,28,0.6)"
        stroke={borderColor}
        strokeWidth={active ? 1.2 : 0.6}
      />

      {/* Token B fill (blue/USDC) — top portion */}
      <rect
        x={x - 58}
        y={y - 23}
        width={116}
        height={liquidBHeight}
        rx={4}
        fill={COL.blue}
        opacity={0.15}
      />

      {/* Token A fill (gold/WETH) — bottom portion */}
      <rect
        x={x - 58}
        y={y - 23 + liquidBHeight}
        width={116}
        height={liquidAHeight}
        rx={4}
        fill={COL.gold}
        opacity={0.2}
      />

      {/* Animated wave surface at the A/B boundary */}
      <g opacity={0.4}>
        <path
          d={`M${x - 58},${waveY} q15,-3 30,0 t30,0 t30,0 t30,0`}
          fill="none"
          stroke={COL.gold}
          strokeWidth={0.8}
          opacity={0.6}
        >
          <animateTransform
            attributeName="transform"
            type="translate"
            values="0,0; -15,0; 0,0"
            dur="3s"
            repeatCount="indefinite"
          />
        </path>
        <path
          d={`M${x - 58},${waveY + 1} q12,2 24,0 t24,0 t24,0 t24,0 t24,0`}
          fill="none"
          stroke={COL.blue}
          strokeWidth={0.5}
          opacity={0.4}
        >
          <animateTransform
            attributeName="transform"
            type="translate"
            values="0,0; -10,0; 0,0"
            dur="4.5s"
            repeatCount="indefinite"
          />
        </path>
      </g>

      {/* Reserve labels */}
      {reserveA !== null && (
        <text
          x={x - 52}
          y={y + 18}
          fill={COL.gold}
          fontSize={7}
          fontFamily="var(--mono)"
          opacity={0.7}
        >
          A: {formatReserve(reserveA)}
        </text>
      )}
      {reserveB !== null && (
        <text
          x={x + 8}
          y={y + 18}
          fill={COL.blue}
          fontSize={7}
          fontFamily="var(--mono)"
          opacity={0.7}
        >
          B: {formatReserve(reserveB)}
        </text>
      )}

      {/* Shimmer overlay when active */}
      {active && (
        <rect
          x={x - 58}
          y={y - 23}
          width={116}
          height={46}
          rx={4}
          fill="none"
          stroke={chain === "l1" ? "rgba(99,102,241,0.3)" : "rgba(52,211,153,0.3)"}
          strokeWidth={0.5}
        >
          <animate
            attributeName="opacity"
            values="0.3;0.7;0.3"
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
  const strokeColor = complete
    ? "var(--green)"
    : color ?? (active ? "var(--cyan)" : "rgba(99,102,241,0.4)");
  const effectiveWidth = Math.max(1.5, width);
  return (
    <g>
      {/* Glow layer behind active paths */}
      {active && !complete && (
        <path
          d={d}
          fill="none"
          stroke={strokeColor}
          strokeWidth={effectiveWidth * 3}
          opacity={0.15}
          strokeLinecap="round"
          filter="url(#glow)"
        />
      )}
      <path
        d={d}
        fill="none"
        stroke={strokeColor}
        strokeWidth={effectiveWidth}
        strokeDasharray={dashed ? "6 4" : undefined}
        strokeLinecap="round"
        opacity={complete ? 0.8 : active ? 0.8 : 0.3}
        className={complete ? styles.successPath : undefined}
      />
    </g>
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
        viewBox="0 0 960 380"
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
          height={165}
          fill="url(#l1LaneGrad)"
          rx={0}
        />

        {/* L2 lane */}
        <rect
          x={0}
          y={215}
          width={960}
          height={165}
          fill="url(#l2LaneGrad)"
          rx={0}
        />

        {/* Bridge zone */}
        <rect
          x={0}
          y={165}
          width={960}
          height={50}
          fill="url(#scanlines)"
          opacity={0.8}
        />

        {/* Bridge zone dashed borders */}
        <line
          x1={40}
          y1={165}
          x2={920}
          y2={165}
          stroke={COL.cyan}
          strokeWidth={0.5}
          strokeDasharray="12 8"
          opacity={0.15}
        />
        <line
          x1={40}
          y1={215}
          x2={920}
          y2={215}
          stroke={COL.cyan}
          strokeWidth={0.5}
          strokeDasharray="12 8"
          opacity={0.15}
        />

        {/* Lane labels */}
        <text
          x={22}
          y={20}
          fill="rgba(99,102,241,0.3)"
          fontSize={9}
          fontFamily="var(--mono)"
          fontWeight={600}
          letterSpacing="0.12em"
        >
          L1
        </text>
        <text
          x={22}
          y={240}
          fill="rgba(52,211,153,0.3)"
          fontSize={9}
          fontFamily="var(--mono)"
          fontWeight={600}
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
          width={2}
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

        {/* Route duel ghost path — direct L1-only route shown as red dashed */}
        {showRouteDuel && (
          <RoutePath
            d="M140,55 L660,55"
            active={true}
            complete={false}
            width={1.5}
            dashed={true}
            color="var(--red)"
          />
        )}

        {/* ══════════════ Layer 3: Liquid Pools ══════════════ */}

        <LiquidPool
          x={480}
          y={70}
          reserveA={l1ReserveA}
          reserveB={l1ReserveB}
          chain="l1"
          active={vizPhase >= 2 && vizPhase <= 4}
        />
        <LiquidPool
          x={520}
          y={300}
          reserveA={l2ReserveA}
          reserveB={l2ReserveB}
          chain="l2"
          active={vizPhase >= 5 && vizPhase <= 6}
        />

        {/* ══════════════ Layer 4: Bridge Portals ══════════════ */}

        <Portal x={240} y={190} active={portalDownActive} />
        <Portal x={720} y={190} active={portalUpActive} />

        {/* ══════════════ Layer 5: Particles ══════════════ */}

        {/* Phase 1: User -> Aggregator */}
        <ParticleStream
          pathData={PATHS.userToAgg}
          color={COL.gold}
          active={vizPhase >= 1 && !isComplete}
          speed={0.6}
          count={3}
        />

        {/* Phase 2: Aggregator -> L1 AMM (local split) */}
        <ParticleStream
          pathData={PATHS.aggToL1Amm}
          color={COL.gold}
          active={vizPhase >= 2 && !isComplete}
          speed={0.8}
          count={Math.max(2, Math.round(splitPercent / 20))}
        />

        {/* Phase 3: Aggregator -> Portal (down) */}
        <ParticleStream
          pathData={PATHS.aggToPortalDown}
          color={COL.gold}
          active={vizPhase >= 3 && !isComplete}
          speed={0.7}
          count={3}
        />
        {/* Portal crossing particles — cyan during bridge */}
        <ParticleStream
          pathData={PATHS.portalToL2Exec}
          color={portalDownActive ? COL.cyan : COL.gold}
          active={vizPhase >= 3 && !isComplete}
          speed={0.9}
          count={4}
        />

        {/* Phase 4: L1 AMM -> Output */}
        <ParticleStream
          pathData={PATHS.l1AmmToOutput}
          color={COL.blue}
          active={vizPhase >= 4 && !isComplete}
          speed={0.8}
          count={3}
        />

        {/* Phase 5: L2 Executor -> L2 AMM (color transition) */}
        <ParticleStream
          pathData={PATHS.l2ExecToL2Amm}
          color={COL.gold}
          active={vizPhase >= 5 && !isComplete}
          speed={0.8}
          count={3}
        />
        {/* Overlaid blue particles for transition effect */}
        <ParticleStream
          pathData={PATHS.l2ExecToL2Amm}
          color={COL.blue}
          active={vizPhase >= 5 && !isComplete}
          speed={1.1}
          count={2}
        />

        {/* Phase 6: L2 AMM -> Portal (up) -> Output */}
        <ParticleStream
          pathData={PATHS.l2AmmToPortalUp}
          color={portalUpActive ? COL.cyan : COL.blue}
          active={vizPhase >= 6 && !isComplete}
          speed={1.0}
          count={4}
        />
        <ParticleStream
          pathData={PATHS.portalToOutput}
          color={COL.blue}
          active={vizPhase >= 6 && !isComplete}
          speed={0.7}
          count={3}
        />

        {/* Ambient idle particles — always visible regardless of phase */}
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
          x={480}
          y={47}
          textAnchor="middle"
          fill="var(--text)"
          fontSize={11}
          fontWeight={600}
          fontFamily="var(--sans)"
        >
          L1 AMM
        </text>
        <text
          x={520}
          y={277}
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
            x={180}
            y={45}
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
              cx={240}
              cy={70}
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
              cx={240}
              cy={70}
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
              x={480}
              y={192}
              textAnchor="middle"
              fill={COL.cyan}
              fontSize={10}
              fontFamily="var(--mono)"
              opacity={0.7}
              fontWeight={500}
              letterSpacing="0.15em"
            >
              DEPTH: {Math.min(vizPhase + 1, 7)}
            </text>
            {/* Animated dots */}
            <circle cx={430} cy={190} r={1.5} fill={COL.cyan} opacity={0.4}>
              <animate
                attributeName="opacity"
                values="0.2;0.6;0.2"
                dur="1.5s"
                repeatCount="indefinite"
              />
            </circle>
            <circle cx={530} cy={190} r={1.5} fill={COL.cyan} opacity={0.4}>
              <animate
                attributeName="opacity"
                values="0.2;0.6;0.2"
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
              x={680}
              y={33}
              width={60}
              height={20}
              rx={4}
              fill="rgba(52,211,153,0.15)"
              stroke="var(--green)"
              strokeWidth={0.8}
            />
            <text
              x={710}
              y={47}
              textAnchor="middle"
              fill="var(--green)"
              fontSize={10}
              fontWeight={700}
              fontFamily="var(--mono)"
            >
              {improvement}
            </text>
          </g>
        )}

        {/* Success state — ATOMIC badge */}
        {isComplete && (
          <g>
            {/* Glowing success rectangle */}
            <rect
              x={415}
              y={157}
              width={130}
              height={32}
              rx={6}
              fill="rgba(52,211,153,0.08)"
              stroke="var(--green)"
              strokeWidth={1.2}
              strokeDasharray="200"
              strokeDashoffset="200"
              filter="url(#bloomFilter)"
              className={styles.atomicBadge}
            />
            <text
              x={480}
              y={178}
              textAnchor="middle"
              fill="var(--green)"
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
              d="M516,172 L520,177 L527,168"
              fill="none"
              stroke="var(--green)"
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
              cx={480}
              cy={190}
              r={5}
              fill="none"
              stroke="var(--green)"
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
              cx={480}
              cy={190}
              r={5}
              fill="none"
              stroke="var(--green)"
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
              x={340}
              y={56}
              textAnchor="middle"
              fill="rgba(99,102,241,0.6)"
              fontSize={8}
              fontFamily="var(--mono)"
              fontWeight={600}
            >
              {splitPercent}%
            </text>
            {/* Remote percentage */}
            <text
              x={240}
              y={128}
              textAnchor="middle"
              fill="rgba(52,211,153,0.6)"
              fontSize={8}
              fontFamily="var(--mono)"
              fontWeight={600}
            >
              {100 - splitPercent}%
            </text>
          </g>
        )}

        {/* Decorative grid dots in bridge zone */}
        {Array.from({ length: 12 }, (_, i) => (
          <circle
            key={`grid-${i}`}
            cx={120 + i * 65}
            cy={190}
            r={0.8}
            fill={COL.cyan}
            opacity={0.12}
          />
        ))}
      </svg>
    </div>
  );
}

export default CrossChainFlowViz;
