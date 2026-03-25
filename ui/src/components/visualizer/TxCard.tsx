/**
 * TxCard — per-transaction card in Block Explorer.
 * Shows batch entries, trigger, consumed actions, and execution flow.
 * Matches the output of DecodeExecutions.s.sol forge script.
 */

import { useState } from "react";
import type { DecodedTx } from "../../lib/blockLogDecoder";
import type { ExecutionEntry, Action } from "../../types/chain";
import { actionTypeName, formatEther, formatScope } from "../../lib/actionFormatter";
import { ExplorerLink } from "../ExplorerLink";
import { ExecutionFlow } from "./ExecutionFlow";
import styles from "./BlockExplorer.module.css";

const KNOWN_SELECTORS: Record<string, string> = {
  // User contracts
  "0xd09de08a": "increment()",
  "0x06661abd": "counter()",
  "0x5a6a9e05": "targetCounter()",
  "0x2bf21647": "incrementProxy()",
  // L2Context
  "0xe9d68d7b": "setContext()",
  // CrossChainManagerL2
  "0x96609ad5": "loadExecutionTable()",
  "0x0f64c845": "executeIncomingCrossChainCall()",
  "0x9af53259": "executeCrossChainCall()",
  "0x92f6fe4a": "newScope()",
  // CrossChainProxy
  "0x532f0839": "executeOnBehalf()",
};

function selectorName(data: string): string | null {
  if (!data || data.length < 10) return null;
  return KNOWN_SELECTORS[data.slice(0, 10).toLowerCase()] ?? null;
}

const ZERO_HASH = "0x" + "0".repeat(64);
const ZERO_ADDR = "0x" + "0".repeat(40);

function isImmediate(entry: ExecutionEntry): boolean {
  return entry.actionHash.toLowerCase() === ZERO_HASH;
}

function CopyableHex({ value, className }: { value: string; className?: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <span
      className={className}
      onClick={(e) => {
        e.stopPropagation();
        navigator.clipboard.writeText(value);
        setCopied(true);
        setTimeout(() => setCopied(false), 1200);
      }}
      title={copied ? "Copied!" : "Click to copy"}
      style={{ cursor: "pointer" }}
    >
      {value}
      {copied && <span className={styles.copiedIndicator}>{" \u2713"}</span>}
    </span>
  );
}

// ─── Render nextAction as JSX with explorer links ───

function NextActionDetail({ action }: { action: Action }) {
  const type = actionTypeName(action.actionType);
  const fn = selectorName(action.data) ?? (action.data.length >= 10 ? action.data.slice(0, 10) : "()");

  if (action.actionType === 0) {
    // CALL
    return (
      <span>
        {type}(rollup {action.rollupId.toString()},{" "}
        <ExplorerLink value={action.destination} chain="l2" short={false} className={styles.inlineLink} />
        .{fn}
        {action.sourceAddress !== ZERO_ADDR && (
          <>, from <ExplorerLink value={action.sourceAddress} chain="l1" short={false} className={styles.inlineLink} /></>
        )}
        )
      </span>
    );
  }
  if (action.actionType === 1) {
    // RESULT
    return (
      <span>
        {type}(rollup {action.rollupId.toString()}, {action.failed ? "FAILED" : "ok"}, data=
        <CopyableHex value={action.data} className={styles.fieldMono} />)
      </span>
    );
  }
  if (action.actionType === 2) {
    // L2TX
    return (
      <span>
        {type}(rollup {action.rollupId.toString()}, data=
        <CopyableHex value={action.data} className={styles.fieldMono} />)
      </span>
    );
  }
  return <span>{type}(rollup {action.rollupId.toString()})</span>;
}

// ─── Entry Row — matches _logBatchEntry ───

