import { useMemo } from "react";
import styles from "./ChainStrip.module.css";

interface ChainSlotProps {
  label: "L1" | "L2";
  blockNumber: number | null;
  txCount?: number | null;
  gasUsed?: number | null;
  gasLimit?: number | null;
  timestamp?: number | null;
  synced?: boolean | null;
}

function formatAge(timestamp: number): string {
  const age = Math.floor(Date.now() / 1000) - timestamp;
  if (age < 0) return "just now";
  if (age < 60) return `${age}s ago`;
  if (age < 3600) return `${Math.floor(age / 60)}m ago`;
  return `${Math.floor(age / 3600)}h ago`;
}

function formatGasCompact(used: number, limit: number): string {
  const usedM = (used / 1e6).toFixed(1);
  if (limit > 0) {
    const pct = Math.round((used / limit) * 100);
    return `${usedM}M gas (${pct}%)`;
  }
  return `${usedM}M gas`;
}

function ChainSlot({ label, blockNumber, txCount, gasUsed, gasLimit, timestamp, synced }: ChainSlotProps) {
  const age = useMemo(
    () => (timestamp ? formatAge(timestamp) : null),
    [timestamp],
  );

  const gas = useMemo(
    () =>
      gasUsed != null && gasLimit != null && gasLimit > 0
        ? formatGasCompact(gasUsed, gasLimit)
        : null,
    [gasUsed, gasLimit],
  );

  const isL1 = label === "L1";

  return (
    <div className={`${styles.slot} ${isL1 ? styles.slotL1 : styles.slotL2}`}>
      {/* Chain label pill */}
      <span className={`${styles.chainPill} ${isL1 ? styles.pillL1 : styles.pillL2}`}>
        {label}
      </span>

      {/* Block number — the primary number */}
      <span className={styles.blockNum}>
        {blockNumber !== null ? `#${blockNumber.toLocaleString()}` : "—"}
      </span>

      {/* Secondary meta row */}
      <span className={styles.metaRow}>
        {txCount != null && (
          <span className={styles.metaPiece}>
            {txCount} {txCount === 1 ? "tx" : "txs"}
          </span>
        )}
        {gas && (
          <>
            <span className={styles.metaDot} />
            <span className={styles.metaPiece}>{gas}</span>
          </>
        )}
        {age && (
          <>
            <span className={styles.metaDot} />
            <span className={styles.metaPiece}>{age}</span>
          </>
        )}
      </span>

      {/* Sync badge — L2 only */}
      {!isL1 && synced !== undefined && synced !== null && (
        <span className={`${styles.syncBadge} ${synced ? styles.synced : styles.syncing}`}>
          <span className={styles.pulse} />
          {synced ? "SYNCED" : "SYNCING"}
        </span>
      )}
    </div>
  );
}

interface Props {
  l1: {
    blockNumber: number | null;
    txCount?: number | null;
    gasUsed?: number | null;
    gasLimit?: number | null;
    timestamp?: number | null;
  };
  l2: {
    blockNumber: number | null;
    txCount?: number | null;
    gasUsed?: number | null;
    gasLimit?: number | null;
    timestamp?: number | null;
    synced?: boolean | null;
  };
}

export function ChainStrip({ l1, l2 }: Props) {
  return (
    <div className={styles.strip}>
      <ChainSlot label="L1" {...l1} />
      <span className={styles.divider} />
      <ChainSlot label="L2" {...l2} />
    </div>
  );
}
