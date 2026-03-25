import React from "react";
import { useMonitorStore } from "../../store";
import { useTxIntrospection } from "../../hooks/useTxIntrospection";
import { truncateAddress, truncateHex } from "../../lib/actionFormatter";
import type { Chain } from "../../types/visualization";
import styles from "./TxDetails.module.css";

type Props = {
  txHash: `0x${string}`;
  chain: Chain;
};

export const TxDetails: React.FC<Props> = ({ txHash, chain }) => {
  const rpcUrl = useMonitorStore((s) =>
    chain === "l1" ? s.l1RpcUrl : s.l2RpcUrl,
  );
  const { data, loading } = useTxIntrospection(txHash, rpcUrl, chain);

  if (loading) {
    return <div className={styles.loading}>Loading tx receipt...</div>;
  }

  if (!data) return null;

  return (
    <div className={styles.root}>
      <div className={styles.meta}>
        <span>From: {truncateAddress(data.from)}</span>
        {data.to && <span> · To: {truncateAddress(data.to)}</span>}
        <span> · Gas: {data.gasUsed.toString()}</span>
      </div>

      <div className={styles.logsTitle}>
        Decoded Logs ({data.logs.length})
      </div>

      {data.logs.map((log, i) => (
        <div key={i} className={styles.logEntry}>
          <div className={styles.logHeader}>
            <span style={{ color: "var(--accent)", fontWeight: 700 }}>
              {log.eventName}
            </span>
            <span style={{ color: "var(--text-dim)" }}>
              @{truncateAddress(log.address)}
            </span>
          </div>
          <div className={styles.logArgs}>
            {Object.entries(log.args).map(([key, val]) => (
              <div key={key} className={styles.logArg}>
                <span style={{ color: "var(--text-dim)" }}>{key}:</span>{" "}
                <span style={{ color: "var(--text)" }}>
                  {formatArg(val)}
                </span>
              </div>
            ))}
          </div>
        </div>
      ))}
    </div>
  );
};

function formatArg(val: unknown): string {
  if (typeof val === "bigint") return val.toString();
  if (typeof val === "string") {
    if (val.startsWith("0x") && val.length > 20) return truncateHex(val);
    return val;
  }
  if (typeof val === "boolean") return val ? "true" : "false";
  if (Array.isArray(val)) return `[${val.map(formatArg).join(", ")}]`;
  if (val && typeof val === "object") {
    return JSON.stringify(val, (_, v) =>
      typeof v === "bigint" ? v.toString() : v,
    );
  }
  return String(val);
}
