import React, { useState, useMemo, useCallback, useEffect } from "react";
import { useMonitorStore } from "../../store";
import type { TransactionBundle } from "../../types/visualization";
import { buildBundleSteps, type BundleStep } from "../../lib/callFlowBuilder";
import { ArchitectureDiagram } from "./ArchitectureDiagram";
import { buildBundleArchitecture, type StepTableState, type StepContractState } from "../../lib/bundleArchitecture";
import styles from "./BundleDetail.module.css";

type Props = {
  bundle: TransactionBundle;
  onClose: () => void;
};

export const BundleDetail: React.FC<Props> = ({ bundle, onClose }) => {
  const events = useMonitorStore((s) => s.events);
  const knownAddresses = useMonitorStore((s) => s.knownAddresses);
  const l1Contract = useMonitorStore((s) => s.l1ContractAddress);
  const l2Contract = useMonitorStore((s) => s.l2ContractAddress);
  const [activeStep, setActiveStep] = useState(0);

  const bundleEvents = useMemo(() => {
    const eventSet = new Set(bundle.events);
    return events.filter((e) => eventSet.has(e.id));
  }, [events, bundle.events]);

  const arch = useMemo(
    () => buildBundleArchitecture(bundleEvents, knownAddresses, l1Contract, l2Contract, events),
    [bundleEvents, knownAddresses, l1Contract, l2Contract, events],
  );

  const steps = useMemo(() => buildBundleSteps(arch.mergedEvents), [arch.mergedEvents]);

  const currentHighlight = arch.stepHighlights[activeStep];
  const activeNodesSet = useMemo(
    () => new Set(currentHighlight?.activeNodes ?? []),
    [currentHighlight],
  );
  const activeEdgesSet = useMemo(
    () => new Set(currentHighlight?.activeEdges ?? []),
    [currentHighlight],
  );

  const currentTable: StepTableState | undefined = arch.tableStates[activeStep];
  const currentState: StepContractState | undefined = arch.contractStates[activeStep];

  const currentStep = steps[activeStep];

  const handleKeyDown = useCallback(
    (e: KeyboardEvent) => {
      if (e.key === "ArrowRight" || e.key === "ArrowDown") {
        e.preventDefault();
        e.stopPropagation();
        setActiveStep((s) => Math.min(steps.length - 1, s + 1));
      } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
        e.preventDefault();
        e.stopPropagation();
        setActiveStep((s) => Math.max(0, s - 1));
      } else if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    },
    [steps.length, onClose],
  );

  useEffect(() => {
    window.addEventListener("keydown", handleKeyDown, true);
    return () => window.removeEventListener("keydown", handleKeyDown, true);
  }, [handleKeyDown]);

  return (
    <div
      className={styles.overlay}
      onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}
    >
      <div className={styles.modal}>
        {/* Header */}
        <div className={styles.modalHeader}>
          <div>
            <div className={styles.headerLeft}>
              <DirectionBadge direction={bundle.direction} />
              <span className={styles.bundleTitle}>{bundle.title}</span>
              <StatusDot status={bundle.status} />
            </div>
            <div className={styles.headerMeta}>
              {bundle.events.length} events | {bundle.actionHashes.length} action hashes |
              blocks {bundle.blockRange.from.toString()}-{bundle.blockRange.to.toString()} |
              arrow keys to navigate
            </div>
          </div>
          <button onClick={onClose} className={styles.closeBtn}>ESC</button>
        </div>

        {/* Body */}
        <div className={styles.modalBody}>
          <div className={styles.mainContent}>
            <ArchitectureDiagram
              l1Nodes={arch.l1Nodes}
              l2Nodes={arch.l2Nodes}
              edges={arch.edges}
              activeNodes={activeNodesSet}
              activeEdges={activeEdgesSet}
            />

            {currentStep && (
              <div className={styles.stepDesc}>
                <div className={styles.stepDescHeader}>
                  <div className={styles.stepBadge}>{activeStep + 1}</div>
                  <ChainBadge chain={currentStep.chain} />
                  <div style={{ fontWeight: 700, fontSize: "0.65rem" }}>
                    {currentHighlight?.description ?? currentStep.title}
                  </div>
                </div>
                <div className={styles.stepDescDetail}>{currentStep.detail}</div>
                <div className={styles.stepDescTx}>
                  <span>tx: {currentStep.txHash}</span>
                  <CopyButton text={currentStep.txHash} />
                </div>
              </div>
            )}

            <div className={styles.tables}>
              <MiniTablePanel
                title="L1 Execution Table"
                subtitle="Rollups"
                entries={currentTable?.l1 ?? []}
                chainColor="#3b82f6"
              />
              <MiniTablePanel
                title="L2 Execution Table"
                subtitle="ManagerL2"
                entries={currentTable?.l2 ?? []}
                chainColor="#a855f7"
              />
            </div>

            {currentState && currentState.entries.length > 0 && (
              <div className={styles.statePanel}>
                <div className={styles.sectionHeader}>Contract State</div>
                <div className={styles.stateGrid}>
                  {currentState.entries.map((e) => (
                    <div key={e.key} style={{ display: "flex", gap: 6 }}>
                      <span style={{ color: "var(--text-dim)" }}>{e.key}</span>
                      <span style={{ color: e.changed ? "var(--green)" : "var(--text)", fontWeight: e.changed ? 700 : 400 }}>
                        {e.value}
                      </span>
                    </div>
                  ))}
                </div>
              </div>
            )}
          </div>

          {/* Step sidebar */}
          <div className={styles.stepSidebar}>
            <div className={styles.stepSidebarTitle}>Steps ({steps.length})</div>

            <div className={styles.stepNav}>
              <button
                onClick={() => setActiveStep(Math.max(0, activeStep - 1))}
                disabled={activeStep === 0}
                className={styles.smallBtn}
              >
                Prev
              </button>
              <button
                onClick={() => setActiveStep(Math.min(steps.length - 1, activeStep + 1))}
                disabled={activeStep >= steps.length - 1}
                className={styles.smallBtn}
              >
                Next
              </button>
            </div>

            {steps.map((step, i) => {
              const tableSnap = arch.tableStates[i];
              return (
                <StepItem
                  key={step.eventId}
                  step={step}
                  index={i}
                  active={i === activeStep}
                  played={i < activeStep}
                  onClick={() => setActiveStep(i)}
                  highlight={arch.stepHighlights[i]}
                  tableChanges={tableSnap}
                />
              );
            })}
          </div>
        </div>
      </div>
    </div>
  );
};

