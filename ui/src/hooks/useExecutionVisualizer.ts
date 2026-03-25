import { useCallback, useRef, useState } from "react";
import { config } from "../config";
import { rpcCall } from "../rpc";
import { buildVisualizerEntries } from "../lib/crossChainEntries";
import type { CrossChainPhase } from "./useCrossChain";
import type { SimulateCallResult, VisualizerEntry } from "../types";

export interface ExecutionVisualizerState {
  active: boolean;
  simulation: SimulateCallResult | null;
  entries: VisualizerEntry[];
  currentStep: number; // 0-4
  l1TxHash: string | null;
  error: string | null;
}

const INITIAL: ExecutionVisualizerState = {
  active: false,
  simulation: null,
  entries: [],
  currentStep: -1,
  l1TxHash: null,
  error: null,
};

/**
 * Step definitions:
 *  0: Initiate — user clicked Send, show target + calldata
 *  1: Simulate — simulateCall returned, show entries + state deltas
 *  2: Post Batch — entries submitted to Rollups.postBatch on L1
 *  3: Execute — L1 tx sent via proxy, execution table consumed
 *  4: Confirmed — L1 tx confirmed, L2 state updated
 */
export const STEP_LABELS = [
  "Initiate",
  "Simulate",
  "Post Batch",
  "Execute",
  "Confirmed",
] as const;

export function useExecutionVisualizer() {
  const [state, setState] = useState<ExecutionVisualizerState>(INITIAL);
  const prevPhaseRef = useRef<CrossChainPhase>("idle");

  /** Run syncrollups_simulateCall on the L2 RPC */
  const simulate = useCallback(
    async (targetAddress: string, calldata: string, proxyAddress: string) => {
      try {
        const result = await rpcCall(config.l2Rpc, "syncrollups_simulateCall", [
          targetAddress,
          calldata,
        ]) as SimulateCallResult | null;

        if (!result) {
          setState((s) => ({ ...s, currentStep: 1, error: "simulateCall returned null" }));
          return;
        }

        const entries = buildVisualizerEntries(
          result,
          proxyAddress,
          targetAddress,
          calldata,
        );

        setState((s) => ({
          ...s,
          simulation: result,
          entries,
          currentStep: 1,
          error: null,
        }));
      } catch (e) {
        // Non-fatal — visualizer still works, just won't show simulation data
        setState((s) => ({
          ...s,
          currentStep: 1,
          error: `Simulation: ${(e as Error).message}`,
        }));
      }
    },
    [],
  );

  /** Sync visualizer steps with cross-chain phase transitions */
  const syncWithPhase = useCallback(
    (
      phase: CrossChainPhase,
      txHash: string | null,
      targetAddress: string,
      calldata: string,
      proxyAddress: string,
    ) => {
      const prev = prevPhaseRef.current;
      prevPhaseRef.current = phase;

      if (phase === "sending" && prev !== "sending") {
        // Activate visualizer at step 0, then trigger simulation
        setState({
          active: true,
          simulation: null,
          entries: [],
          currentStep: 0,
          l1TxHash: null,
          error: null,
        });
        if (targetAddress && calldata) {
          simulate(targetAddress, calldata, proxyAddress);
        }
        return;
      }

      if (phase === "l1-pending" && prev !== "l1-pending") {
        setState((s) => ({
          ...s,
          currentStep: s.currentStep < 2 ? 2 : 3,
          l1TxHash: txHash,
        }));
        // After a short delay, advance to step 3 (execute)
        setTimeout(() => {
          setState((s) =>
            s.active && s.currentStep === 2
              ? { ...s, currentStep: 3 }
              : s,
          );
        }, 1500);
        return;
      }

      if (phase === "confirmed" && prev !== "confirmed") {
        setState((s) => ({
          ...s,
          currentStep: 4,
          l1TxHash: txHash,
        }));
        return;
      }

      if (phase === "failed" && prev !== "failed") {
        setState((s) => ({
          ...s,
          error: "Transaction failed",
        }));
      }
    },
    [simulate],
  );

  const reset = useCallback(() => {
    setState(INITIAL);
    prevPhaseRef.current = "idle";
  }, []);

  return { state, syncWithPhase, reset };
}