function EntryRow({ entry, index }: { entry: ExecutionEntry; index: number }) {
  const [expanded, setExpanded] = useState(false);
  const imm = isImmediate(entry);

  return (
    <div className={`${styles.entryRow} ${imm ? styles.entryImm : styles.entryDef}`}>
      <div className={styles.entrySummary} onClick={() => setExpanded(!expanded)}>
        <span className={styles.entryIdx}>[{index}]</span>
        <span className={`${styles.entryBadge} ${imm ? styles.immBadge : styles.defBadge}`}>
          {imm ? "IMMEDIATE" : "DEFERRED"}
        </span>
        <span className={styles.entryHash}>
          actionHash: <CopyableHex value={imm ? ZERO_HASH : entry.actionHash} className={styles.fieldMono} />
        </span>
        <span className={styles.entryChevron}>{expanded ? "\u25B4" : "\u25BE"}</span>
      </div>
      {expanded && (
        <div className={styles.entryDetail}>
          {/* State deltas — matching forge: "stateDelta: rollup N  0x1234 -> 0xabcd  ether: N" */}
          {entry.stateDeltas.map((sd, si) => (
            <div key={si} className={styles.deltaRow}>
              <span className={styles.deltaLabel}>stateDelta:</span>
              <span className={styles.deltaContent}>
                rollup {sd.rollupId.toString()}
                {"  "}
                <CopyableHex value={sd.currentState} className={styles.stateRoot} />
                {" \u2192 "}
                <CopyableHex value={sd.newState} className={styles.stateRootNew} />
                {"  ether: "}{formatEther(sd.etherDelta)}
              </span>
            </div>
          ))}

          {/* Next action (deferred only) — matching forge: "nextAction: CALL(rollup 1, ...)" */}
          {!imm && (
            <div className={styles.fullField}>
              <span className={styles.fieldLabel}>nextAction:</span>
              <span className={styles.fieldValue}>
                <NextActionDetail action={entry.nextAction} />
              </span>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// ─── Consumed Row — matches _logAction ───

function ConsumedRow({ actionHash, action, index }: { actionHash: string; action: Action; index: number }) {
  const [expanded, setExpanded] = useState(false);

  return (
    <div className={styles.consumedRow}>
      <div className={styles.consumedSummary} onClick={() => setExpanded(!expanded)}>
        <span className={styles.consumedIdx}>[{index}]</span>
        <span className={styles.consumedType}>
          {actionTypeName(action.actionType)}
        </span>
        <span className={styles.consumedHash}>
          actionHash: <CopyableHex value={actionHash} className={styles.fieldMono} />
        </span>
        {selectorName(action.data) && (
          <span className={styles.consumedFn}>{"\u2192"} {selectorName(action.data)}</span>
        )}
        <span className={styles.entryChevron}>{expanded ? "\u25B4" : "\u25BE"}</span>
      </div>
      {expanded && (
        <div className={styles.consumedDetail}>
          {/* Line 1: TYPE | rollup=N dest=ADDR val=N */}
          <div className={styles.consumedLine}>
            {actionTypeName(action.actionType)}
            {" | rollup="}{action.rollupId.toString()}
            {" dest="}
            {action.destination === ZERO_ADDR
              ? "0x0"
              : <ExplorerLink value={action.destination} chain="l2" short={false} className={styles.inlineLink} />
            }
            {" val="}{action.value.toString()}
          </div>
          {/* Line 2: data=FULL_DATA */}
          <div className={styles.consumedLine}>
            data=<CopyableHex value={action.data} className={styles.fieldMono} />
            {selectorName(action.data) && (
              <span className={styles.selectorHint}>{" \u2192 "}{selectorName(action.data)}</span>
            )}
          </div>
          {/* Line 3: failed=bool src=ADDR srcRollup=N scope=[] */}
          <div className={styles.consumedLine}>
            failed={action.failed ? "true" : "false"}
            {" src="}
            {action.sourceAddress === ZERO_ADDR
              ? "0x0"
              : <ExplorerLink value={action.sourceAddress} chain="l1" short={false} className={styles.inlineLink} />
            }
            {" srcRollup="}{action.sourceRollup.toString()}
            {" scope="}{formatScope(action.scope)}
          </div>
          {/* actionHash full */}
          <div className={styles.consumedLine}>
            actionHash: <CopyableHex value={actionHash} className={styles.fieldMono} />
          </div>
        </div>
      )}
    </div>
  );
}

// ─── Section Header ───

function SectionHeader({ title, count, defaultOpen = false, children }: {
  title: string; count: number; defaultOpen?: boolean; children: React.ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return (
    <div className={styles.section}>
      <button className={styles.sectionHeader} onClick={() => setOpen(!open)}>
        <span className={styles.sectionTitle}>{title}</span>
        <span className={styles.sectionCount}>{count}</span>
        <span className={styles.sectionChevron}>{open ? "\u25B4" : "\u25BE"}</span>
      </button>
      {open && <div className={styles.sectionBody}>{children}</div>}
    </div>
  );
}

// ─── Main TxCard ───

interface Props {
  tx: DecodedTx;
  index: number;
}

export function TxCard({ tx, index }: Props) {
  const hasConsumed = tx.consumed.length > 0;
  const hasFlow = tx.flowSteps.length > 0;

  return (
    <div className={styles.txCard}>
      {/* Header — tx hash as explorer link + L2 block badges */}
      <div className={styles.txHeader}>
        <span className={styles.txIdx}>TX {index + 1}</span>
        <ExplorerLink value={tx.txHash} type="tx" chain="l1" short={false} className={styles.txHashFull} />
        {tx.l2BlockNumbers.length > 0 && (
          <span className={styles.l2BlockBadges}>
            {tx.l2BlockNumbers.map((num) => (
              <button
                key={num}
                className={styles.l2BlockBadge}
                onClick={(e) => {
                  e.stopPropagation();
                  document.getElementById(`l2-block-${num}`)?.scrollIntoView({ behavior: "smooth", block: "center" });
                }}
                title={`Scroll to L2 block ${num}`}
              >
                L2#{num}
              </button>
            ))}
          </span>
        )}
      </div>

      {/* Summary */}
      <div className={styles.summaryLine}>{tx.summary}</div>

      {/* Batch Entries — auto-open when it's the only section */}
      {tx.batchEntries.length > 0 && (
        <SectionHeader title="Batch Entries" count={tx.batchEntries.length} defaultOpen={!hasConsumed && !tx.trigger}>
          {tx.batchEntries.map((entry, i) => (
            <EntryRow key={i} entry={entry} index={i} />
          ))}
        </SectionHeader>
      )}

      {/* Trigger: CrossChainCall — full addresses with explorer links */}
      {tx.trigger && tx.trigger.type === "cross-chain" && (
        <div className={styles.triggerCard}>
          <div className={styles.triggerBadge}>TRIGGER: CrossChainCall</div>
          <div className={styles.triggerDetail}>
            <div className={styles.consumedLine}>
              actionHash: <CopyableHex value={tx.trigger.actionHash} className={styles.fieldMono} />
            </div>
            <div className={styles.consumedLine}>
              proxy: <ExplorerLink value={tx.trigger.proxy} chain="l1" short={false} className={styles.inlineLink} />
            </div>
            <div className={styles.consumedLine}>
              sourceAddress: <ExplorerLink value={tx.trigger.sourceAddress} chain="l1" short={false} className={styles.inlineLink} />
            </div>
            <div className={styles.consumedLine}>
              callData: <CopyableHex value={tx.trigger.callData} className={styles.fieldMono} />
              {selectorName(tx.trigger.callData) && (
                <span className={styles.selectorHint}>{" \u2192 "}{selectorName(tx.trigger.callData)}</span>
              )}
            </div>
          </div>
        </div>
      )}

      {/* Trigger: L2TX — full data */}
      {tx.trigger && tx.trigger.type === "l2tx" && (
        <div className={styles.triggerCard}>
          <div className={styles.triggerBadgeL2}>TRIGGER: L2TX</div>
          <div className={styles.triggerDetail}>
            <div className={styles.consumedLine}>
              actionHash: <CopyableHex value={tx.trigger.actionHash} className={styles.fieldMono} />
            </div>
            <div className={styles.consumedLine}>
              rollupId: {tx.trigger.rollupId.toString()}
            </div>
            <div className={styles.consumedLine}>
              rlpData: <CopyableHex value={tx.trigger.rlpData} className={styles.fieldMono} />
            </div>
          </div>
        </div>
      )}

      {/* Consumed */}
      {hasConsumed && (
        <SectionHeader title="Executions Consumed" count={tx.consumed.length} defaultOpen>
          {tx.consumed.map((c, i) => (
            <ConsumedRow key={i} actionHash={c.actionHash} action={c.action} index={i} />
          ))}
        </SectionHeader>
      )}

      {/* Execution Flow */}
      {hasFlow && (
        <div className={styles.section}>
          <div className={styles.sectionHeader} style={{ cursor: "default" }}>
            <span className={styles.sectionTitle}>Execution Flow</span>
          </div>
          <div className={styles.sectionBody}>
            <ExecutionFlow flowSteps={tx.flowSteps} />
          </div>
        </div>
      )}
    </div>
  );
}
