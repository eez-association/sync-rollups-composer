/**
 * L2BlockCard — renders one L2 block in the dual-lane explorer.
 * Shows header, stats, state root, and collapsible transaction list.
 * Protocol txs (CCM) can be expanded to show internal call traces.
 */

import { useState, useCallback } from "react";
import type { L2BlockInfo, L2TxInfo } from "../../types/chain";
import { config } from "../../config";
import { rpcCall } from "../../rpc";
import { lookupAddress, registerAddress } from "../../lib/addressBook";
import { ExplorerLink } from "../ExplorerLink";
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

// Selectors that have interesting internal call traces
const TRACEABLE_SELECTORS = new Set(["0x0f64c845", "0x96609ad5"]);

function selectorName(data: string): string | null {
  if (!data || data.length < 10) return null;
  return KNOWN_SELECTORS[data.slice(0, 10).toLowerCase()] ?? null;
}

function formatGas(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(0) + "K";
  return n.toString();
}

function truncateHex(hex: string, bytes: number): string {
  if (!hex || hex.length <= bytes * 2 + 4) return hex;
  return hex.slice(0, bytes * 2 + 2) + "...";
}

function shortAddr(addr: string): string {
  return addr.slice(0, 8) + "..." + addr.slice(-4);
}

// ─── Call trace types ───

type CallTrace = {
  from: string;
  to: string;
  input: string;
  output?: string;
  value: string;
  type: string;
  gasUsed: string;
  calls?: CallTrace[];
};

function isTraceable(tx: L2TxInfo): boolean {
  if (!tx.data || tx.data.length < 10) return false;
  return TRACEABLE_SELECTORS.has(tx.data.slice(0, 10).toLowerCase());
}

/** Walk a call trace and register unknown addresses based on context. */
function discoverAddressesFromTrace(trace: CallTrace, depth = 0) {
  const sel = trace.input?.slice(0, 10).toLowerCase();
  // call() on a proxy → the proxy address
  if (sel === "0x532f0839" && !lookupAddress(trace.to)) {
    registerAddress(trace.to, "CrossChainProxy", "l2");
  }
  // Leaf call from a proxy → the target contract
  if (depth >= 2 && !lookupAddress(trace.to) && trace.calls == null) {
    const fn = selectorName(trace.input);
    if (fn) {
      registerAddress(trace.to, `Contract (${fn.replace("()", "")})`, "l2");
    }
  }
  for (const child of trace.calls ?? []) {
    discoverAddressesFromTrace(child, depth + 1);
  }
}

// ─── Call trace tree renderer ───

function CallTraceNode({ trace, depth = 0 }: { trace: CallTrace; depth?: number }) {
  const fn = selectorName(trace.input);
  const gas = parseInt(trace.gasUsed, 16);

  return (
    <div className={styles.traceNode} style={{ marginLeft: depth * 16 }}>
      <div className={styles.traceRow}>
        {depth > 0 && <span className={styles.traceConnector}>{"\u2514\u2500"} </span>}
        <span className={styles.traceType}>{trace.type}</span>
        <ExplorerLink value={trace.from} chain="l2" short className={styles.traceAddr} />
        <span className={styles.traceArrow}>{"\u2192"}</span>
        <ExplorerLink value={trace.to} chain="l2" short className={styles.traceAddr} />
        {fn && <span className={styles.traceFn}>.{fn}</span>}
        {!fn && trace.input.length >= 10 && (
          <span className={styles.traceSel}>.{trace.input.slice(0, 10)}</span>
        )}
        {gas > 0 && <span className={styles.traceGas}>{formatGas(gas)} gas</span>}
      </div>
      {trace.calls?.map((child, i) => (
        <CallTraceNode key={i} trace={child} depth={depth + 1} />
      ))}
    </div>
  );
}

// ─── Per-transaction row with expandable trace ───

