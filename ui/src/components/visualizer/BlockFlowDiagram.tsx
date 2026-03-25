/**
 * BlockFlowDiagram — premium data-driven flow visualization.
 *
 * L1 lane: HTML — Builder → postBatch() → Rollups.sol → State Root
 *          (+ cross-chain: User TX → CrossChainProxy → executeCrossChainCall)
 * L2 lane: HTML — protocol tx blocks, with cascading internal call trace
 *          blocks fetched via debug_traceTransaction.  Each call is a
 *          styled card stepping right with depth.
 */

import React, { useState, useEffect } from "react";
import type { DecodedBlock } from "../../lib/blockLogDecoder";
import type { L2TxInfo } from "../../types/chain";
import { config } from "../../config";
import { rpcCall } from "../../rpc";
import styles from "./BlockFlowDiagram.module.css";

interface Props {
  block: DecodedBlock;
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

// ─── Known selectors ───

const KNOWN_SELECTORS: Record<string, string> = {
  "0xe9d68d7b": "setContext()",
  "0x96609ad5": "loadTable()",
  "0x0f64c845": "execIncoming()",
  "0x9af53259": "execCrossChain()",
  "0x92f6fe4a": "newScope()",
  "0xd09de08a": "increment()",
  "0x06661abd": "counter()",
  "0x5a6a9e05": "targetCounter()",
  "0x2bf21647": "incrementProxy()",
  "0x532f0839": "execOnBehalf()",
};

const TRACEABLE = new Set(["0x0f64c845", "0x96609ad5"]);

function fnName(data: string): string {
  if (!data || data.length < 10) return "?";
  return KNOWN_SELECTORS[data.slice(0, 10).toLowerCase()] ?? data.slice(0, 10);
}

function shortAddr(hex: string): string {
  if (!hex || hex.length < 10) return hex || "";
  return hex.slice(0, 6) + "\u2026" + hex.slice(-4);
}

function short(hex: string, bytes = 4): string {
  if (!hex || hex.length < bytes * 2 + 4) return hex || "";
  return hex.slice(0, bytes * 2 + 2) + "\u2026";
}

// ─── Recursive call trace block renderer ───

function TraceBlock({ trace, depth = 0 }: { trace: CallTrace; depth?: number }) {
  const fn = fnName(trace.input);
  const children = trace.calls ?? [];
  const isRoot = depth === 0;

  return (
    <div className={styles.traceLevel}>
      {depth > 0 && <div className={styles.traceConnector} />}

      <div className={`${styles.traceBlock} ${isRoot ? styles.traceBlockRoot : ""}`}>
        <div className={styles.traceBlockHeader}>
          <span className={`${styles.traceCallType} ${isRoot ? styles.traceCallTypeRoot : ""}`}>
            {trace.type}
          </span>
          <span className={styles.traceBlockFn}>{fn}</span>
        </div>
        <div className={styles.traceBlockAddrs}>
          <span className={styles.traceAddr}>{shortAddr(trace.from)}</span>
          <span className={styles.traceAddrArrow}>{"\u2192"}</span>
          <span className={styles.traceAddr}>{shortAddr(trace.to)}</span>
        </div>
      </div>

      {children.length > 0 && (
        <div className={styles.traceChildren}>
          {children.map((child, i) => (
            <TraceBlock key={i} trace={child} depth={depth + 1} />
          ))}
        </div>
      )}
    </div>
  );
}

// ─── L1 chip renderer ───

function L1Chip({ item }: { item: { label: string; sub?: string; active?: boolean } }) {
  return (
    <div className={`${styles.l1Chip} ${item.active ? styles.l1ChipActive : ""}`}>
      <div className={styles.l1ChipLabel}>{item.label}</div>
      {item.sub && <div className={styles.l1ChipSub}>{item.sub}</div>}
    </div>
  );
}

// ─── Component ───

export function BlockFlowDiagram({ block }: Props) {
  const [traces, setTraces] = useState<Map<string, CallTrace>>(new Map());
  const [traceLoading, setTraceLoading] = useState(false);

  const hasL2Blocks = block.l2Blocks.length > 0;
  const hasCrossChain = block.txs.some((tx) => tx.trigger?.type === "cross-chain");
  const hasEntries = block.allBatchEntries.length > 0;
  const txCount = block.txs.length;

  // Collect traceable protocol txs
  const traceableTxs: L2TxInfo[] = [];
  if (hasL2Blocks) {
    for (const l2Block of block.l2Blocks) {
      for (const tx of l2Block.transactions) {
        if (tx.isProtocol && tx.data?.length >= 10 && TRACEABLE.has(tx.data.slice(0, 10).toLowerCase())) {
          traceableTxs.push(tx);
        }
      }
    }
  }

  // Auto-fetch traces
  useEffect(() => {
    if (traceableTxs.length === 0 || !config.l2Rpc) return;
    const missing = traceableTxs.filter((tx) => !traces.has(tx.hash));
    if (missing.length === 0) return;

    let cancelled = false;
    setTraceLoading(true);

    Promise.all(
      missing.map((tx) =>
        rpcCall(config.l2Rpc, "debug_traceTransaction", [tx.hash, { tracer: "callTracer" }])
          .then((result) => ({ hash: tx.hash, trace: result as CallTrace }))
          .catch(() => null),
      ),
    ).then((results) => {
      if (cancelled) return;
      const newTraces = new Map(traces);
      for (const r of results) {
        if (r) newTraces.set(r.hash, r.trace);
      }
      setTraces(newTraces);
      setTraceLoading(false);
    });

    return () => { cancelled = true; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [block.blockNumber, hasL2Blocks]);

  // Don't render for empty blocks
  if (txCount === 0 && !hasL2Blocks) return null;

  // Cross-chain details
  const ccTxs = block.txs.filter((tx) => tx.trigger?.type === "cross-chain");
  const ccCount = ccTxs.length;
  const ccTrigger = ccCount === 1 && ccTxs[0]!.trigger?.type === "cross-chain" ? ccTxs[0]!.trigger : null;

  const entryCount = block.allBatchEntries.length;
  const showL2 = hasL2Blocks || hasEntries;

  // ── Build L1 items ──
  type L1Item = { kind: "chip"; label: string; sub?: string; active?: boolean };

  const l1Items: L1Item[] = [
    { kind: "chip", label: "Composer", sub: `${txCount} tx${txCount !== 1 ? "s" : ""}`, active: true },
    { kind: "chip", label: "postBatch()", active: txCount > 0 },
    { kind: "chip", label: "Rollups.sol", sub: entryCount > 0 ? `${entryCount} entr${entryCount !== 1 ? "ies" : "y"}` : "contract", active: txCount > 0 },
  ];
  if (hasEntries) {
    l1Items.push({ kind: "chip", label: "BatchPosted", active: true });
  }
  l1Items.push({
    kind: "chip",
    label: "On-chain State",
    sub: hasEntries ? "stateRoot updated" : "unchanged",
    active: hasEntries || hasL2Blocks,
  });

  // Cross-chain items as a second row
  const l1CcItems: L1Item[] = [];
  if (hasCrossChain) {
    l1CcItems.push({
      kind: "chip",
      label: ccCount > 1 ? `${ccCount} User TXs` : "User TX",
      sub: ccTrigger ? short(ccTrigger.sourceAddress) : ccCount > 1 ? `${ccCount} calls` : "caller",
      active: true,
    });
    l1CcItems.push({ kind: "chip", label: "send tx", active: true });
    l1CcItems.push({
      kind: "chip",
      label: "CrossChainProxy",
      sub: ccTrigger ? short(ccTrigger.proxy) : ccCount > 1 ? `${ccCount} proxies` : "proxy",
      active: true,
    });
    l1CcItems.push({ kind: "chip", label: "executeCrossChainCall", active: true });
    l1CcItems.push({ kind: "chip", label: "Rollups.sol", active: true });
  }

  // ── Build L2 data ──
  type L2Item =
    | { kind: "chip"; label: string; type: "protocol" | "user" }
    | { kind: "trace"; trace: CallTrace }
    | { kind: "chip-loading"; label: string };

  const l2Items: L2Item[] = [];

  if (hasL2Blocks) {
    const allL2Txs = block.l2Blocks.flatMap((b) => b.transactions);
    let userCount = 0;

    for (const tx of allL2Txs) {
      if (!tx.isProtocol) {
        userCount++;
        continue;
      }
      const sel = tx.data?.slice(0, 10).toLowerCase() ?? "";
      const fn = KNOWN_SELECTORS[sel] ?? sel;
      const trace = traces.get(tx.hash);

      if (TRACEABLE.has(sel)) {
        if (trace) {
          l2Items.push({ kind: "trace", trace });
        } else {
          l2Items.push({ kind: "chip-loading", label: fn });
        }
      } else {
        l2Items.push({ kind: "chip", label: fn, type: "protocol" });
      }
    }

    if (userCount > 0) {
      l2Items.push({ kind: "chip", label: `${userCount} user tx${userCount !== 1 ? "s" : ""}`, type: "user" });
    }
  }

  const l2BlockNums = block.l2Blocks.map((b) => `#${b.number}`).join(", ");
  const l2BlocksSub = hasL2Blocks
    ? (l2BlockNums.length > 24 ? `${block.l2Blocks.length} blocks` : l2BlockNums)
    : "";

  return (
    <div className={styles.wrap}>
      {/* ═══ L1 Section ═══ */}
      <div className={styles.l1Lane}>
        <span className={styles.l1LaneLabel}>L1</span>

        <div className={styles.l1BlockInfo}>#{block.blockNumber.toLocaleString()}</div>

        {/* L1 main flow — Rollups.sol pinned to center axis */}
        <div className={styles.l1FlowGrid}>
          <div className={styles.l1FlowLeft}>
            {l1Items.slice(0, 2).map((item, i) => (
              <React.Fragment key={i}>
                {i > 0 && <div className={styles.l1FlowConnector} />}
                <L1Chip item={item} />
              </React.Fragment>
            ))}
            <div className={styles.l1FlowConnector} />
          </div>
          <div className={styles.l1FlowCenter}>
            <L1Chip item={l1Items[2]!} />
          </div>
          <div className={styles.l1FlowRight}>
            {l1Items.slice(3).map((item, i) => (
              <React.Fragment key={i}>
                <div className={styles.l1FlowConnector} />
                <L1Chip item={item} />
              </React.Fragment>
            ))}
          </div>
        </div>

        {hasCrossChain && (
          <>
            <div className={styles.l1CcSeparator}>
              <div className={styles.l1CcSepLine} />
              <span className={styles.l1CcSepLabel}>cross-chain</span>
              <div className={styles.l1CcSepLine} />
            </div>
            {/* Cross-chain flow — CrossChainProxy pinned to center axis */}
            <div className={styles.l1FlowGrid}>
              <div className={styles.l1FlowLeft}>
                {l1CcItems.slice(0, 2).map((item, i) => (
                  <React.Fragment key={i}>
                    {i > 0 && <div className={styles.l1FlowConnector} />}
                    <L1Chip item={item} />
                  </React.Fragment>
                ))}
                <div className={styles.l1FlowConnector} />
              </div>
              <div className={styles.l1FlowCenter}>
                <L1Chip item={l1CcItems[2]!} />
              </div>
              <div className={styles.l1FlowRight}>
                {l1CcItems.slice(3).map((item, i) => (
                  <React.Fragment key={i}>
                    <div className={styles.l1FlowConnector} />
                    <L1Chip item={item} />
                  </React.Fragment>
                ))}
              </div>
            </div>
          </>
        )}
      </div>

      {/* ═══ L2 Section ═══ */}
      {showL2 && (
        <div className={styles.l2Section}>
          <div className={styles.deriveConnector}>
            <div className={styles.deriveLine} />
            <span className={styles.deriveLabel}>derives from L1 events</span>
            <div className={styles.deriveLine} />
          </div>

          <div className={styles.l2Lane}>
            <span className={styles.l2LaneLabel}>L2</span>

            {l2BlocksSub && (
              <div className={styles.l2BlockInfo}>{l2BlocksSub}</div>
            )}

            {traceLoading && (
              <div className={styles.l2Loading}>
                <span className={styles.l2Spinner} />
                loading call traces...
              </div>
            )}

            <div className={styles.l2Flow}>
              {l2Items.map((item, i) => (
                <React.Fragment key={i}>
                  {i > 0 && <div className={styles.l2FlowConnector} />}
                  {(item.kind === "chip" || item.kind === "chip-loading") ? (
                    <div className={`${styles.l2Chip} ${item.kind === "chip" && item.type === "user" ? styles.l2ChipUser : ""}`}>
                      <div className={styles.l2ChipLabel}>{item.label}</div>
                      {item.kind === "chip-loading" && (
                        <span className={styles.l2SpinnerTiny} />
                      )}
                    </div>
                  ) : (
                    item.trace.calls?.length ? (
                      <div className={styles.traceGroup}>
                        <TraceBlock trace={item.trace} />
                      </div>
                    ) : (
                      <TraceBlock trace={item.trace} />
                    )
                  )}
                </React.Fragment>
              ))}
            </div>

            {!hasL2Blocks && hasEntries && (
              <div className={styles.l2Empty}>Entries only — no L2 block data</div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
