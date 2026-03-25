/**
 * DebugTxMode — extracted from VisualizerView.tsx
 * Single-tx cross-chain execution debugger with step walkthrough.
 */

import { useState, useCallback, useEffect, useRef } from "react";
import { config } from "../../config";
import { rpcCall } from "../../rpc";
import { buildVisualizerEntries } from "../../lib/crossChainEntries";
import type { ExecutionVisualizerState } from "../../hooks/useExecutionVisualizer";
import { STEP_LABELS } from "../../hooks/useExecutionVisualizer";
import type { SimulateCallResult, VisualizerEntry } from "../../types";
import { ExplorerLink } from "../ExplorerLink";
import { TxLink } from "../TxLink";
import styles from "../VisualizerView.module.css";

// ─── Types ───

interface DebugTxResult {
  txHash: string;
  from: string;
  proxyAddress: string;
  targetAddress: string;
  calldata: string;
  simulation: SimulateCallResult;
  entries: VisualizerEntry[];
  status: "confirmed" | "failed" | "pending";
  gasUsed: string | null;
  blockNumber: number | null;
}

interface StepDef {
  chain: "l1" | "l2";
  title: string;
  detail: string;
  l1Entries: number[];
  l2Entries: number[];
  justAdded: number[];
  justConsumed: number[];
}

interface Props {
  liveState: ExecutionVisualizerState;
  liveTargetAddress: string;
  liveCalldata: string;
  onBack: () => void;
  initialDebugHash?: string | null;
}

// ─── Data fetching ───

const AUTHORIZED_PROXIES_SELECTOR = "0x360d95b6";

async function detectProxy(address: string): Promise<{ destination: string; rollupId: string } | null> {
  if (!config.rollupsAddress) return null;
  try {
    const calldata = AUTHORIZED_PROXIES_SELECTOR + address.toLowerCase().replace("0x", "").padStart(64, "0");
    const result = await rpcCall(config.l1Rpc, "eth_call", [
      { to: config.rollupsAddress, data: calldata }, "latest",
    ]) as string;
    if (!result || result === "0x" || result.length < 130) return null;
    const hex = result.replace("0x", "");
    const destination = "0x" + hex.slice(24, 64);
    if (destination === "0x" + "0".repeat(40)) return null;
    const rollupId = parseInt(hex.slice(64, 128), 16).toString();
    return { destination, rollupId };
  } catch {
    return null;
  }
}

/** Walk a debug_traceTransaction callTracer result to find executeCrossChainCall (0x9af53259) */
function findProxyInTrace(call: Record<string, unknown>): string | null {
  const calls = call.calls as Record<string, unknown>[] | undefined;
  if (!calls) return null;
  for (const sub of calls) {
    const input = (sub.input as string) || "";
    // executeCrossChainCall selector
    if (input.startsWith("0x9af53259")) {
      // The `from` of this call is the proxy
      return (sub.from as string) || null;
    }
    const found = findProxyInTrace(sub);
    if (found) return found;
  }
  return null;
}

async function debugTransaction(txHash: string): Promise<DebugTxResult> {
  const tx = await rpcCall(config.l1Rpc, "eth_getTransactionByHash", [txHash]) as {
    from: string; to: string; input: string; blockNumber: string | null;
  } | null;
  if (!tx) throw new Error(`Transaction ${txHash} not found on L1`);
  if (!tx.to) throw new Error("Transaction has no 'to' address (contract creation)");

  // Try direct proxy detection first
  let proxy = await detectProxy(tx.to);
  let proxyAddress = tx.to;

  // If tx.to is not a proxy (e.g. bridge contract), trace internal calls
  if (!proxy) {
    try {
      const trace = await rpcCall(config.l1Rpc, "debug_traceTransaction", [
        txHash, { tracer: "callTracer" },
      ]) as Record<string, unknown>;
      const found = findProxyInTrace(trace);
      if (found) {
        proxy = await detectProxy(found);
        if (proxy) proxyAddress = found;
      }
    } catch { /* tracing not available */ }
  }

  if (!proxy) throw new Error(`Could not find a CrossChainProxy in transaction ${txHash}. The target address ${tx.to} is not a proxy and no internal proxy call was found.`);

  const sim = await rpcCall(config.l2Rpc, "syncrollups_simulateCall", [proxy.destination, tx.input]) as SimulateCallResult | null;
  if (!sim) throw new Error("syncrollups_simulateCall returned null");

  const entries = buildVisualizerEntries(sim, proxyAddress, proxy.destination, tx.input);

  let status: "confirmed" | "failed" | "pending" = "pending";
  let gasUsed: string | null = null;
  let blockNumber: number | null = null;

  if (tx.blockNumber) {
    const receipt = await rpcCall(config.l1Rpc, "eth_getTransactionReceipt", [txHash]) as {
      status: string; gasUsed: string; blockNumber: string;
    } | null;
    if (receipt) {
      status = receipt.status === "0x1" ? "confirmed" : "failed";
      gasUsed = parseInt(receipt.gasUsed, 16).toLocaleString();
      blockNumber = parseInt(receipt.blockNumber, 16);
    }
  }

  return { txHash, from: tx.from, proxyAddress: proxyAddress, targetAddress: proxy.destination,
    calldata: tx.input, simulation: sim, entries, status, gasUsed, blockNumber };
}

