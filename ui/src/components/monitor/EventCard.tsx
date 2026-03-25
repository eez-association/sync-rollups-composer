import React, { useState, useMemo } from "react";
import type { EventRecord } from "../../types/events";
import { truncateHex, truncateAddress } from "../../lib/actionFormatter";
import { TxDetails } from "./TxDetails";
import { actionFromEventArgs, decodeActionHash, actionSummary } from "../../lib/actionHashDecoder";
import styles from "./EventCard.module.css";

type Props = {
  event: EventRecord;
  selected: boolean;
  onClick: () => void;
  correlatedChain?: "l1" | "l2";
  stepNumber: number;
  isPlayed: boolean;
};

const EVENT_COLORS: Record<string, string> = {
  BatchPosted: "#3b82f6",
  ExecutionTableLoaded: "#a855f7",
  ExecutionConsumed: "var(--red)",
  CrossChainCallExecuted: "var(--cyan)",
  CrossChainProxyCreated: "var(--green)",
  RollupCreated: "var(--accent)",
  StateUpdated: "var(--yellow)",
  L2ExecutionPerformed: "#a855f7",
  IncomingCrossChainCallExecuted: "#a855f7",
  L2TXExecuted: "var(--yellow)",
};

function eventColor(eventName: string): string {
  return EVENT_COLORS[eventName] ?? "var(--text-dim)";
}

function eventDetail(event: EventRecord): string {
  switch (event.eventName) {
    case "BatchPosted": {
      const entries = event.args.entries as unknown[] | undefined;
      return entries ? `Posts ${entries.length} execution ${entries.length === 1 ? "entry" : "entries"} to L1 table` : "";
    }
    case "ExecutionTableLoaded": {
      const entries = event.args.entries as unknown[] | undefined;
      return entries ? `Loads ${entries.length} ${entries.length === 1 ? "entry" : "entries"} into L2 table` : "";
    }
    case "ExecutionConsumed":
      return `Entry consumed: ${truncateHex(event.args.actionHash as string)}`;
    case "CrossChainCallExecuted":
      return `Proxy ${truncateAddress(event.args.proxy as string)} called by ${truncateAddress(event.args.sourceAddress as string)}`;
    case "CrossChainProxyCreated":
      return `Proxy ${truncateAddress(event.args.proxy as string)} for ${truncateAddress(event.args.originalAddress as string)}`;
    case "IncomingCrossChainCallExecuted":
      return `Incoming call to ${truncateAddress(event.args.destination as string)} from ${truncateAddress(event.args.sourceAddress as string)}`;
    case "RollupCreated":
      return `Rollup ${String(event.args.rollupId)} created`;
    case "L2ExecutionPerformed":
      return `State updated for rollup ${String(event.args.rollupId)}`;
    default:
      return "";
  }
}

function tableChangeSummary(event: EventRecord): { adds: string[]; consumes: string[] } {
  const adds: string[] = [];
  const consumes: string[] = [];
  if (event.eventName === "BatchPosted") {
    const entries = event.args.entries as Array<{ actionHash: string }> | undefined;
    if (entries) {
      for (const e of entries) {
        if (e.actionHash !== "0x0000000000000000000000000000000000000000000000000000000000000000") {
          adds.push(`+${event.chain.toUpperCase()}`);
        }
      }
    }
  }
  if (event.eventName === "ExecutionTableLoaded") {
    const entries = event.args.entries as unknown[] | undefined;
    if (entries) {
      for (let i = 0; i < entries.length; i++) {
        adds.push(`+${event.chain.toUpperCase()}`);
      }
    }
  }
  if (event.eventName === "ExecutionConsumed") {
    consumes.push(`-${event.chain.toUpperCase()}`);
  }
  return { adds, consumes };
}

