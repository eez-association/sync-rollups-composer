import { useMemo } from "react";
import { ExplorerLink } from "./ExplorerLink";
import styles from "./ChainCard.module.css";

interface Props {
  title: string;
  chain?: "l1" | "l2";
  blockNumber: number | null;
  timestamp?: number | null;
  gasUsed?: number | null;
  gasLimit?: number | null;
  txCount?: number | null;
  gasPrice?: string | null;
  stateRoot?: string;
  stateRootLabel?: string;
  synced?: boolean | null;
  extraAddresses?: { label: string; value: string }[];
  stateMatch?: React.ReactNode;
}

function truncate(s: string) {
  if (!s || s.length <= 20) return s;
  return s.slice(0, 10) + "\u2026" + s.slice(-8);
}

function CopyableValue({ value, small }: { value: string; small?: boolean }) {
  const copy = () => {
    if (value && value !== "\u2014") {
      navigator.clipboard.writeText(value);
    }
  };

  return (
    <div
      className={`${styles.statValue} ${small ? styles.mono : ""}`}
      onClick={copy}
      title={value ? `Click to copy: ${value}` : undefined}
      style={{ cursor: value ? "pointer" : "default" }}
    >
      {value ? truncate(value) : "\u2014"}
    </div>
  );
}

function formatAge(timestamp: number): string {
  const age = Math.floor(Date.now() / 1000) - timestamp;
  if (age < 0) return "just now";
  if (age < 60) return `${age}s ago`;
  if (age < 3600) return `${Math.floor(age / 60)}m ago`;
  return `${Math.floor(age / 3600)}h ago`;
}

function formatGas(used: number, limit: number): string {
  const pct = ((used / limit) * 100).toFixed(1);
  const usedM = (used / 1e6).toFixed(1);
  return `${usedM}M (${pct}%)`;
}

export function ChainCard({
  title,
  chain = "l2",
  blockNumber,
  timestamp,
  gasUsed,
  gasLimit,
  txCount,
  gasPrice,
  stateRoot,
  stateRootLabel,
  synced,
  extraAddresses,
  stateMatch,
}: Props) {
  const blockAge = useMemo(
    () => (timestamp ? formatAge(timestamp) : null),
    [timestamp],
  );

  const gasDisplay = useMemo(
    () =>
      gasUsed != null && gasLimit != null && gasLimit > 0
        ? formatGas(gasUsed, gasLimit)
        : null,
    [gasUsed, gasLimit],
  );

  return (
    <div
      className={styles.card}
      style={{ "--card-accent": chain === "l1" ? "var(--l1-accent)" : "var(--l2-accent)" } as React.CSSProperties}
    >
      <div className={styles.cardHeader}>
        <span className={styles.cardTitle}>{title}</span>
        {blockAge && <span className={styles.blockAge}>{blockAge}</span>}
      </div>

      {/* Block number + inline stats row */}
      <div className={styles.stat}>
        <div className={styles.statLabel}>Block Number</div>
        <div className={styles.blockRow}>
          <span className={styles.blockNum}>
            {blockNumber !== null ? blockNumber.toLocaleString() : "\u2014"}
          </span>
          {(txCount != null || gasDisplay || gasPrice) && (
            <span className={styles.blockMeta}>
              {txCount != null && (
                <span className={styles.metaItem}>
                  {txCount} tx{txCount !== 1 ? "s" : ""}
                </span>
              )}
              {gasDisplay && (
                <span className={styles.metaItem}>{gasDisplay} gas</span>
              )}
              {gasPrice && (
                <span className={styles.metaItem}>{gasPrice} gwei</span>
              )}
            </span>
          )}
        </div>
      </div>

      {extraAddresses?.map((addr) => (
        <div className={styles.stat} key={addr.label}>
          <div className={styles.statLabel}>{addr.label}</div>
          {addr.value ? (
            <ExplorerLink value={addr.value} chain={chain} className={`${styles.statValue} ${styles.mono}`} />
          ) : (
            <div className={styles.statValue}>{"\u2014"}</div>
          )}
        </div>
      ))}

      {stateRoot !== undefined && (
        <div className={styles.stat}>
          <div className={styles.statLabel}>
            {stateRootLabel || "State Root"}
          </div>
          <CopyableValue value={stateRoot} small />
        </div>
      )}

      {synced !== undefined && synced !== null && (
        <div className={styles.stat}>
          <div className={styles.statLabel}>Sync Status</div>
          <div className={styles.statValue}>
            {synced ? (
              <span className={`${styles.syncBadge} ${styles.synced}`}>
                <span className={styles.pulse} />
                SYNCED
              </span>
            ) : (
              <span className={`${styles.syncBadge} ${styles.syncing}`}>
                <span className={styles.pulse} />
                SYNCING
              </span>
            )}
          </div>
        </div>
      )}

      {stateMatch && <div className={styles.stateMatch}>{stateMatch}</div>}
    </div>
  );
}