function L2TxRow({ tx }: { tx: L2TxInfo }) {
  const [traceOpen, setTraceOpen] = useState(false);
  const [trace, setTrace] = useState<CallTrace | null>(null);
  const [traceLoading, setTraceLoading] = useState(false);
  const [traceError, setTraceError] = useState<string | null>(null);
  const canTrace = isTraceable(tx);

  const fetchTrace = useCallback(async () => {
    if (trace) {
      setTraceOpen(!traceOpen);
      return;
    }
    setTraceLoading(true);
    setTraceError(null);
    try {
      const result = await rpcCall(config.l2Rpc, "debug_traceTransaction", [
        tx.hash,
        { tracer: "callTracer" },
      ]) as CallTrace;
      discoverAddressesFromTrace(result);
      setTrace(result);
      setTraceOpen(true);
    } catch (e) {
      setTraceError((e as Error).message);
    } finally {
      setTraceLoading(false);
    }
  }, [tx.hash, trace, traceOpen]);

  const fn = selectorName(tx.data);

  return (
    <div className={styles.l2TxRowWrap}>
      <div
        className={`${styles.l2TxRow} ${tx.isProtocol ? styles.l2TxProtocol : ""} ${canTrace ? styles.l2TxExpandable : ""}`}
        onClick={canTrace ? fetchTrace : undefined}
      >
        <span className={tx.isProtocol ? styles.protocolBadge : styles.userBadge}>
          {tx.isProtocol ? "PROTOCOL" : "USER"}
        </span>
        <ExplorerLink value={tx.hash} type="tx" chain="l2" short className={styles.l2TxHash} />
        <ExplorerLink value={tx.from} chain="l2" short className={styles.l2TxAddr} />
        {tx.to ? (
          <>
            <span className={styles.l2TxArrow}>{"\u2192"}</span>
            <ExplorerLink value={tx.to} chain="l2" short className={styles.l2TxAddr} />
          </>
        ) : (
          <span className={styles.l2TxDeploy}>CREATE</span>
        )}
        {fn && <span className={styles.l2TxFn}>{fn}</span>}
        {canTrace && (
          <span className={styles.traceToggle}>
            {traceLoading ? (
              <span className={styles.spinnerTiny} />
            ) : traceOpen ? "\u25B4" : "\u25BE"}
          </span>
        )}
      </div>

      {/* Inline call trace */}
      {traceOpen && trace && (
        <div className={styles.traceContainer}>
          <div className={styles.traceTitle}>Internal Call Trace</div>
          <CallTraceNode trace={trace} />
        </div>
      )}
      {traceError && (
        <div className={styles.traceError}>Trace failed: {traceError}</div>
      )}
    </div>
  );
}

// ─── Main card ───

interface Props {
  block: L2BlockInfo;
}

export function L2BlockCard({ block }: Props) {
  const [expanded, setExpanded] = useState(false);
  const isPending = block.txCount === -1;
  const gasPercent = block.gasLimit > 0 ? (block.gasUsed / block.gasLimit) * 100 : 0;
  const protocolCount = block.transactions.filter((t) => t.isProtocol).length;
  const userCount = block.transactions.filter((t) => !t.isProtocol).length;

  return (
    <div className={styles.l2Card} id={`l2-block-${block.number}`}>
      {/* Header */}
      <div className={styles.l2CardHeader}>
        <span className={styles.l2Badge}>L2</span>
        <ExplorerLink
          value={block.number.toString()}
          type="block"
          chain="l2"
          short={false}
          label={`BLOCK ${block.number.toLocaleString()}`}
          className={styles.l2BlockNumLink}
        />
        {block.hash && (
          <span
            className={styles.l2Hash}
            onClick={() => navigator.clipboard.writeText(block.hash)}
            title="Click to copy block hash"
          >
            {shortAddr(block.hash)}
          </span>
        )}
        {block.timestamp > 0 && (
          <span className={styles.l2Timestamp}>
            {new Date(block.timestamp * 1000).toLocaleTimeString()}
          </span>
        )}
      </div>

      {isPending ? (
        <div className={styles.l2Pending}>Pending derivation</div>
      ) : (
        <>
          {/* Stats */}
          <div className={styles.l2Stats}>
            <div className={styles.l2StatRow}>
              <span className={styles.l2StatLabel}>Gas</span>
              <div className={styles.gasBar}>
                <div className={styles.gasBarFill} style={{ width: `${Math.min(gasPercent, 100)}%` }} />
              </div>
              <span className={styles.l2StatValue}>
                {formatGas(block.gasUsed)} / {formatGas(block.gasLimit)} ({gasPercent.toFixed(1)}%)
              </span>
            </div>
            <div className={styles.l2StatRow}>
              <span className={styles.l2StatLabel}>Txs</span>
              <span className={styles.l2StatValue}>
                {block.txCount} ({protocolCount} protocol, {userCount} user)
              </span>
            </div>
            <div className={styles.l2StatRow}>
              <span className={styles.l2StatLabel}>State</span>
              <span
                className={styles.l2StateRoot}
                onClick={() => navigator.clipboard.writeText(block.stateRoot)}
                title="Click to copy"
              >
                {truncateHex(block.stateRoot, 8)}
              </span>
            </div>
          </div>

          {/* Transactions (collapsible) */}
          {block.transactions.length > 0 && (
            <div className={styles.l2TxSection}>
              <button className={styles.l2TxToggle} onClick={() => setExpanded(!expanded)}>
                <span>Transactions ({block.transactions.length})</span>
                <span className={styles.entryChevron}>{expanded ? "\u25B4" : "\u25BE"}</span>
              </button>
              {expanded && (
                <div className={styles.l2TxList}>
                  {block.transactions.map((tx) => (
                    <L2TxRow key={tx.hash} tx={tx} />
                  ))}
                </div>
              )}
            </div>
          )}
        </>
      )}
    </div>
  );
}