// ─── Helpers ───

function short(hex: string, len = 8): string {
  if (!hex || hex.length <= len * 2 + 4) return hex || "\u2014";
  return `${hex.slice(0, len + 2)}\u2026${hex.slice(-len)}`;
}

function buildDebugSteps(r: DebugTxResult): StepDef[] {
  return [
    {
      chain: "l1", title: "Prover submits postBatch to Rollups",
      detail: `Entries posted to Rollups.sol on L1. Entry 0 carries the immediate state delta. Entry 1 is CALL\u2192${short(r.targetAddress, 4)}. Entry 2 is RESULT.`,
      l1Entries: [0, 1], l2Entries: [], justAdded: [0, 1], justConsumed: [],
    },
    {
      chain: "l2", title: "SYSTEM loads L2 execution table",
      detail: `SYSTEM_ADDRESS calls CrossChainManagerL2.loadExecutionTable(). The RESULT entry is stored on L2 for later consumption by the target contract.`,
      l1Entries: [0, 1], l2Entries: [2], justAdded: [2], justConsumed: [],
    },
    {
      chain: "l1", title: "User calls Proxy \u2192 executeCrossChainCall",
      detail: `${short(r.from, 4)} calls ${short(r.proxyAddress, 4)}. Proxy.fallback() triggers Rollups.executeCrossChainCall(). Entries 0 and 1 are matched and consumed from the L1 table.`,
      l1Entries: [0, 1], l2Entries: [2], justAdded: [], justConsumed: [0, 1],
    },
    {
      chain: "l2", title: "Cross-chain: L2 target executes",
      detail: `executeIncomingCrossChainCall routes to ${short(r.targetAddress, 4)}. The target contract executes and returns data. The RESULT entry is consumed from the L2 table.`,
      l1Entries: [0, 1], l2Entries: [2], justAdded: [], justConsumed: [2],
    },
    {
      chain: "l1", title: "Result returned to L1 caller",
      detail: `executeCrossChainCall returns the RESULT to the proxy fallback, which forwards it to the original caller. ${r.simulation.success ? "Execution succeeded." : "Execution reverted."} ${r.gasUsed ? `Gas: ${r.gasUsed}.` : ""}`,
      l1Entries: [0, 1], l2Entries: [2], justAdded: [], justConsumed: [],
    },
  ];
}

// ─── Architecture Diagram ───

