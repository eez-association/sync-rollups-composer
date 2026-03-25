import React, { useState } from "react";
import type { TableEntry } from "../../types/visualization";
import { truncateHex } from "../../lib/actionFormatter";
import styles from "./TableEntryRow.module.css";

type Props = {
  entry: TableEntry;
  index: number;
};

export const TableEntryRow: React.FC<Props> = ({ entry, index }) => {
  const [expanded, setExpanded] = useState(true);
  const isAdded = entry.status === "ja";
  const isConsuming = entry.status === "jc";
  const isConsumed = entry.status === "consumed";

  const rowClass = `${styles.row} ${isAdded ? styles.rowAdded : ""} ${isConsuming ? styles.rowConsuming : ""} ${isConsumed ? styles.rowConsumed : ""}`;

  return (
    <div className={rowClass}>
      <div
        onClick={() => setExpanded(!expanded)}
        className={`${styles.summary} ${isConsuming || isConsumed ? styles.summaryStrikethrough : ""}`}
      >
        <div className={styles.indexBadge}>{index + 1}</div>
        <div className={styles.hashText}>
          <span style={{ color: "var(--text-dim)" }}>{entry.actionHash}</span>
          <span style={{ color: "var(--yellow)", margin: "0 3px" }}>{" -> "}</span>
          <span style={{ color: "var(--green)" }}>{entry.nextActionHash}</span>
          {entry.delta && (
            <span className={styles.delta}>{entry.delta}</span>
          )}
        </div>
        <div className={styles.expandIcon}>
          {expanded ? "\u25B2" : "\u25BC"}
        </div>
      </div>

      {expanded && (
        <div className={styles.details}>
          {entry.actionDetail && (
            <HashSection
              title="Action (hashed as actionHash)"
              detail={entry.actionDetail}
              fullHash={entry.fullActionHash}
            />
          )}

          {entry.nextActionDetail && (
            <HashSection
              title="Next Action (returned on match)"
              detail={entry.nextActionDetail}
              fullHash={entry.fullNextActionHash}
            />
          )}

          {entry.stateDeltas.length > 0 && (
            <div className={styles.hashSection}>
              <div className={styles.sectionHeader}>State Delta</div>
              <div className={styles.kvGrid}>
                {entry.stateDeltas.map((sd, i) => (
                  <React.Fragment key={i}>
                    <span style={{ color: "var(--text-dim)" }}>delta {i}</span>
                    <span style={{ color: "var(--text)", wordBreak: "break-all" }}>{sd}</span>
                  </React.Fragment>
                ))}
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  );
};

const HashSection: React.FC<{
  title: string;
  detail: Record<string, string>;
  fullHash?: string;
}> = ({ title, detail, fullHash }) => {
  const [showFull, setShowFull] = useState(false);

  const { computedHash, actionHash, ...fields } = detail;
  const displayHash = computedHash || actionHash;
  const hasDecodedFields = Object.keys(fields).length > 0;

  return (
    <div className={styles.hashSection}>
      <div className={styles.sectionHeader}>{title}</div>
      <div className={styles.kvGrid}>
        {displayHash && (
          <>
            <span style={{ color: "var(--text-dim)" }}>
              {computedHash ? "computedHash" : "actionHash"}
            </span>
            <span style={{ wordBreak: "break-all" }}>
              <span
                style={{ color: "var(--cyan)", cursor: "pointer" }}
                onClick={(e) => { e.stopPropagation(); setShowFull(!showFull); }}
                title={fullHash || displayHash}
              >
                {showFull ? (fullHash || displayHash) : truncateHex(displayHash, 12)}
              </span>
              {fullHash && <CopyButton text={fullHash} />}
            </span>
          </>
        )}

        {hasDecodedFields && Object.entries(fields).map(([k, v]) => (
          <React.Fragment key={k}>
            <span style={{ color: "var(--text-dim)" }}>{k}</span>
            <span
              style={{
                color: isHighlightField(k, v) ? "var(--cyan)" : "var(--text)",
                wordBreak: "break-all",
              }}
            >
              {v}
            </span>
          </React.Fragment>
        ))}

        {!hasDecodedFields && actionHash && (
          <>
            <span style={{ color: "var(--text-dim)" }}>decoded</span>
            <span style={{ color: "var(--text-dim)", fontStyle: "italic", opacity: 0.5 }}>
              available when consumed (ExecutionConsumed event)
            </span>
          </>
        )}
      </div>
    </div>
  );
};

const CopyButton: React.FC<{ text: string }> = ({ text }) => {
  const [copied, setCopied] = useState(false);
  return (
    <button
      onClick={(e) => {
        e.stopPropagation();
        navigator.clipboard.writeText(text);
        setCopied(true);
        setTimeout(() => setCopied(false), 1500);
      }}
      style={{
        marginLeft: 4,
        fontSize: "0.45rem",
        color: copied ? "var(--green)" : "var(--text-dim)",
        background: "none",
        border: "none",
        cursor: "pointer",
        fontFamily: "monospace",
        padding: "0 2px",
      }}
      title="Copy full hash"
    >
      {copied ? "copied" : "copy"}
    </button>
  );
};

function isHighlightField(key: string, value: string): boolean {
  if (key === "actionType") return true;
  if (key === "data" && value !== "0x" && value !== "0x00") return true;
  return false;
}
