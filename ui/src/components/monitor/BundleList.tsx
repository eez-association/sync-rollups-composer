import React, { useMemo, useState } from "react";
import { useMonitorStore } from "../../store";
import { buildTransactionBundles } from "../../lib/crossChainCorrelation";
import { truncateHex } from "../../lib/actionFormatter";
import type { TransactionBundle, BundleDirection } from "../../types/visualization";
import styles from "./BundleList.module.css";

const DIRECTION_COLORS: Record<BundleDirection, string> = {
  "L1->L2": "#3b82f6",
  "L2->L1": "#a855f7",
  "L1->L2->L1": "var(--yellow)",
  "L2->L1->L2": "var(--yellow)",
  "L1": "#3b82f6",
  "L2": "#a855f7",
  "mixed": "var(--text-dim)",
};

type Props = {
  onSelectBundle: (bundle: TransactionBundle) => void;
  selectedBundleId: string | null;
};

export const BundleList: React.FC<Props> = ({ onSelectBundle, selectedBundleId }) => {
  const events = useMonitorStore((s) => s.events);

  const bundles = useMemo(() => buildTransactionBundles(events), [events]);

  const significantBundles = useMemo(
    () => bundles.filter((b) => b.events.length > 1 || b.actionHashes.length > 0),
    [bundles],
  );

  if (significantBundles.length === 0) {
    return <div className={styles.empty}>No cross-chain bundles yet...</div>;
  }

  return (
    <div className={styles.root}>
      {significantBundles.map((bundle) => (
        <BundleCard
          key={bundle.id}
          bundle={bundle}
          selected={bundle.id === selectedBundleId}
          onClick={() => onSelectBundle(bundle)}
        />
      ))}
    </div>
  );
};

const BundleCard: React.FC<{
  bundle: TransactionBundle;
  selected: boolean;
  onClick: () => void;
}> = ({ bundle, selected, onClick }) => {
  const [hovered, setHovered] = useState(false);
  const dirColor = DIRECTION_COLORS[bundle.direction];
  const isComplete = bundle.status === "complete";

  return (
    <div
      onClick={onClick}
      onMouseEnter={() => setHovered(true)}
      onMouseLeave={() => setHovered(false)}
      className={`${styles.card} ${selected ? styles.cardSelected : ""} ${hovered && !selected ? styles.cardHover : ""}`}
    >
      <div
        style={{
          flexShrink: 0,
          padding: "2px 6px",
          borderRadius: 4,
          fontSize: "0.48rem",
          fontWeight: 700,
          background: `${dirColor}15`,
          color: dirColor,
          border: `1px solid ${dirColor}30`,
          whiteSpace: "nowrap",
          marginTop: 1,
        }}
      >
        {bundle.direction}
      </div>

      <div className={styles.cardBody}>
        <div className={styles.cardTitle}>{bundle.title}</div>
        <div className={styles.cardMeta}>
          <span>{bundle.events.length} events</span>
          <span>{bundle.actionHashes.length} action{bundle.actionHashes.length !== 1 ? "s" : ""}</span>
          <span>
            block {bundle.blockRange.from.toString()}
            {bundle.blockRange.to !== bundle.blockRange.from && `-${bundle.blockRange.to.toString()}`}
          </span>
        </div>

        {bundle.actionHashes.length > 0 && (
          <div className={styles.hashList}>
            {bundle.actionHashes.slice(0, 3).map((h) => (
              <span key={h} className={styles.hashBadge}>
                {truncateHex(h, 8)}
              </span>
            ))}
            {bundle.actionHashes.length > 3 && (
              <span style={{ fontSize: "0.45rem", color: "var(--text-dim)" }}>
                +{bundle.actionHashes.length - 3} more
              </span>
            )}
          </div>
        )}

        <div className={styles.txList}>
          {[...bundle.txHashes].slice(0, 2).map((h) => (
            <span key={h} style={{ marginRight: 6 }}>
              tx: {truncateHex(h, 8)}
            </span>
          ))}
          {bundle.txHashes.size > 2 && (
            <span>+{bundle.txHashes.size - 2} more</span>
          )}
        </div>
      </div>

      <div
        className={styles.statusDot}
        style={{
          background: isComplete ? "var(--green)" : "var(--yellow)",
          boxShadow: `0 0 6px ${isComplete ? "rgba(52,211,153,0.4)" : "rgba(245,158,11,0.4)"}`,
        }}
        title={isComplete ? "Complete" : "In progress"}
      />
    </div>
  );
};
