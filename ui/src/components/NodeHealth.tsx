import { useMemo, useState, useEffect } from "react";
import type { HealthData } from "../hooks/useHealth";
import { config } from "../config";
import styles from "./NodeHealth.module.css";

interface ChainData {
  blockNumber: number | null;
  txCount?: number | null;
  gasUsed?: number | null;
  gasLimit?: number | null;
  timestamp?: number | null;
  synced?: boolean | null;
}

interface Props {
  health: HealthData | null;
  l1?: ChainData;
  l2?: ChainData;
}

function formatAge(ts: number, now: number): string {
  const age = now - ts;
  if (age < 0) return "now";
  if (age < 60) return `${age}s`;
  if (age < 3600) return `${Math.floor(age / 60)}m`;
  return `${Math.floor(age / 3600)}h`;
}

function formatGas(used: number, limit: number): string {
  const m = (used / 1e6).toFixed(1);
  return limit > 0 ? `${m}M (${Math.round((used / limit) * 100)}%)` : `${m}M`;
}

function ChainMini({ label, chain }: { label: "L1" | "L2"; chain?: ChainData }) {
  const isL1 = label === "L1";
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => {
    const id = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000);
    return () => clearInterval(id);
  }, []);
  const age = chain?.timestamp ? formatAge(chain.timestamp, now) : null;
  const gas = useMemo(
    () => chain?.gasUsed != null && chain?.gasLimit ? formatGas(chain.gasUsed, chain.gasLimit) : null,
    [chain?.gasUsed, chain?.gasLimit],
  );

  const explorerUrl = isL1 ? config.l1Explorer : config.l2Explorer;
  const blockUrl = chain?.blockNumber != null
    ? `${explorerUrl}/block/${chain.blockNumber}`
    : undefined;

  return (
    <span className={styles.chainGroup} data-chain={isL1 ? "l1" : "l2"}>
      <a
        href={explorerUrl}
        target="_blank"
        rel="noopener noreferrer"
        className={`${styles.chainPill} ${isL1 ? styles.pillL1 : styles.pillL2}`}
      >
        {label}
      </a>
      {blockUrl ? (
        <a href={blockUrl} target="_blank" rel="noopener noreferrer" className={styles.blockLink}>
          {chain!.blockNumber!.toLocaleString()}
        </a>
      ) : (
        <span className={styles.blockNum}>—</span>
      )}
      {chain?.txCount != null && (
        <span className={styles.meta}>{chain.txCount}{chain.txCount === 1 ? "tx" : "txs"}</span>
      )}
      {gas && <span className={styles.meta}>{gas}</span>}
      {age && <span className={styles.age}>{age}</span>}
    </span>
  );
}

export function NodeHealth({ health, l1, l2 }: Props) {
  if (!health) {
    return (
      <div className={styles.bar}>
        <span className={styles.statusGroup}>
          <span className={`${styles.dot} ${styles.warn}`} />
          <span className={styles.statusText}>Connecting...</span>
        </span>
      </div>
    );
  }

  const synced = l2?.synced;

  return (
    <div className={styles.bar}>
      {/* Left: Chain data */}
      <ChainMini label="L1" chain={l1} />

      <span className={styles.sep} />

      <ChainMini label="L2" chain={l2} />

      {/* Right: Status cluster */}
      <span className={styles.rightCluster}>
        {synced != null && (
          <span className={`${styles.syncBadge} ${synced ? styles.synced : styles.syncing}`}>
            {synced ? "SYNCED" : "SYNCING"}
          </span>
        )}

        {health.pending_submissions > 0 && (
          <span className={styles.alertBadge}>
            {health.pending_submissions} pending
          </span>
        )}

        {health.consecutive_rewind_cycles > 0 && (
          <span className={styles.rewindBadge}>
            {health.consecutive_rewind_cycles} rewinds
          </span>
        )}

        <span className={`${styles.dot} ${health.healthy ? styles.ok : styles.err}`} />

        {health.commit && (
          <span className={styles.commit}>{health.commit.slice(0, 7)}</span>
        )}
      </span>
    </div>
  );
}