function ArchitectureDiagram({ step, result }: { step: number; result: DebugTxResult }) {
  const w = 760, h = 170;
  const laneH = 60;
  const l1y = 20, l2y = l1y + laneH + 30;
  const nodeW = 100, nodeH = 36;

  const activeNodes: Record<number, string[]> = {
    0: ["rollups"],
    1: ["manager"],
    2: ["wallet", "proxy", "rollups"],
    3: ["manager", "target"],
    4: ["proxy", "wallet", "returned"],
  };

  const activeArrows: Record<number, number[]> = {
    0: [3],
    1: [4],
    2: [1, 2],
    3: [5, 6],
    4: [7],
  };

  const isActive = (id: string) => (activeNodes[step] ?? []).includes(id);
  const isArrowActive = (id: number) => (activeArrows[step] ?? []).includes(id);

  const nodes = {
    wallet:   { x: 16,   y: l1y, label: "Wallet", sub: short(result.from, 3) },
    proxy:    { x: 142,  y: l1y, label: "Proxy", sub: short(result.proxyAddress, 3) },
    rollups:  { x: 268,  y: l1y, label: "Rollups", sub: "postBatch" },
    returned: { x: 520,  y: l1y, label: "Returned", sub: result.simulation.success ? "success" : "reverted" },
    manager:  { x: 268,  y: l2y, label: "ManagerL2", sub: "loadTable" },
    target:   { x: 394,  y: l2y, label: "Target", sub: short(result.targetAddress, 3) },
    result:   { x: 520,  y: l2y, label: "Result", sub: "returnData" },
  };

  function Node({ id, x, y, label, sub }: { id: string; x: number; y: number; label: string; sub: string }) {
    const active = isActive(id);
    return (
      <g>
        <rect x={x} y={y} width={nodeW} height={nodeH} rx={6}
          className={`${styles.archNode} ${active ? styles.archNodeActive : ""}`}
        />
        <text x={x + nodeW / 2} y={y + 14} className={styles.archLabel} textAnchor="middle">{label}</text>
        <text x={x + nodeW / 2} y={y + 27} className={styles.archSub} textAnchor="middle">{sub}</text>
        {active && (
          <rect x={x} y={y} width={nodeW} height={nodeH} rx={6}
            className={styles.archPulse}
          />
        )}
      </g>
    );
  }

  function Arrow({ id, x1, y1, x2, y2, dashed }: { id: number; x1: number; y1: number; x2: number; y2: number; dashed?: boolean }) {
    const active = isArrowActive(id);
    return (
      <line x1={x1} y1={y1} x2={x2} y2={y2}
        className={`${styles.archArrow} ${active ? styles.archArrowActive : ""}`}
        strokeDasharray={dashed ? "4 3" : undefined}
        markerEnd={active ? "url(#arrowActive)" : "url(#arrow)"}
      />
    );
  }

  return (
    <div className={styles.archWrap}>
      <svg viewBox={`0 0 ${w} ${h}`} className={styles.archSvg}>
        <defs>
          <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
            <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--border-bright)" />
          </marker>
          <marker id="arrowActive" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
            <path d="M 0 0 L 10 5 L 0 10 z" fill="var(--cyan)" />
          </marker>
        </defs>

        <rect x={0} y={l1y - 8} width={w} height={laneH + 16} rx={8} className={styles.laneL1} />
        <rect x={0} y={l2y - 8} width={w} height={laneH + 16} rx={8} className={styles.laneL2} />

        <text x={w - 8} y={l1y + 10} className={styles.laneLabel} textAnchor="end">L1</text>
        <text x={w - 8} y={l2y + 10} className={styles.laneLabelL2} textAnchor="end">L2</text>

        <Arrow id={1} x1={nodes.wallet.x + nodeW} y1={l1y + nodeH / 2} x2={nodes.proxy.x} y2={l1y + nodeH / 2} />
        <Arrow id={2} x1={nodes.proxy.x + nodeW} y1={l1y + nodeH / 2} x2={nodes.rollups.x} y2={l1y + nodeH / 2} />
        <Arrow id={3} x1={nodes.rollups.x + nodeW / 2 - 20} y1={l1y - 8} x2={nodes.rollups.x + nodeW / 2 + 20} y2={l1y - 8} dashed />

        <Arrow id={4} x1={nodes.rollups.x + nodeW / 2} y1={l1y + nodeH} x2={nodes.manager.x + nodeW / 2} y2={l2y} />

        <Arrow id={5} x1={nodes.manager.x + nodeW} y1={l2y + nodeH / 2} x2={nodes.target.x} y2={l2y + nodeH / 2} />
        <Arrow id={6} x1={nodes.target.x + nodeW} y1={l2y + nodeH / 2} x2={nodes.result.x} y2={l2y + nodeH / 2} />

        <Arrow id={7} x1={nodes.result.x + nodeW / 2} y1={l2y} x2={nodes.returned.x + nodeW / 2} y2={l1y + nodeH} />

        {Object.entries(nodes).map(([id, n]) => (
          <Node key={id} id={id} {...n} />
        ))}
      </svg>
    </div>
  );
}

// ─── Progress Bar ───

function ProgressBar({ current, total }: { current: number; total: number }) {
  return (
    <div className={styles.progressWrap}>
      <div className={styles.progressTrack}>
        <div className={styles.progressFill} style={{ width: `${((current + 1) / total) * 100}%` }} />
      </div>
      <div className={styles.progressDots}>
        {Array.from({ length: total }, (_, i) => (
          <div key={i}
            className={`${styles.progressDot} ${i <= current ? styles.progressDotDone : ""} ${i === current ? styles.progressDotActive : ""}`}
            />
        ))}
      </div>
    </div>
  );
}

// ─── Expandable Entry ───

function ExpandableEntry({ entry, status, expanded, onToggle }: {
  entry: VisualizerEntry;
  status: "added" | "consumed" | "normal";
  expanded: boolean;
  onToggle: () => void;
}) {
  const typeCls = entry.actionType === "CALL" ? styles.teTypeCall
    : entry.actionType === "RESULT" ? styles.teTypeResult
    : styles.teTypeImm;

  return (
    <div className={`${styles.te} ${status === "added" ? styles.teAdded : status === "consumed" ? styles.teConsumed : ""}`}>
      <div className={styles.teSummary} onClick={onToggle}>
        <div className={styles.teIdxBadge}>{entry.index}</div>
        <div className={`${styles.teTypeBadge} ${typeCls}`}>{entry.actionType || "IMM"}</div>
        <div className={styles.teCompact}>
          {entry.destination ? short(entry.destination, 5) : "\u2014 no destination"}
          {entry.calldata && <span className={styles.teSep}>{"\u00b7"}</span>}
          {entry.calldata && <span className={styles.teDataHint}>{short(entry.calldata, 3)}</span>}
        </div>
        <div className={styles.teStatus}>
          {status === "added" && <span className={styles.teStatusAdd}>+ADD</span>}
          {status === "consumed" && <span className={styles.teStatusRm}>{"\u2715"} USED</span>}
        </div>
        <div className={styles.teChevron}>{expanded ? "\u25B4" : "\u25BE"}</div>
      </div>
      {expanded && (
        <div className={styles.teBody}>
          <div className={styles.teGrid}>
            <span className={styles.teK}>actionType</span>
            <span className={`${styles.teV} ${styles.teVHl}`}>{entry.actionType || "IMMEDIATE"}</span>
            <span className={styles.teK}>destination</span>
            <span className={styles.teV}>{entry.destination || "\u2014"}</span>
            <span className={styles.teK}>sourceAddress</span>
            <span className={styles.teV}>{entry.sourceAddress || "\u2014"}</span>
            <span className={styles.teK}>data</span>
            <span className={styles.teV} title={entry.calldata}>{entry.calldata ? short(entry.calldata, 12) : "\u2014"}</span>
            <span className={styles.teK}>actionHash</span>
            <span className={styles.teV}>{entry.actionHash || "\u2014"}</span>
          </div>
          {entry.stateDelta && (
            <>
              <div className={styles.teDivider} />
              <div className={styles.teSecLabel}>State Delta</div>
              <div className={styles.teGrid}>
                <span className={styles.teK}>rollupId</span>
                <span className={styles.teV}>{entry.stateDelta.rollupId}</span>
                <span className={styles.teK}>preState</span>
                <span className={styles.teV}>{short(entry.stateDelta.preState, 10)}</span>
                <span className={styles.teK}>postState</span>
                <span className={`${styles.teV} ${styles.teVHl}`}>{short(entry.stateDelta.postState, 10)}</span>
              </div>
            </>
          )}
        </div>
      )}
    </div>
  );
}