// ─── Mini Execution Table Panel ──────────────────────────────

type MiniEntry = {
  stepStatus: string;
  actionHash: string;
  nextActionHash: string;
  delta: string | null;
  stateDeltas?: string[];
  actionDetail?: Record<string, string>;
  nextActionDetail?: Record<string, string>;
  fullActionHash?: string;
  fullNextActionHash?: string;
};

const MiniTablePanel: React.FC<{
  title: string;
  subtitle: string;
  entries: MiniEntry[];
  chainColor: string;
}> = ({ title, subtitle, entries, chainColor }) => {
  const active = entries.filter(e => e.stepStatus !== "consumed");
  return (
    <div
      style={{
        flex: 1,
        background: "var(--bg-card)",
        border: "1px solid var(--border)",
        borderRadius: 8,
        padding: "10px 12px",
        minWidth: 0,
      }}
    >
      <div style={{ display: "flex", justifyContent: "space-between", alignItems: "baseline", marginBottom: 6 }}>
        <div style={{ fontSize: "0.6rem", fontWeight: 700, color: chainColor }}>
          {title} <span style={{ color: "var(--text-dim)", fontWeight: 400 }}>({subtitle})</span>
        </div>
        <div style={{ fontSize: "0.5rem", color: "var(--text-dim)" }}>{active.length} entries</div>
      </div>
      {entries.length === 0 ? (
        <div style={{ fontSize: "0.55rem", color: "var(--text-dim)", fontStyle: "italic", textAlign: "center", padding: 8 }}>
          empty
        </div>
      ) : (
        entries.map((entry, i) => <TableEntryMini key={i} entry={entry} index={i} />)
      )}
    </div>
  );
};

