import { useCallback, useEffect, useRef, useState } from "react";
import { config } from "../config";
import { rpcCall } from "../rpc";
import {
  EXECUTOR_BYTECODE,
  ZERO_CONSTRUCTOR_ARGS,
  encodeExecutorConstructorArgs,
} from "../lib/flashLoanBytecode";

type Logger = (msg: string, type?: "ok" | "err" | "info") => void;
type SendTx = (params: Record<string, string>) => Promise<string>;

export type DeployPhase =
  | "checking"
  | "need-deploy"
  | "deploying-l2"
  | "deploying-proxy"
  | "deploying-l1"
  | "ready";

export interface DeployState {
  phase: DeployPhase;
  executorL2: string | null;
  proxyL1: string | null;
  executorL1: string | null;
  error: string | null;
  /** Which sub-step within deploy we're at (0-2) for the progress indicator */
  deployStep: number;
}

interface PersistedDeploy {
  executorL2: string;
  proxyL1: string;
  executorL1: string;
}

const INITIAL_STATE: DeployState = {
  phase: "checking",
  executorL2: null,
  proxyL1: null,
  executorL1: null,
  error: null,
  deployStep: 0,
};

/** Selector for Rollups.createCrossChainProxy(address,uint256) */
const CREATE_PROXY_SELECTOR = "2dd72120";

/** Selector for Rollups.computeCrossChainProxyAddress(address,uint256) */
const COMPUTE_PROXY_SELECTOR = "b761ba7e";

function padAddress(addr: string): string {
  return addr.replace("0x", "").toLowerCase().padStart(64, "0");
}

function padUint256(n: number | string): string {
  return BigInt(n).toString(16).padStart(64, "0");
}

function storageKey(walletAddress: string | null): string {
  return `flashLoanDeploy:${walletAddress || "demo"}`;
}

function loadFromStorage(walletAddress: string | null): PersistedDeploy | null {
  try {
    const raw = localStorage.getItem(storageKey(walletAddress));
    if (!raw) return null;
    return JSON.parse(raw) as PersistedDeploy;
  } catch {
    return null;
  }
}

function saveToStorage(walletAddress: string | null, data: Partial<PersistedDeploy>) {
  try {
    const existing = loadFromStorage(walletAddress) || {};
    localStorage.setItem(storageKey(walletAddress), JSON.stringify({ ...existing, ...data }));
  } catch {
    /* storage unavailable */
  }
}

function clearStorage(walletAddress: string | null) {
  try {
    localStorage.removeItem(storageKey(walletAddress));
  } catch {
    /* ignore */
  }
}

async function hasCode(rpcUrl: string, address: string): Promise<boolean> {
  try {
    const code = (await rpcCall(rpcUrl, "eth_getCode", [address, "latest"])) as string;
    return !!code && code !== "0x" && code !== "0x0" && code.length > 4;
  } catch {
    return false;
  }
}

async function pollReceipt(
  rpcUrl: string,
  txHash: string,
  maxAttempts = 60,
  intervalMs = 1000,
): Promise<{ contractAddress?: string; status?: string } | null> {
  for (let i = 0; i < maxAttempts; i++) {
    await new Promise((r) => setTimeout(r, intervalMs));
    try {
      const receipt = (await rpcCall(rpcUrl, "eth_getTransactionReceipt", [txHash])) as {
        contractAddress?: string;
        status?: string;
      } | null;
      if (receipt) return receipt;
    } catch {
      /* not mined yet */
    }
  }
  return null;
}

async function computeProxyAddress(
  executorL2: string,
  rollupId: string,
): Promise<string | null> {
  if (!config.rollupsAddress) return null;
  try {
    const calldata =
      "0x" +
      COMPUTE_PROXY_SELECTOR +
      padAddress(executorL2) +
      padUint256(rollupId);
    const result = (await rpcCall(config.l1Rpc, "eth_call", [
      { to: config.rollupsAddress, data: calldata },
      "latest",
    ])) as string;
    if (result && result.length >= 66) {
      return "0x" + result.slice(26, 66);
    }
  } catch {
    /* Rollups contract may not be deployed yet */
  }
  return null;
}