// ─── Dual Tables ───

function DualTables({ entries, allSteps, currentStep, expandedSet, onToggle }: {
  entries: VisualizerEntry[];
  allSteps: StepDef[];
  currentStep: number;
  expandedSet: Set<number>;
  onToggle: (idx: number) => void;
}) {
  const curStep = allSteps[currentStep]!;

  const consumed = new Set<number>();
  for (let i = 0; i <= currentStep; i++)
    for (const idx of allSteps[i]!.justConsumed) consumed.add(idx);

  const l1 = entries.filter((e) => curStep.l1Entries.includes(e.index));
  const l2 = entries.filter((e) => curStep.l2Entries.includes(e.index));

  function status(idx: number): "added" | "consumed" | "normal" {
    if (curStep.justAdded.includes(idx)) return "added";
    if (consumed.has(idx)) return "consumed";
    return "normal";
  }

  const l1Active = l1.filter((e) => !consumed.has(e.index)).length;
  const l2Active = l2.filter((e) => !consumed.has(e.index)).length;

  function Panel({ chain, label, items, active }: { chain: "l1" | "l2"; label: string; items: VisualizerEntry[]; active: number }) {
    return (
      <div className={`${styles.tblPanel} ${chain === "l1" ? styles.tblL1 : styles.tblL2}`}>
        <div className={styles.tblHdr}>
          <span className={styles.tblTitle}>
            <span className={`${styles.tblDot} ${chain === "l1" ? styles.tblDotL1 : styles.tblDotL2}`} />
            {label}
          </span>
          <span className={styles.tblCnt}>
            {active}/{items.length} active
          </span>
        </div>
        <div className={styles.tblBody}>
          {items.length === 0 ? (
            <div className={styles.tblEmpty}>No entries at this step</div>
          ) : (
            items.map((e) => (
              <ExpandableEntry key={e.index} entry={e} status={status(e.index)}
                expanded={expandedSet.has(e.index)} onToggle={() => onToggle(e.index)} />
            ))
          )}
        </div>
      </div>
    );
  }

  return (
    <div className={styles.dualTables}>
      <Panel chain="l1" label="L1 Execution Table" items={l1} active={l1Active} />
      <Panel chain="l2" label="L2 Execution Table" items={l2} active={l2Active} />
    </div>
  );
}

// ─── Step List ───

function StepList({ steps, currentStep, onJump }: { steps: StepDef[]; currentStep: number; onJump: (i: number) => void }) {
  return (
    <div className={styles.stepList}>
      <div className={styles.stepListHdr}>Execution Steps</div>
      {steps.map((s, i) => (
        <div key={i}
          className={`${styles.si} ${i < currentStep ? styles.siDone : ""} ${i === currentStep ? styles.siCurrent : ""}`}
          onClick={() => onJump(i)}
        >
          <div className={`${styles.siChain} ${s.chain === "l1" ? styles.siChainL1 : styles.siChainL2}`}>
            {s.chain.toUpperCase()}
          </div>
          <div className={styles.siNum}>{i + 1}</div>
          <div className={styles.siContent}>
            <div className={styles.siTitle}>{s.title}</div>
            {i === currentStep && <div className={styles.siDetail}>{s.detail}</div>}
            <div className={styles.siChanges}>
              {s.justAdded.length > 0 && <span className={styles.siAdd}>+{s.justAdded.length} added</span>}
              {s.justConsumed.length > 0 && <span className={styles.siRm}>{"\u2212"}{s.justConsumed.length} consumed</span>}
              {s.justAdded.length === 0 && s.justConsumed.length === 0 && <span className={styles.siNoop}>no table changes</span>}
            </div>
          </div>
          {i < currentStep && <div className={styles.siCheck}>{"\u2713"}</div>}
          {i === currentStep && <div className={styles.siArrow}>{"\u25B6"}</div>}
        </div>
      ))}
    </div>
  );
}

