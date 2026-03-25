/**
 * VisualizerView — Unified container with 3 mode tabs:
 *   - Block Explorer (default)
 *   - Live (LiveFeed — cross-chain block monitor)
 *   - Debug TX (extracted DebugTxMode)
 */

import { useState, useEffect, useCallback } from "react";
import type { ExecutionVisualizerState } from "../hooks/useExecutionVisualizer";
import { BlockExplorer } from "./visualizer/BlockExplorer";
import { LiveFeed } from "./monitor/LiveFeed";
import { DebugTxMode } from "./visualizer/DebugTxMode";
import styles from "./VisualizerView.module.css";

type Mode = "explorer" | "live" | "debug";

interface Props {
  liveState: ExecutionVisualizerState;
  liveTargetAddress: string;
  liveCalldata: string;
  onBack: () => void;
  initialDebugHash?: string | null;
  initialMode?: Mode;
  initialBlock?: number | null;
}

const MODE_LABELS: Record<Mode, string> = {
  explorer: "Block Explorer",
  live: "Live",
  debug: "Debug TX",
};

export function VisualizerView({ liveState, liveTargetAddress, liveCalldata, onBack, initialDebugHash, initialMode, initialBlock }: Props) {
  const [mode, setMode] = useState<Mode>(() => {
    if (initialDebugHash) return "debug";
    if (initialMode) return initialMode;
    return "explorer";
  });

  // Internal block target — set by LiveFeed navigation, merged with prop
  const [explorerBlock, setExplorerBlock] = useState<number | null>(null);
  const effectiveBlock = explorerBlock ?? initialBlock ?? null;

  // Sync mode when initialDebugHash or initialMode changes
  useEffect(() => {
    if (initialDebugHash) {
      setMode("debug");
    } else if (initialMode) {
      setMode(initialMode);
    }
  }, [initialDebugHash, initialMode]);

  // Navigate from Live → Block Explorer with a specific L1 block
  const handleNavigateToBlock = useCallback((blockNumber: number) => {
    setExplorerBlock(blockNumber);
    setMode("explorer");
  }, []);

  // Clear internal block when user manually switches tabs
  const handleSetMode = useCallback((m: Mode) => {
    if (m !== "explorer") setExplorerBlock(null);
    setMode(m);
  }, []);

  return (
    <div className={styles.page}>
      {/* Top bar */}
      <div className={styles.topBar}>
        <button className="btn btn-sm btn-outline" onClick={onBack}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
            <line x1="19" y1="12" x2="5" y2="12" /><polyline points="12 19 5 12 12 5" />
          </svg>
          Dashboard
        </button>
        <h1 className={styles.pageTitle}>Execution Visualizer</h1>
      </div>

      {/* Mode tabs */}
      <div className={styles.modeTabs}>
        {(Object.keys(MODE_LABELS) as Mode[]).map((m) => (
          <button
            key={m}
            className={`${styles.modeTab} ${mode === m ? styles.modeTabActive : ""}`}
            onClick={() => handleSetMode(m)}
          >
            {MODE_LABELS[m]}
            {m === "live" && <span className={styles.modeTabDot} />}
          </button>
        ))}
      </div>

      {/* Content — key forces remount when explorerBlock changes */}
      {mode === "explorer" && <BlockExplorer key={`be-${effectiveBlock}`} initialBlock={effectiveBlock} />}
      {mode === "live" && <LiveFeed onNavigateToBlock={handleNavigateToBlock} />}
      {mode === "debug" && (
        <DebugTxMode
          liveState={liveState}
          liveTargetAddress={liveTargetAddress}
          liveCalldata={liveCalldata}
          onBack={onBack}
          initialDebugHash={initialDebugHash}
        />
      )}
    </div>
  );
}
