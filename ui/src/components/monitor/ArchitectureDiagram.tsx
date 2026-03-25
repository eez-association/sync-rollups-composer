import React from "react";
import { useMonitorStore } from "../../store";
import { computeLayout, nodeColor, NODE_W, NODE_H } from "../../lib/layout";
import type { ArchNode, ArchEdge } from "../../types/visualization";
import styles from "./ArchitectureDiagram.module.css";

type Props = {
  activeNodes?: Set<string>;
  activeEdges?: Set<string>;
  l1Nodes?: ArchNode[];
  l2Nodes?: ArchNode[];
  edges?: ArchEdge[];
};

export const ArchitectureDiagram: React.FC<Props> = ({
  activeNodes: activeNodesProp,
  activeEdges: activeEdgesProp,
  l1Nodes: l1NodesProp,
  l2Nodes: l2NodesProp,
  edges: edgesProp,
}) => {
  const storeL1Nodes = useMonitorStore((s) => s.l1Nodes);
  const storeL2Nodes = useMonitorStore((s) => s.l2Nodes);
  const storeEdges = useMonitorStore((s) => s.edges);
  const storeActiveNodes = useMonitorStore((s) => s.activeNodes);
  const storeActiveEdges = useMonitorStore((s) => s.activeEdges);
  const l1Nodes = l1NodesProp ?? storeL1Nodes;
  const l2Nodes = l2NodesProp ?? storeL2Nodes;
  const edges = edgesProp ?? storeEdges;
  const activeNodes = activeNodesProp ?? storeActiveNodes;
  const activeEdges = activeEdgesProp ?? storeActiveEdges;

  if (l1Nodes.length === 0 && l2Nodes.length === 0) {
    return (
      <div className={styles.empty}>
        Connect to RPCs to see architecture
      </div>
    );
  }

  const cols = Math.max(l1Nodes.length, l2Nodes.length, 1);
  const arch = { l1: l1Nodes, l2: l2Nodes, cols, edges };
  const { pos, svgW, svgH, laneH, boundaryY } = computeLayout(arch);

  const edgeElements: React.ReactNode[] = [];
  const labelElements: React.ReactNode[] = [];

  for (let ei = 0; ei < edges.length; ei++) {
    const e = edges[ei];
    if (!e) continue;
    const p1 = pos[e.from];
    const p2 = pos[e.to];
    if (!p1 || !p2) continue;

    const edgeId = e.id || `${e.from}->${e.to}`;
    const lit = activeEdges.has(edgeId);
    const stroke = lit ? "var(--cyan)" : "#2a2a3a";
    const op = lit ? 0.85 : 0.12;
    const sw = lit ? 1.5 : 0.7;
    const markerId = lit ? "ah-lit" : "ah-dim";

    if (Math.abs(p1.cy - p2.cy) < 5) {
      const goRight = p1.cx < p2.cx;
      const x1 = goRight ? p1.x + NODE_W : p1.x;
      const x2 = goRight ? p2.x : p2.x + NODE_W;
      const defaultDir = e.back ? 1 : -1;
      const arcDir = e.alt ? -defaultDir : defaultDir;
      const arcH = NODE_H / 2 + (arcDir === 1 ? 12 : 10);
      const midX = (x1 + x2) / 2;
      const y1 = e.back ? p1.y + NODE_H : p1.y;
      const y2 = e.back ? p2.y + NODE_H : p2.y;
      const midY = p1.cy + arcDir * arcH;

      edgeElements.push(
        <path
          key={`edge-${ei}`}
          d={`M${x1},${y1} Q${midX},${midY} ${x2},${y2}`}
          stroke={stroke}
          strokeWidth={sw}
          fill="none"
          opacity={op}
          markerEnd={`url(#${markerId})`}
        />,
      );

      if (e.label && lit) {
        const lblY = midY + arcDir * 4;
        labelElements.push(
          <text
            key={`lbl-${ei}`}
            x={midX}
            y={lblY}
            textAnchor="middle"
            fill="var(--cyan)"
            fontSize={7}
            fontFamily="monospace"
            fontWeight={700}
            opacity={0.95}
          >
            {e.label}
          </text>,
        );
      }
    } else {
      const x1 = p1.cx;
      const x2 = p2.cx;
      const y1 = p1.cy < p2.cy ? p1.y + NODE_H : p1.y;
      const y2 = p2.cy < p1.cy ? p2.y + NODE_H : p2.y;

      edgeElements.push(
        <line
          key={`edge-${ei}`}
          x1={x1}
          y1={y1}
          x2={x2}
          y2={y2}
          stroke={stroke}
          strokeWidth={sw}
          opacity={op}
          markerEnd={`url(#${markerId})`}
        />,
      );

      if (e.label && lit) {
        labelElements.push(
          <text
            key={`lbl-${ei}`}
            x={(x1 + x2) / 2}
            y={(y1 + y2) / 2 - 4}
            textAnchor="middle"
            fill="var(--cyan)"
            fontSize={7}
            fontFamily="monospace"
            fontWeight={700}
            opacity={0.95}
          >
            {e.label}
          </text>,
        );
      }
    }
  }

  const allNodes = [
    ...l1Nodes.map((n) => ({ ...n, chain: "l1" as const })),
    ...l2Nodes.map((n) => ({ ...n, chain: "l2" as const })),
  ];

  const nodeElements = allNodes.map((n) => {
    const p = pos[n.id];
    if (!p) return null;

    const lit = activeNodes.has(n.id);
    const col = nodeColor(n.type, n.chain);
    const isGhost = n.type === "ghost";
    const baseOp = lit ? (isGhost ? 0.7 : 1) : isGhost ? 0.08 : 0.25;
    const isDashed = n.type === "proxy" || isGhost;

    return (
      <g
        key={n.id}
        opacity={baseOp}
        style={{
          filter: lit ? `drop-shadow(0 0 8px ${col})` : undefined,
          transition: "opacity 0.3s, filter 0.3s",
        }}
      >
        <rect
          x={p.x}
          y={p.y}
          width={NODE_W}
          height={NODE_H}
          rx={5}
          fill={lit ? "rgba(20,20,35,0.9)" : "rgba(15,15,25,0.6)"}
          stroke={col}
          strokeWidth={lit ? 1.8 : 1}
          strokeDasharray={isDashed ? "4 2" : undefined}
        />
        <text
          x={p.cx}
          y={p.y + 16}
          textAnchor="middle"
          fill={lit ? "var(--text)" : "var(--text-dim)"}
          fontSize={9}
          fontWeight={700}
          fontFamily="monospace"
        >
          {n.label}
        </text>
        {n.sub && (
          <text
            x={p.cx}
            y={p.y + 27}
            textAnchor="middle"
            fill={lit ? "var(--text-dim)" : "#4a4a60"}
            fontSize={6}
            fontFamily="monospace"
          >
            {n.sub}
          </text>
        )}
      </g>
    );
  });

  return (
    <div className={styles.container}>
      <svg
        viewBox={`0 0 ${svgW} ${svgH}`}
        className={styles.svg}
      >
        <defs>
          <marker
            id="ah-dim"
            viewBox="0 0 10 7"
            refX={9}
            refY={3.5}
            markerWidth={7}
            markerHeight={5}
            orient="auto-start-reverse"
          >
            <path d="M0,0 L10,3.5 L0,7z" fill="#3a3a50" />
          </marker>
          <marker
            id="ah-lit"
            viewBox="0 0 10 7"
            refX={9}
            refY={3.5}
            markerWidth={7}
            markerHeight={5}
            orient="auto-start-reverse"
          >
            <path d="M0,0 L10,3.5 L0,7z" fill="var(--cyan)" />
          </marker>
        </defs>

        <rect
          x={0}
          y={0}
          width={svgW}
          height={laneH}
          rx={6}
          fill="rgba(59,130,246,0.04)"
          stroke="rgba(59,130,246,0.12)"
          strokeWidth={1}
        />
        <rect
          x={0}
          y={laneH + 28}
          width={svgW}
          height={laneH}
          rx={6}
          fill="rgba(168,85,247,0.04)"
          stroke="rgba(168,85,247,0.12)"
          strokeWidth={1}
        />

        <line
          x1={10}
          y1={boundaryY}
          x2={svgW - 10}
          y2={boundaryY}
          stroke="#2a2a3a"
          strokeWidth={0.5}
          strokeDasharray="3 4"
          opacity={0.4}
        />

        <text x={6} y={10} fill="#3b82f6" fontSize={7} fontFamily="monospace" fontWeight={700} opacity={0.6}>
          L1
        </text>
        <text x={6} y={laneH + 28 + 10} fill="#a855f7" fontSize={7} fontFamily="monospace" fontWeight={700} opacity={0.6}>
          L2
        </text>

        {edgeElements}
        {nodeElements}
        {labelElements}
      </svg>
    </div>
  );
};
