import type { SimulateCallResult, VisualizerEntry } from "../types";

/**
 * Build the 3 visualizer entries from a simulateCall result + known addresses,
 * mirroring the L1 proxy's build_post_batch_calldata() logic.
 *
 * Entry 0: Immediate state delta (state roots not available from simulateCall)
 * Entry 1: CALL action (routes calldata to L2 destination)
 * Entry 2: RESULT action (returns data back to caller)
 */
export function buildVisualizerEntries(
  sim: SimulateCallResult,
  proxyAddress: string,
  targetAddress: string,
  calldata: string,
): VisualizerEntry[] {
  return [
    {
      index: 0,
      label: "Immediate State Update",
      actionType: "",
      actionHash: "",
      stateDelta: null,
      destination: "",
      sourceAddress: "",
      calldata: "",
    },
    {
      index: 1,
      label: "CALL Action",
      actionType: "CALL",
      actionHash: "",
      stateDelta: null,
      destination: targetAddress,
      sourceAddress: proxyAddress,
      calldata,
    },
    {
      index: 2,
      label: "RESULT Action",
      actionType: "RESULT",
      actionHash: "",
      stateDelta: null,
      destination: proxyAddress,
      sourceAddress: targetAddress,
      calldata: sim.returnData,
    },
  ];
}
