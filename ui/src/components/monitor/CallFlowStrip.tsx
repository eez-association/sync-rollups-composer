import React from "react";
import type { DiagramItem } from "../../types/visualization";
import styles from "./CallFlowStrip.module.css";

type Props = {
  items: DiagramItem[];
};

export const CallFlowStrip: React.FC<Props> = ({ items }) => {
  if (items.length === 0) return null;

  return (
    <div className={styles.root}>
      <div className={styles.title}>Call flow detail</div>
      <div className={styles.strip}>
        {items.map((item, i) => {
          if (item.kind === "arrow") {
            return <ArrowItem key={i} label={item.label} />;
          }
          return (
            <FlowNode
              key={i}
              label={item.label}
              sub={item.sub}
              type={item.type}
              chain={item.chain}
            />
          );
        })}
      </div>
    </div>
  );
};

const FlowNode: React.FC<{
  label: string;
  sub: string;
  type: string;
  chain: string;
}> = ({ label, sub, type, chain }) => {
  const borderColor =
    type === "user"
      ? "#666"
      : type === "system"
        ? chain === "l1"
          ? "#3b82f6"
          : "#a855f7"
        : type === "contract"
          ? chain === "l1"
            ? "#3b82f6"
            : "#a855f7"
          : type === "proxy"
            ? chain === "l1"
              ? "rgba(59,130,246,0.5)"
              : "rgba(168,85,247,0.5)"
            : "var(--border)";

  const bg =
    type === "system"
      ? chain === "l1"
        ? "rgba(59,130,246,0.06)"
        : "rgba(168,85,247,0.06)"
      : type === "user"
        ? "var(--bg-inset)"
        : "var(--bg-inset)";

  return (
    <div
      className={styles.flowNode}
      style={{
        border: `1.5px ${type === "proxy" ? "dashed" : "solid"} ${borderColor}`,
        background: bg,
      }}
    >
      <div className={styles.flowNodeLabel}>{label}</div>
      {sub && <div className={styles.flowNodeSub}>{sub}</div>}
    </div>
  );
};

const ArrowItem: React.FC<{ label: string }> = ({ label }) => {
  const arrowW = 60;
  const lineLen = 48;

  return (
    <div className={styles.arrowItem}>
      <div className={styles.arrowLabel}>{label}</div>
      <svg
        width={arrowW}
        height={10}
        viewBox={`0 0 ${arrowW} 10`}
        style={{ display: "block" }}
      >
        <line
          x1={4}
          y1={5}
          x2={lineLen}
          y2={5}
          stroke="var(--text-dim)"
          strokeWidth={1.5}
        />
        <polygon
          points={`${lineLen - 1},1 ${lineLen + 6},5 ${lineLen - 1},9`}
          fill="var(--text-dim)"
        />
      </svg>
    </div>
  );
};