export function useFlashLoanDeploy(
  log: Logger,
  sendTx: SendTx,
  sendL1Tx: SendTx,
  walletAddress: string | null,
) {
  const [state, setState] = useState<DeployState>(INITIAL_STATE);
  const stateRef = useRef(state);
  stateRef.current = state;
  const walletRef = useRef(walletAddress);
  walletRef.current = walletAddress;

  // On mount (and when wallet changes), check localStorage and verify addresses
  useEffect(() => {
    let cancelled = false;

    async function check() {
      const stored = loadFromStorage(walletAddress);
      if (!stored || !stored.executorL2 || !stored.proxyL1 || !stored.executorL1) {
        if (!cancelled) setState((s) => ({ ...s, phase: "need-deploy" }));
        return;
      }

      // Verify all 3 addresses still have code
      const [l2Ok, l1ProxyOk, l1Ok] = await Promise.all([
        hasCode(config.l2Rpc, stored.executorL2),
        hasCode(config.l1Rpc, stored.proxyL1),
        hasCode(config.l1Rpc, stored.executorL1),
      ]);

      if (cancelled) return;

      if (l2Ok && l1ProxyOk && l1Ok) {
        setState({
          phase: "ready",
          executorL2: stored.executorL2,
          proxyL1: stored.proxyL1,
          executorL1: stored.executorL1,
          error: null,
          deployStep: 3,
        });
      } else {
        // Stale cache — chain was wiped
        clearStorage(walletAddress);
        setState((s) => ({ ...s, phase: "need-deploy" }));
        log("Cached executor addresses no longer have code — please redeploy.", "info");
      }
    }

    setState(INITIAL_STATE);
    check();

    return () => { cancelled = true; };
  }, [walletAddress]); // eslint-disable-line react-hooks/exhaustive-deps

  const deploy = useCallback(async () => {
    const rollupId = config.rollupId || "1";

    // Validate that flash loan infrastructure is deployed on this network.
    // Without these contracts, the FlashExecutor will be deployed with zero
    // addresses and execute() will always revert.
    const ZERO = "0x0000000000000000000000000000000000000000";
    const missingContracts: string[] = [];
    if (!config.flashPoolAddress || config.flashPoolAddress === ZERO)
      missingContracts.push("Flash Pool");
    if (!config.flashTokenAddress || config.flashTokenAddress === ZERO)
      missingContracts.push("Flash Token");
    if (!config.l1Bridge || config.l1Bridge === ZERO)
      missingContracts.push("L1 Bridge");

    if (missingContracts.length > 0) {
      const msg = `Flash loan infrastructure not deployed on this network: ${missingContracts.join(", ")}. Run the deploy-l2 service first.`;
      log(msg, "err");
      setState((s) => ({ ...s, error: msg, phase: "need-deploy" }));
      return;
    }

    setState((s) => ({
      ...s,
      phase: "deploying-l2",
      error: null,
      deployStep: 0,
      executorL2: null,
      proxyL1: null,
      executorL1: null,
    }));

    // ── Step 1: Deploy L2 Executor ───────────────────────────────────────────
    log("Step 1/3: Deploying L2 executor contract...", "info");
    let executorL2: string;
    try {
      const txHash = await sendTx({
        data: "0x" + EXECUTOR_BYTECODE + ZERO_CONSTRUCTOR_ARGS,
        gas: "0x100000",
        gasPrice: "0x77359400",
      });
      log(`L2 executor deploy tx: ${txHash.slice(0, 18)}...`);
      setState((s) => ({ ...s, deployStep: 0 }));

      const receipt = await pollReceipt(config.l2Rpc, txHash, 60, 1000);
      if (!receipt) throw new Error("L2 executor deploy: no receipt after 60s");
      if (receipt.status !== "0x1") throw new Error("L2 executor deploy: transaction reverted");
      if (!receipt.contractAddress) throw new Error("L2 executor deploy: no contractAddress in receipt");

      executorL2 = receipt.contractAddress;
      log(`L2 executor deployed at ${executorL2}`, "ok");
      setState((s) => ({ ...s, executorL2, deployStep: 1 }));
      saveToStorage(walletRef.current, { executorL2 });
    } catch (e) {
      const msg = (e as Error).message || "L2 executor deploy failed";
      setState((s) => ({ ...s, phase: "need-deploy", error: msg }));
      log(`L2 executor deploy failed: ${msg}`, "err");
      return;
    }

    // ── Step 2: Create Cross-Chain Proxy on L1 ───────────────────────────────
    setState((s) => ({ ...s, phase: "deploying-proxy", deployStep: 1 }));
    log("Step 2/3: Creating cross-chain proxy on L1...", "info");

    // Pre-compute proxy address
    const expectedProxy = await computeProxyAddress(executorL2, rollupId);

    let proxyL1: string;
    try {
      if (!config.rollupsAddress) throw new Error("Rollups contract address not configured");

      const createData =
        "0x" +
        CREATE_PROXY_SELECTOR +
        padAddress(executorL2) +
        padUint256(rollupId);

      const txHash = await sendL1Tx({
        to: config.rollupsAddress,
        data: createData,
      });
      log(`Create proxy tx: ${txHash.slice(0, 18)}...`);

      const receipt = await pollReceipt(config.l1Rpc, txHash, 30, 1000);
      if (!receipt) throw new Error("Create proxy: no receipt after 30s");
      if (receipt.status !== "0x1") throw new Error("Create proxy: transaction reverted");

      // Use pre-computed address or fall back to re-computing
      proxyL1 = expectedProxy || (await computeProxyAddress(executorL2, rollupId)) || "";
      if (!proxyL1) throw new Error("Could not determine proxy address after creation");

      log(`Cross-chain proxy created at ${proxyL1}`, "ok");
      setState((s) => ({ ...s, proxyL1, deployStep: 2 }));
      saveToStorage(walletRef.current, { proxyL1 });
    } catch (e) {
      const msg = (e as Error).message || "Create proxy failed";
      setState((s) => ({ ...s, phase: "need-deploy", error: msg }));
      log(`Create proxy failed: ${msg}`, "err");
      return;
    }

    // ── Step 3: Deploy L1 Executor ───────────────────────────────────────────
    setState((s) => ({ ...s, phase: "deploying-l1", deployStep: 2 }));
    log("Step 3/3: Deploying L1 executor contract...", "info");

    const pool = config.flashPoolAddress || "0x0000000000000000000000000000000000000000";
    const bridge = config.l1Bridge || "0x0000000000000000000000000000000000000000";
    const wrappedTokenL2 = config.flashWrappedTokenL2 || "0x0000000000000000000000000000000000000000";
    const nftL2 = config.flashNftAddress || "0x0000000000000000000000000000000000000000";
    const bridgeL2 = config.l2Bridge || "0x0000000000000000000000000000000000000000";
    const token = config.flashTokenAddress || "0x0000000000000000000000000000000000000000";
    const rollupIdNum = parseInt(rollupId, 10);

    const constructorArgs = encodeExecutorConstructorArgs(
      pool,
      bridge,
      proxyL1,
      executorL2,
      wrappedTokenL2,
      nftL2,
      bridgeL2,
      rollupIdNum,
      token,
    );

    let executorL1: string;
    try {
      const txHash = await sendL1Tx({
        data: "0x" + EXECUTOR_BYTECODE + constructorArgs,
        gas: "0x200000",
      });
      log(`L1 executor deploy tx: ${txHash.slice(0, 18)}...`);

      const receipt = await pollReceipt(config.l1Rpc, txHash, 30, 1000);
      if (!receipt) throw new Error("L1 executor deploy: no receipt after 30s");
      if (receipt.status !== "0x1") throw new Error("L1 executor deploy: transaction reverted");
      if (!receipt.contractAddress) throw new Error("L1 executor deploy: no contractAddress in receipt");

      executorL1 = receipt.contractAddress;
      log(`L1 executor deployed at ${executorL1}`, "ok");
      setState({
        phase: "ready",
        executorL2,
        proxyL1,
        executorL1,
        error: null,
        deployStep: 3,
      });
      saveToStorage(walletRef.current, { executorL1 });
    } catch (e) {
      const msg = (e as Error).message || "L1 executor deploy failed";
      setState((s) => ({ ...s, phase: "need-deploy", error: msg }));
      log(`L1 executor deploy failed: ${msg}`, "err");
    }
  }, [log, sendTx, sendL1Tx]);

  const reset = useCallback(() => {
    clearStorage(walletRef.current);
    setState({
      ...INITIAL_STATE,
      phase: "need-deploy",
    });
  }, []);

  return { state, deploy, reset };
}
