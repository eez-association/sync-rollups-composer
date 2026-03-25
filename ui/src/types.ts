/** Shared types for the dashboard */

export interface LogEntry {
  id: number;
  time: string;
  message: string;
  type: "ok" | "err" | "info";
}

export interface ChainStats {
  blockNumber: number | null;
  stateRoot: string;
  /** Block timestamp (unix seconds) */
  timestamp: number | null;
  /** Gas used in latest block */
  gasUsed: number | null;
  /** Gas limit of latest block */
  gasLimit: number | null;
  /** Number of transactions in latest block */
  txCount: number | null;
}

export interface L2Stats extends ChainStats {
  synced: boolean | null;
  /** Current gas price in gwei */
  gasPrice: string | null;
}

export interface WalletState {
  address: string | null;
  chainId: string | null;
  l1Balance: string | null;
  l2Balance: string | null;
  isConnected: boolean;
}

/** Result from syncrollups_simulateCall — returns only success + returnData */
export interface SimulateCallResult {
  success: boolean;
  returnData: string;
}

/** Entry displayed in the execution visualizer */
export interface VisualizerEntry {
  index: number;
  label: string;
  actionType: string;
  actionHash: string;
  stateDelta: { rollupId: string; preState: string; postState: string } | null;
  destination: string;
  sourceAddress: string;
  calldata: string;
}