// ─── Live Timeline + Flow ───

function StepTimeline({ currentStep }: { currentStep: number }) {
  return (
    <div className={styles.timeline}>
      {STEP_LABELS.map((label, i) => {
        let cls = styles.tlStep;
        if (i < currentStep) cls += ` ${styles.tlDone}`;
        else if (i === currentStep) cls += ` ${styles.tlActive}`;
        return (
          <div key={label} className={styles.tlGroup}>
            {i > 0 && <div className={`${styles.tlLine} ${i <= currentStep ? styles.tlLineDone : ""}`} />}
            <div className={cls}>
              <div className={styles.tlDot}>
                {i < currentStep ? (
                  <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3"><polyline points="20 6 9 17 4 12" /></svg>
                ) : <span className={styles.tlDotInner} />}
              </div>
              <span className={styles.tlLabel}>{label}</span>
            </div>
          </div>
        );
      })}
    </div>
  );
}

function EntriesTable({ entries, currentStep }: { entries: VisualizerEntry[]; currentStep: number }) {
  if (entries.length === 0) return null;
  return (
    <div className={styles.entriesSection}>
      <div className={styles.sectionLabel}>Execution Entries</div>
      <div className={styles.entriesTable}>
        <div className={styles.entriesHeader}>
          <span>#</span><span>Type</span><span>Destination</span><span>Data</span>
        </div>
        {entries.map((entry) => {
          const isActive = (currentStep === 2 && entry.index === 0) || (currentStep === 3 && entry.index <= 2);
          return (
            <div key={entry.index} className={`${styles.entryRow} ${isActive ? styles.entryActive : ""} ${currentStep >= 4 ? styles.entryDone : ""}`}>
              <span className={styles.colIdx}>{entry.index}</span>
              <span>
                {entry.actionType ? (
                  <span className={`${styles.typeBadge} ${entry.actionType === "CALL" ? styles.typeCall : styles.typeResult}`}>{entry.actionType}</span>
                ) : <span className={styles.typeBadge}>IMM</span>}
              </span>
              <span>
                {entry.destination ? <ExplorerLink value={entry.destination} chain={entry.actionType === "RESULT" ? "l1" : "l2"} className={styles.destLink} /> : "\u2014"}
              </span>
              <span title={entry.calldata}>{entry.calldata ? short(entry.calldata, 6) : "\u2014"}</span>
            </div>
          );
        })}
      </div>
    </div>
  );
}

// ─── Main Component ───