export const EventCard: React.FC<Props> = ({
  event,
  selected,
  onClick,
  correlatedChain,
  stepNumber,
  isPlayed,
}) => {
  const [expanded, setExpanded] = useState(false);
  const chainColor = event.chain === "l1" ? "#3b82f6" : "#a855f7";
  const chainBg = event.chain === "l1" ? "rgba(59,130,246,0.06)" : "rgba(168,85,247,0.06)";
  const chainBorder = event.chain === "l1" ? "rgba(59,130,246,0.2)" : "rgba(168,85,247,0.2)";
  const detail = eventDetail(event);
  const { adds, consumes } = tableChangeSummary(event);

  const decoded = useMemo(() => {
    if (event.eventName !== "ExecutionConsumed") return null;
    try {
      const actionArg = event.args.action as Record<string, unknown>;
      if (!actionArg) return null;
      const fields = actionFromEventArgs(actionArg);
      const storedHash = event.args.actionHash as string;
      return decodeActionHash(storedHash, fields);
    } catch {
      return null;
    }
  }, [event]);

  const opacity = selected ? 1 : isPlayed ? 0.65 : 0.25;

  return (
    <div
      onClick={onClick}
      className={`${styles.card} ${selected ? styles.cardSelected : ""}`}
      style={{ opacity }}
    >
      <div className={`${styles.stepNum} ${selected ? styles.stepNumActive : styles.stepNumInactive}`}>
        {stepNumber}
      </div>

      <div
        className={styles.chainBadge}
        style={{ background: chainBg, color: chainColor, border: `1px solid ${chainBorder}` }}
      >
        {event.chain.toUpperCase()}
      </div>

      <div className={styles.body}>
        <div className={styles.eventName} style={{ color: eventColor(event.eventName) }}>
          {event.eventName}
        </div>
        {detail && <div className={styles.detail}>{detail}</div>}

        {decoded && (
          <div
            className={styles.decoded}
            style={{ border: `1px solid ${decoded.verified ? "rgba(52,211,153,0.2)" : "rgba(239,68,68,0.3)"}` }}
          >
            <div className={styles.decodedHeader}>
              <span style={{ color: decoded.verified ? "var(--green)" : "var(--red)", fontWeight: 700 }}>
                {decoded.verified ? "hash verified" : "HASH MISMATCH"}
              </span>
              <span style={{ color: "var(--text-dim)" }}>|</span>
              <span style={{ color: "var(--cyan)" }}>
                {actionSummary(decoded.fields)}
              </span>
            </div>
            <div className={styles.decodedGrid}>
              {Object.entries(decoded.display).map(([k, v]) => (
                <React.Fragment key={k}>
                  <span style={{ color: "var(--text-dim)" }}>{k}</span>
                  <span style={{ color: "var(--text)", wordBreak: "break-all" }}>{v}</span>
                </React.Fragment>
              ))}
            </div>
          </div>
        )}

        {(adds.length > 0 || consumes.length > 0) && (
          <div className={styles.tableChanges}>
            {adds.map((a, i) => (
              <span key={`a${i}`} style={{ color: "var(--cyan)", marginRight: 4 }}>{a}</span>
            ))}
            {consumes.map((c, i) => (
              <span key={`c${i}`} style={{ color: "var(--red)", marginRight: 4 }}>{c} consumed</span>
            ))}
          </div>
        )}

        {correlatedChain && event.eventName === "ExecutionConsumed" && (
          <div className={styles.correlation}>
            {"<->"} Matched on {correlatedChain.toUpperCase()} (same actionHash)
          </div>
        )}

        {event.eventName === "ExecutionConsumed" && (
          <div>
            <button
              onClick={(e) => { e.stopPropagation(); setExpanded(!expanded); }}
              className={styles.expandBtn}
            >
              {expanded ? "\u25BC Hide tx details" : "\u25B6 Show tx details"}
            </button>
            {expanded && (
              <TxDetails txHash={event.transactionHash} chain={event.chain} />
            )}
          </div>
        )}
      </div>
    </div>
  );
};