const TableEntryMini: React.FC<{
  entry: MiniEntry;
  index: number;
}> = ({ entry, index }) => {
  const [expanded, setExpanded] = useState(true);
  const isJa = entry.stepStatus === "ja";
  const isJc = entry.stepStatus === "jc";
  const isConsumed = entry.stepStatus === "consumed";

  const borderColor = isJa ? "var(--cyan)" : isJc ? "var(--red)" : "var(--border)";
  const opacity = isConsumed ? 0.3 : 1;

  const actionFields = entry.actionDetail
    ? Object.entries(entry.actionDetail).filter(([k]) => k !== "computedHash" && k !== "actionHash")
    : [];
  const nextActionFields = entry.nextActionDetail
    ? Object.entries(entry.nextActionDetail).filter(([k]) => k !== "computedHash" && k !== "actionHash")
    : [];
  const hasDecodedFields = actionFields.length > 0 || nextActionFields.length > 0;

  return (
    <div
      style={{
        marginBottom: 4,
        borderRadius: 5,
        background: "var(--bg-inset)",
        border: `1px solid ${borderColor}`,
        opacity,
        fontSize: "0.58rem",
        transition: "all 0.2s",
        overflow: "hidden",
      }}
    >
      <div
        onClick={() => hasDecodedFields && setExpanded(!expanded)}
        style={{
          display: "flex",
          gap: 6,
          alignItems: "center",
          padding: "5px 8px",
          cursor: hasDecodedFields ? "pointer" : "default",
          textDecoration: isJc ? "line-through" : undefined,
        }}
      >
        <span style={{ color: "var(--text-dim)", fontWeight: 700 }}>#{index + 1}</span>
        <span style={{ color: "var(--cyan)" }}>{entry.actionHash}</span>
        <span style={{ color: "var(--text-dim)" }}>{"\u2192"}</span>
        <span style={{ color: "var(--yellow)", flex: 1, minWidth: 0, overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap" }}>{entry.nextActionHash}</span>
        {isJa && <span style={{ fontSize: "0.48rem", color: "var(--cyan)", fontWeight: 700, flexShrink: 0 }}>+added</span>}
        {isJc && <span style={{ fontSize: "0.48rem", color: "var(--red)", fontWeight: 700, flexShrink: 0 }}>consumed</span>}
        {hasDecodedFields && (
          <span style={{ fontSize: "0.48rem", color: "var(--text-dim)", flexShrink: 0 }}>
            {expanded ? "\u25B2" : "\u25BC"}
          </span>
        )}
      </div>

      {entry.stateDeltas && entry.stateDeltas.length > 0 && (
        <div style={{ fontSize: "0.52rem", color: "var(--green)", padding: "0 8px 4px" }}>
          {entry.stateDeltas.join("; ")}
        </div>
      )}

      {expanded && hasDecodedFields && (
        <div style={{ borderTop: "1px solid var(--border)", padding: "6px 8px", background: "rgba(0,0,0,0.2)" }}>
          {actionFields.length > 0 && (
            <div style={{ marginBottom: 5 }}>
              <div style={{ fontSize: "0.48rem", fontWeight: 700, color: "var(--accent)", textTransform: "uppercase", letterSpacing: "0.04em", marginBottom: 3 }}>
                Action (hashed as actionHash)
              </div>
              <div style={{ display: "grid", gridTemplateColumns: "85px 1fr", gap: "2px 8px", fontSize: "0.52rem" }}>
                {actionFields.map(([k, v]) => (
                  <React.Fragment key={k}>
                    <span style={{ color: "var(--text-dim)" }}>{k}</span>
                    <span style={{ color: k === "actionType" || (k === "data" && v !== "0x") ? "var(--cyan)" : "var(--text)", wordBreak: "break-all" }}>{v}</span>
                  </React.Fragment>
                ))}
              </div>
            </div>
          )}
          {nextActionFields.length > 0 && (
            <div>
              <div style={{ fontSize: "0.48rem", fontWeight: 700, color: "var(--accent)", textTransform: "uppercase", letterSpacing: "0.04em", marginBottom: 3 }}>
                Next Action (returned on match)
              </div>
              <div style={{ display: "grid", gridTemplateColumns: "85px 1fr", gap: "2px 8px", fontSize: "0.52rem" }}>
                {nextActionFields.map(([k, v]) => (
                  <React.Fragment key={k}>
                    <span style={{ color: "var(--text-dim)" }}>{k}</span>
                    <span style={{ color: k === "actionType" || (k === "data" && v !== "0x") ? "var(--cyan)" : "var(--text)", wordBreak: "break-all" }}>{v}</span>
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

// ─── Small components ────────────────────────────────

const ChainBadge: React.FC<{ chain: string }> = ({ chain }) => {
  const color = chain === "l1" ? "#3b82f6" : "#a855f7";
  return (
    <div
      className={styles.chainBadge}
      style={{ color, border: `1px solid ${color}30`, background: `${color}10` }}
    >
      {chain.toUpperCase()}
    </div>
  );
};

const DirectionBadge: React.FC<{ direction: string }> = ({ direction }) => {
  const color = direction.includes("L1") && direction.includes("L2")
    ? "var(--yellow)"
    : direction.includes("L1")
      ? "#3b82f6"
      : "#a855f7";
  return (
    <span
      className={styles.directionBadge}
      style={{ background: `${color}15`, color, border: `1px solid ${color}30` }}
    >
      {direction}
    </span>
  );
};

const StatusDot: React.FC<{ status: string }> = ({ status }) => (
  <div
    className={styles.statusDot}
    style={{
      background: status === "complete" ? "var(--green)" : "var(--yellow)",
      boxShadow: `0 0 6px ${status === "complete" ? "rgba(52,211,153,0.4)" : "rgba(245,158,11,0.4)"}`,
    }}
    title={status}
  />
);

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
      className={`${styles.copyBtn} ${copied ? styles.copyBtnCopied : ""}`}
      title="Copy to clipboard"
    >
      {copied ? "copied" : "copy"}
    </button>
  );
};

const StepItem: React.FC<{
  step: BundleStep;
  index: number;
  active: boolean;
  played: boolean;
  onClick: () => void;
  highlight?: { description: string };
  tableChanges?: StepTableState;
}> = ({ step, index, active, played, onClick, highlight, tableChanges }) => {
  const chainColor = step.chain === "l1" ? "#3b82f6" : "#a855f7";

  const l1Added = tableChanges?.l1.filter(e => e.stepStatus === "ja").length ?? 0;
  const l1Consumed = tableChanges?.l1.filter(e => e.stepStatus === "jc").length ?? 0;
  const l2Added = tableChanges?.l2.filter(e => e.stepStatus === "ja").length ?? 0;
  const l2Consumed = tableChanges?.l2.filter(e => e.stepStatus === "jc").length ?? 0;
  const hasChanges = l1Added + l1Consumed + l2Added + l2Consumed > 0;

  return (
    <div
      onClick={onClick}
      style={{
        display: "flex",
        gap: 6,
        padding: "5px 6px",
        borderRadius: 5,
        border: `1px solid ${active ? "var(--accent)" : "transparent"}`,
        background: active ? "var(--bg-inset)" : "transparent",
        marginBottom: 3,
        cursor: "pointer",
        opacity: active ? 1 : played ? 0.65 : 0.3,
        transition: "all 0.15s",
        fontSize: "0.55rem",
      }}
    >
      <div
        style={{
          width: 14,
          height: 14,
          borderRadius: "50%",
          background: active ? "var(--accent)" : "var(--bg-inset)",
          color: active ? "#fff" : "var(--text-dim)",
          display: "flex",
          alignItems: "center",
          justifyContent: "center",
          fontSize: "0.4rem",
          fontWeight: 700,
          flexShrink: 0,
          marginTop: 1,
        }}
      >
        {index + 1}
      </div>
      <div
        style={{
          flexShrink: 0,
          padding: "0 4px",
          borderRadius: 3,
          fontSize: "0.4rem",
          fontWeight: 700,
          marginTop: 1,
          color: chainColor,
          border: `1px solid ${chainColor}30`,
          background: `${chainColor}10`,
        }}
      >
        {step.chain.toUpperCase()}
      </div>
      <div style={{ flex: 1, minWidth: 0 }}>
        <div style={{ fontWeight: 700, fontSize: "0.5rem" }}>
          {highlight?.description ?? step.title}
        </div>
        <div
          style={{
            color: "var(--text-dim)",
            fontSize: "0.45rem",
            overflow: "hidden",
            textOverflow: "ellipsis",
            whiteSpace: "nowrap",
          }}
        >
          {step.detail}
        </div>
        {hasChanges && (
          <div style={{ fontSize: "0.4rem", marginTop: 2 }}>
            {l1Added > 0 && <span style={{ color: "var(--cyan)", marginRight: 4 }}>+L1</span>}
            {l1Consumed > 0 && <span style={{ color: "var(--red)", marginRight: 4 }}>-L1</span>}
            {l2Added > 0 && <span style={{ color: "var(--cyan)", marginRight: 4 }}>+L2</span>}
            {l2Consumed > 0 && <span style={{ color: "var(--red)", marginRight: 4 }}>-L2</span>}
          </div>
        )}
      </div>
    </div>
  );
};