export function DebugTxMode({ liveState, liveTargetAddress, liveCalldata, onBack, initialDebugHash }: Props) {
  // Manual simulation
  const [targetAddr, setTargetAddr] = useState("");
  const [calldata, setCalldata] = useState("");
  const [proxyAddr, setProxyAddr] = useState("");
  const [simulating, setSimulating] = useState(false);
  const [manualResult, setManualResult] = useState<{
    simulation: SimulateCallResult; entries: VisualizerEntry[]; targetAddress: string; calldata: string;
  } | null>(null);
  const [manualError, setManualError] = useState<string | null>(null);

  // Debug existing tx
  const [debugHash, setDebugHash] = useState("");
  const [debugging, setDebugging] = useState(false);
  const [debugResult, setDebugResult] = useState<DebugTxResult | null>(null);
  const [debugError, setDebugError] = useState<string | null>(null);

  // Step navigation
  const [debugStep, setDebugStep] = useState(-1);
  const [expandedEntries, setExpandedEntries] = useState<Set<number>>(new Set());
  const [playing, setPlaying] = useState(false);
  const [showTxDetails, setShowTxDetails] = useState(false);
  const playTimer = useRef<ReturnType<typeof setInterval> | null>(null);

  const debugSteps = debugResult ? buildDebugSteps(debugResult) : [];
  const totalSteps = debugSteps.length;

  const stepNext = useCallback(() => setDebugStep((s) => Math.min(s + 1, totalSteps - 1)), [totalSteps]);
  const stepPrev = useCallback(() => setDebugStep((s) => Math.max(s - 1, 0)), []);
  const stepReset = useCallback(() => {
    setDebugStep(0);
    setExpandedEntries(new Set());
    setPlaying(false);
    if (playTimer.current) { clearInterval(playTimer.current); playTimer.current = null; }
  }, []);
  const stepJump = useCallback((i: number) => setDebugStep(Math.max(0, Math.min(i, totalSteps - 1))), [totalSteps]);

  const togglePlay = useCallback(() => {
    setPlaying((p) => {
      if (p) {
        if (playTimer.current) { clearInterval(playTimer.current); playTimer.current = null; }
        return false;
      }
      playTimer.current = setInterval(() => {
        setDebugStep((s) => {
          if (s >= totalSteps - 1) {
            setTimeout(() => {
              if (playTimer.current) { clearInterval(playTimer.current); playTimer.current = null; }
              setPlaying(false);
            }, 0);
            return s;
          }
          return s + 1;
        });
      }, 1500);
      return true;
    });
  }, [totalSteps]);

  const toggleEntry = useCallback((idx: number) => {
    setExpandedEntries((prev) => {
      const next = new Set(prev);
      if (next.has(idx)) next.delete(idx); else next.add(idx);
      return next;
    });
  }, []);

  const handleSimulate = useCallback(async () => {
    if (!targetAddr) return;
    const cd = calldata || "0x";
    setSimulating(true); setManualError(null); setManualResult(null);
    try {
      const result = await rpcCall(config.l2Rpc, "syncrollups_simulateCall", [targetAddr, cd]) as SimulateCallResult | null;
      if (!result) { setManualError("simulateCall returned null"); return; }
      const entries = buildVisualizerEntries(result, proxyAddr || "0x0", targetAddr, cd);
      setManualResult({ simulation: result, entries, targetAddress: targetAddr, calldata: cd });
    } catch (e) { setManualError((e as Error).message); }
    finally { setSimulating(false); }
  }, [targetAddr, calldata, proxyAddr]);

  const handleDebug = useCallback(async () => {
    if (!debugHash) return;
    setDebugging(true); setDebugError(null); setDebugResult(null);
    try { setDebugResult(await debugTransaction(debugHash.trim())); }
    catch (e) { setDebugError((e as Error).message); }
    finally { setDebugging(false); }
  }, [debugHash]);

  const clearDebug = useCallback(() => {
    setDebugResult(null); setDebugStep(-1); setPlaying(false); setShowTxDetails(false);
    if (playTimer.current) { clearInterval(playTimer.current); playTimer.current = null; }
  }, []);

  // Auto-trigger
  const lastTriggeredHash = useRef<string | null>(null);
  useEffect(() => {
    if (initialDebugHash && initialDebugHash !== lastTriggeredHash.current) {
      lastTriggeredHash.current = initialDebugHash;
      setDebugHash(initialDebugHash);
      setDebugging(true); setDebugError(null); setDebugResult(null);
      debugTransaction(initialDebugHash)
        .then((r) => setDebugResult(r))
        .catch((e) => setDebugError((e as Error).message))
        .finally(() => setDebugging(false));
    }
  }, [initialDebugHash]);

  // Init step on result
  useEffect(() => { if (debugResult) { setDebugStep(0); setExpandedEntries(new Set()); } }, [debugResult]);

  // Keyboard
  useEffect(() => {
    if (!debugResult) return;
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
      if (e.key === "ArrowRight") { e.preventDefault(); stepNext(); }
      else if (e.key === "ArrowLeft") { e.preventDefault(); stepPrev(); }
      else if (e.key === " ") { e.preventDefault(); togglePlay(); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [debugResult, stepNext, stepPrev, togglePlay]);

  // Cleanup
  useEffect(() => () => { if (playTimer.current) clearInterval(playTimer.current); }, []);

  const hasLive = liveState.active;
  const hasManual = manualResult !== null;
  const hasDebug = debugResult !== null;
  const isDebugMode = hasDebug && debugSteps.length > 0 && debugStep >= 0 && debugStep < debugSteps.length;

  // ─── Debug Mode: Full-width layout ───
  if (isDebugMode) {
    const curStep = debugSteps[debugStep]!;
    return (
      <div>
        {/* Top bar */}
        <div className={styles.topBar} style={{ marginTop: 0 }}>
          <button className={styles.backBtn} onClick={onBack}>
            <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
              <line x1="19" y1="12" x2="5" y2="12" /><polyline points="12 19 5 12 12 5" />
            </svg>
            Dashboard
          </button>
          <h1 className={styles.pageTitle}>Transaction Debug</h1>
          <div className={styles.topRight}>
            <span className={debugResult.status === "confirmed" ? styles.statusOk : debugResult.status === "failed" ? styles.statusFail : styles.statusPending}>
              {debugResult.status}
            </span>
            <TxLink hash={debugResult.txHash} chain="l1" className={styles.topTxLink} />
            <button className="btn btn-sm btn-outline" onClick={clearDebug}>Close</button>
          </div>
        </div>

        {/* Controls */}
        <div className={styles.controls}>
          <div className={styles.controlsLeft}>
            <button className={styles.ctrlBtn} onClick={stepReset} title="Reset (R)">
              <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                <polyline points="1 4 1 10 7 10" /><path d="M3.51 15a9 9 0 1 0 2.13-9.36L1 10" />
              </svg>
            </button>
            <button className={styles.ctrlBtn} onClick={stepPrev} disabled={debugStep <= 0} title="Previous (\u2190)">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                <polyline points="15 18 9 12 15 6" />
              </svg>
            </button>
            <button className={`${styles.ctrlBtnPlay} ${playing ? styles.ctrlBtnPlayActive : ""}`} onClick={togglePlay} title="Play/Pause (Space)">
              {playing ? (
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5">
                  <line x1="10" y1="6" x2="10" y2="18" /><line x1="14" y1="6" x2="14" y2="18" />
                </svg>
              ) : (
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinejoin="round">
                  <polygon points="5 3 19 12 5 21 5 3" />
                </svg>
              )}
            </button>
            <button className={styles.ctrlBtn} onClick={stepNext} disabled={debugStep >= totalSteps - 1} title="Next (\u2192)">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                <polyline points="9 18 15 12 9 6" />
              </svg>
            </button>
          </div>

          <div className={styles.controlsCenter}>
            <ProgressBar current={debugStep} total={totalSteps} />
          </div>

          <div className={styles.controlsRight}>
            <span className={styles.kbdHint}>
              <kbd>{"\u2190"}</kbd> <kbd>{"\u2192"}</kbd> <kbd>Space</kbd>
            </span>
          </div>
        </div>

        {/* Current step banner */}
        <div className={`${styles.stepBanner} ${curStep.chain === "l1" ? styles.stepBannerL1 : styles.stepBannerL2}`}>
          <div className={styles.stepBannerLeft}>
            <span className={styles.stepNum}>Step {debugStep + 1}/{totalSteps}</span>
            <span className={`${styles.chainTag} ${curStep.chain === "l1" ? styles.chainTagL1 : styles.chainTagL2}`}>
              {curStep.chain.toUpperCase()}
            </span>
            <span className={styles.stepTitle}>{curStep.title}</span>
          </div>
        </div>
        <div className={styles.stepDetail}>{curStep.detail}</div>

        {/* Architecture diagram */}
        <ArchitectureDiagram step={debugStep} result={debugResult} />

        {/* TX details toggle */}
        <button className={styles.detailsToggle} onClick={() => setShowTxDetails(!showTxDetails)}>
          {showTxDetails ? "\u25B4 Hide" : "\u25BE Show"} transaction details
        </button>
        {showTxDetails && (
          <div className={styles.txDetails}>
            <div className={styles.txGrid}>
              <span className={styles.txK}>From</span>
              <ExplorerLink value={debugResult.from} chain="l1" className={styles.txV} />
              <span className={styles.txK}>Proxy (L1)</span>
              <ExplorerLink value={debugResult.proxyAddress} chain="l1" className={styles.txV} />
              <span className={styles.txK}>Target (L2)</span>
              <ExplorerLink value={debugResult.targetAddress} chain="l2" className={styles.txV} />
              <span className={styles.txK}>Calldata</span>
              <span className={styles.txV} title={debugResult.calldata}>{short(debugResult.calldata, 16)}</span>
              {debugResult.gasUsed && <><span className={styles.txK}>Gas Used</span><span className={styles.txV}>{debugResult.gasUsed}</span></>}
              {debugResult.blockNumber && <><span className={styles.txK}>L1 Block</span><span className={styles.txV}>#{debugResult.blockNumber.toLocaleString()}</span></>}
              <span className={styles.txK}>Simulation</span>
              <span className={debugResult.simulation.success ? styles.simOk : styles.simErr}>
                {debugResult.simulation.success ? "Success" : "Reverted"}
              </span>
            </div>
          </div>
        )}

        {/* Main content: tables + step list */}
        <div className={styles.debugMain}>
          <div className={styles.debugContent}>
            <DualTables entries={debugResult.entries} allSteps={debugSteps} currentStep={debugStep}
              expandedSet={expandedEntries} onToggle={toggleEntry} />
          </div>
          <div className={styles.debugSide}>
            <StepList steps={debugSteps} currentStep={debugStep} onJump={stepJump} />
          </div>
        </div>
      </div>
    );
  }

  // ─── Default Mode: Sidebar + Main ───
  return (
    <div>
      <div className={styles.layout}>
        <div className={styles.sidebar}>
          {/* Debug input */}
          <div className={styles.card}>
            <div className={styles.cardTitle}>Debug Transaction</div>
            <div className={styles.cardSub}>Paste an L1 tx hash to reconstruct the full cross-chain execution flow</div>
            <div className={styles.formGroup}>
              <label className={styles.label}>L1 Transaction Hash</label>
              <input type="text" className={styles.input} value={debugHash}
                onChange={(e) => setDebugHash(e.target.value)} placeholder="0x..." />
            </div>
            <button className="btn btn-solid btn-yellow btn-block" onClick={handleDebug} disabled={!debugHash.trim() || debugging}>
              {debugging ? <><span className="btn-spinner" /> Analyzing...</> : <>Debug TX</>}
            </button>
            {debugError && <div className={styles.errorMsg}>{debugError}</div>}
          </div>

          {/* Manual simulation */}
          <div className={styles.card}>
            <div className={styles.cardTitle}>Manual Simulation</div>
            <div className={styles.cardSub}>Call syncrollups_simulateCall on L2</div>
            <div className={styles.formGroup}>
              <label className={styles.label}>Target Address (L2)</label>
              <input type="text" className={styles.input} value={targetAddr}
                onChange={(e) => setTargetAddr(e.target.value)} placeholder="0x..." />
            </div>
            <div className={styles.formGroup}>
              <label className={styles.label}>Calldata</label>
              <input type="text" className={styles.input} value={calldata}
                onChange={(e) => setCalldata(e.target.value)} placeholder="0x..." />
            </div>
            <div className={styles.formGroup}>
              <label className={styles.label}>Proxy Address (L1) <span className={styles.optional}>optional</span></label>
              <input type="text" className={styles.input} value={proxyAddr}
                onChange={(e) => setProxyAddr(e.target.value)} placeholder="0x..." />
            </div>
            <button className="btn btn-solid btn-block" onClick={handleSimulate} disabled={!targetAddr || simulating}>
              {simulating ? <><span className="btn-spinner" /> Simulating...</> : "Simulate Call"}
            </button>
            {manualError && <div className={styles.errorMsg}>{manualError}</div>}
          </div>

          {hasLive && (
            <div className={styles.card}>
              <div className={styles.liveIndicator}>
                <span className={styles.liveDot} />
                <span>Live cross-chain call in progress</span>
              </div>
              <div className={styles.liveStep}>
                Step {liveState.currentStep + 1}/5: {STEP_LABELS[liveState.currentStep] ?? "..."}
              </div>
            </div>
          )}
        </div>

        <div className={styles.main}>
          {/* Live */}
          {hasLive && (
            <div className={styles.card}>
              <div className={styles.resultHeader}>
                <span className={styles.resultTitle}>Live Transaction</span>
                <span className={styles.liveBadge}><span className={styles.liveDot} /> LIVE</span>
              </div>
              <StepTimeline currentStep={liveState.currentStep} />
              {liveTargetAddress && (
                <div className={styles.infoRow}>
                  <div className={styles.infoItem}>
                    <span className={styles.infoLabel}>Target</span>
                    <ExplorerLink value={liveTargetAddress} chain="l2" className={styles.infoValue} />
                  </div>
                  {liveCalldata && (
                    <div className={styles.infoItem}>
                      <span className={styles.infoLabel}>Calldata</span>
                      <span className={styles.infoValue} title={liveCalldata}>{short(liveCalldata, 12)}</span>
                    </div>
                  )}
                </div>
              )}
              {liveState.simulation && (
                <div className={styles.simResult}>
                  <div className={styles.simRow}>
                    <span className={styles.simLabel}>Simulation</span>
                    <span className={liveState.simulation.success ? styles.simOk : styles.simErr}>
                      {liveState.simulation.success ? "Success" : "Reverted"}
                    </span>
                  </div>
                </div>
              )}
              <EntriesTable entries={liveState.entries} currentStep={liveState.currentStep} />
              {liveState.l1TxHash && (
                <div className={styles.txRow}>
                  <span className={styles.txLabel}>L1 TX</span>
                  <TxLink hash={liveState.l1TxHash} chain="l1" className={styles.txValue} />
                </div>
              )}
              {liveState.error && <div className={styles.errorMsg}>{liveState.error}</div>}
            </div>
          )}

          {/* Manual result */}
          {hasManual && (
            <div className={styles.card}>
              <div className={styles.resultHeader}>
                <span className={styles.resultTitle}>Simulation Result</span>
                <button className="btn btn-sm btn-outline" onClick={() => setManualResult(null)}>Clear</button>
              </div>
              <StepTimeline currentStep={1} />
              <div className={styles.infoRow}>
                <div className={styles.infoItem}>
                  <span className={styles.infoLabel}>Target</span>
                  <ExplorerLink value={manualResult.targetAddress} chain="l2" className={styles.infoValue} />
                </div>
                <div className={styles.infoItem}>
                  <span className={styles.infoLabel}>Calldata</span>
                  <span className={styles.infoValue} title={manualResult.calldata}>{short(manualResult.calldata, 12)}</span>
                </div>
              </div>
              <div className={styles.simResult}>
                <div className={styles.simRow}>
                  <span className={styles.simLabel}>Simulation</span>
                  <span className={manualResult.simulation.success ? styles.simOk : styles.simErr}>
                    {manualResult.simulation.success ? "Success" : "Reverted"}
                  </span>
                </div>
              </div>
              <EntriesTable entries={manualResult.entries} currentStep={1} />
            </div>
          )}

          {/* Empty */}
          {!hasLive && !hasManual && !hasDebug && (
            <div className={styles.emptyState}>
              <div className={styles.emptyIcon}>
                <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1" strokeLinecap="round" strokeLinejoin="round">
                  <circle cx="11" cy="11" r="8" /><line x1="21" y1="21" x2="16.65" y2="16.65" />
                </svg>
              </div>
              <div className={styles.emptyTitle}>No simulation data</div>
              <div className={styles.emptySub}>
                Use the form to manually simulate a call, debug an existing L1 transaction, or send a cross-chain transaction from the dashboard.
              </div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
