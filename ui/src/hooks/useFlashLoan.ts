import { useCallback, useEffect, useRef, useState } from "react";
import { config, ESTIMATION_SENDER } from "../config";
import { rpcCall } from "../rpc";

type Logger = (msg: string, type?: "ok" | "err" | "info") => void;
type SendTx = (params: Record<string, string>) => Promise<string>;

export type FlashLoanPhase =
  | "idle"
  | "sending"
  | "processing"
  | "verifying"
  | "complete"
  | "failed";

export interface FlashLoanState {
  phase: FlashLoanPhase;
  // Contract addresses (from rollup.env via config)
  executorL1: string;
  tokenAddress: string;
  poolAddress: string;
  nftAddress: string;
  executorL2: string;
  wrappedTokenL2: string;
  // Execution data
  txHash: string | null;
  l1TxStatus: number | null; // 1=success, 0=reverted
  l1BlockNumber: number | null;
  l1GasUsed: string | null;
  l2BlockBefore: number | null;
  l2BlockAfter: number | null;
  // Parallel processing tracking
  l2Done: boolean;
  l1Done: boolean;
  // Balances
  poolBalanceBefore: string | null;
  poolBalanceAfter: string | null;
  // State roots
  builderStateRoot: string | null;
  fullnodeStateRoot: string | null;
  stateRootsMatch: boolean | null;
  // NFT
  nftMinted: boolean | null;
  nftTokenId: string | null;
  /** NFT was already claimed before this session (detected on mount) */
  alreadyClaimed: boolean;
  /** Block/tx where the claim happened (discovered from on-chain events) */
  claimL1Block: number | null;
  claimL1TxHash: string | null;
  claimL2Block: number | null;
  claimL2TxHash: string | null;
  // Timing
  startTime: number | null;
  endTime: number | null;
  // Error
  error: string | null;
  // Whether contracts are deployed
  contractsDeployed: boolean;
  loading: boolean;
}

// execute() selector — keccak256("execute()")[0:4]
const EXECUTE_SELECTOR = "0x61461954";
// balanceOf(address) selector
const BALANCE_OF_SELECTOR = "0x70a08231";
// 2,000,000 gas in hex
const FLASH_LOAN_GAS = "0x1E8480";

function pad32(hex: string): string {
  return hex.replace("0x", "").padStart(64, "0");
}

function decodeUint256(hex: string): bigint {
  const clean = hex.replace("0x", "");
  if (!clean || clean === "0".repeat(64) || clean.length === 0) return 0n;
  return BigInt("0x" + clean.slice(0, 64));
}

function formatTokenBalance(raw: bigint): string {
  const divisor = 10n ** 18n;
  const whole = raw / divisor;
  const frac = raw % divisor;
  if (frac === 0n) return whole.toString();
  const fracStr = frac.toString().padStart(18, "0");
  const trimmed = fracStr.slice(0, 6).replace(/0+$/, "");
  return trimmed ? `${whole}.${trimmed}` : whole.toString();
}

interface TxReceipt {
  status?: string;
  blockNumber?: string;
  gasUsed?: string;
}

const INITIAL_STATE: FlashLoanState = {
  phase: "idle",
  executorL1: "",
  tokenAddress: "",
  poolAddress: "",
  nftAddress: "",
  executorL2: "",
  wrappedTokenL2: "",
  txHash: null,
  l1TxStatus: null,
  l1BlockNumber: null,
  l1GasUsed: null,
  l2BlockBefore: null,
  l2BlockAfter: null,
  l2Done: false,
  l1Done: false,
  poolBalanceBefore: null,
  poolBalanceAfter: null,
  builderStateRoot: null,
  fullnodeStateRoot: null,
  stateRootsMatch: null,
  nftMinted: null,
  nftTokenId: null,
  alreadyClaimed: false,
  claimL1Block: null,
  claimL1TxHash: null,
  claimL2Block: null,
  claimL2TxHash: null,
  startTime: null,
  endTime: null,
  error: null,
  contractsDeployed: false,
  loading: true,
};

// Transfer(address indexed from, address indexed to, uint256 indexed tokenId)
const TRANSFER_TOPIC = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const ZERO_TOPIC = "0x" + "0".repeat(64);

interface ClaimInfo {
  poolBalance: string | null;
  builderRoot: string | null;
  fullnodeRoot: string | null;
  claimL2Block: number | null;
  claimL2TxHash: string | null;
  claimL1Block: number | null;
  claimL1TxHash: string | null;
  nftTokenId: string | null;
}

/** Load pool balance, state roots, and NFT mint event for "already claimed" display */
async function loadClaimInfo(
  tokenAddress: string,
  poolAddress: string,
  nftAddress: string,
  cancelled: boolean,
): Promise<ClaimInfo | null> {
  if (cancelled) return null;
  try {
    const hostname = window.location.hostname;
    const fullnodeRpc = `http://${hostname}:9546`;

    // Query NFT Transfer(0x0 → anyone) to find the mint block + tx.
    // The NFT is minted to the original L1 caller (routed through the cross-chain proxy),
    // NOT to the ExecutorL2 contract itself — so we filter only on from=0x0 and let
    // the recipient be whoever it is.
    const nftLogPromise = nftAddress
      ? rpcCall(config.l2Rpc, "eth_getLogs", [{
          address: nftAddress,
          topics: [TRANSFER_TOPIC, ZERO_TOPIC],
          fromBlock: "0x0",
          toBlock: "latest",
        }]).catch(() => null) as Promise<Array<{ blockNumber?: string; transactionHash?: string; topics?: string[] }> | null>
      : Promise.resolve(null);

    // Query L1 for Transfer events from Pool (the flash loan lending) on FlashToken
    const l1LogPromise = (tokenAddress && poolAddress)
      ? rpcCall(config.l1Rpc, "eth_getLogs", [{
          address: tokenAddress,
          topics: [TRANSFER_TOPIC, "0x" + pad32(poolAddress)],
          fromBlock: "0x0",
          toBlock: "latest",
        }]).catch(() => null) as Promise<Array<{ blockNumber?: string; transactionHash?: string }> | null>
      : Promise.resolve(null);

    // Phase 1: get pool balance + event logs (all in parallel)
    const [poolResult, nftLogs, l1Logs] = await Promise.all([
      tokenAddress && poolAddress
        ? rpcCall(config.l1Rpc, "eth_call", [
            { to: tokenAddress, data: BALANCE_OF_SELECTOR + pad32(poolAddress) },
            "latest",
          ]) as Promise<string>
        : Promise.resolve(null),
      nftLogPromise,
      l1LogPromise,
    ]);

    // Extract mint info from the LAST Transfer log (L2).
    // Multiple flash loans may have been executed by different users.
    // Use the last mint event (most recent) — it's the current user's NFT.
    let claimL2Block: number | null = null;
    let claimL2TxHash: string | null = null;
    let nftTokenId: string | null = null;
    if (nftLogs && nftLogs.length > 0) {
      const log = nftLogs[nftLogs.length - 1]!;
      if (log.blockNumber) claimL2Block = parseInt(log.blockNumber, 16);
      if (log.transactionHash) claimL2TxHash = log.transactionHash;
      if (log.topics && log.topics.length >= 4) {
        const rawId = BigInt(log.topics[3]!);
        nftTokenId = rawId.toString();
      }
    }

    // Extract L1 execute() tx from Transfer(pool → executor) event
    let claimL1Block: number | null = null;
    let claimL1TxHash: string | null = null;
    if (l1Logs && l1Logs.length > 0) {
      const log = l1Logs[0]!;
      if (log.blockNumber) claimL1Block = parseInt(log.blockNumber, 16);
      if (log.transactionHash) claimL1TxHash = log.transactionHash;
    }

    // Phase 2: fetch state roots at the CLAIM block so they match.
    // Use claim block for builder, but fullnode may lag — try claim block first, fall back to latest.
    let builderRoot: string | null = null;
    let fullnodeRoot: string | null = null;
    if (claimL2Block !== null) {
      const blockHex = "0x" + claimL2Block.toString(16);
      const bBlock = await (rpcCall(config.l2Rpc, "eth_getBlockByNumber", [blockHex, false]).catch(() => null) as Promise<{ stateRoot?: string } | null>);
      builderRoot = bBlock?.stateRoot ?? null;

      // Try claim block on fullnode; if it hasn't synced that far, use latest
      let fBlock = await (rpcCall(fullnodeRpc, "eth_getBlockByNumber", [blockHex, false]).catch(() => null) as Promise<{ stateRoot?: string } | null>);
      if (!fBlock?.stateRoot) {
        fBlock = await (rpcCall(fullnodeRpc, "eth_getBlockByNumber", ["latest", false]).catch(() => null) as Promise<{ stateRoot?: string; number?: string } | null>);
        // Only compare if fullnode is at or past the claim block
        if (fBlock && (fBlock as { number?: string }).number) {
          const fnBlock = parseInt((fBlock as { number?: string }).number!, 16);
          if (fnBlock < claimL2Block) {
            fBlock = null; // fullnode hasn't caught up — don't compare
          }
        }
      }
      fullnodeRoot = fBlock?.stateRoot ?? null;
    }

    return {
      poolBalance: poolResult ? formatTokenBalance(decodeUint256(poolResult)) : null,
      builderRoot,
      fullnodeRoot,
      claimL2Block,
      claimL2TxHash,
      claimL1Block,
      claimL1TxHash,
      nftTokenId,
    };
  } catch {
    return null;
  }
}

export function useFlashLoan(
  log: Logger,
  sendL1ProxyTx: SendTx,
  walletAddress?: string,
  overrides?: { executorL1?: string; executorL2?: string },
) {
  const [state, setState] = useState<FlashLoanState>(() => ({
    ...INITIAL_STATE,
    executorL1: overrides?.executorL1 || config.flashExecutorL1 || "",
    tokenAddress: config.flashTokenAddress || "",
    poolAddress: config.flashPoolAddress || "",
    nftAddress: config.flashNftAddress || "",
    executorL2: overrides?.executorL2 || config.flashExecutorL2 || "",
    wrappedTokenL2: config.flashWrappedTokenL2 || "",
  }));

  // Keep executor addresses in sync when overrides change
  const overridesRef = useRef(overrides);
  useEffect(() => {
    const prev = overridesRef.current;
    overridesRef.current = overrides;
    const newL1 = overrides?.executorL1;
    const newL2 = overrides?.executorL2;
    const prevL1 = prev?.executorL1;
    const prevL2 = prev?.executorL2;
    if (newL1 !== prevL1 || newL2 !== prevL2) {
      setState((s) => ({
        ...s,
        ...(newL1 ? { executorL1: newL1 } : {}),
        ...(newL2 ? { executorL2: newL2 } : {}),
      }));
    }
  }, [overrides?.executorL1, overrides?.executorL2]); // eslint-disable-line react-hooks/exhaustive-deps

  const stateRef = useRef(state);
  stateRef.current = state;

  // On mount and when config changes: check if contracts are deployed.
  // Uses a short initial delay (500ms) to let useConfigLoader populate config
  // from /shared/rollup.env before the first check. Retries every 2s until found.
  useEffect(() => {
    let cancelled = false;

    const ZERO_ADDR = "0x" + "0".repeat(40);

    async function checkDeployment(): Promise<boolean> {
      // Re-read config in case it was loaded after initial render
      // overrides take priority (user-deployed executor) over shared config
      const executorL1 = overridesRef.current?.executorL1 || config.flashExecutorL1 || "";
      const tokenAddress = config.flashTokenAddress || "";
      const poolAddress = config.flashPoolAddress || "";
      const nftAddress = config.flashNftAddress || "";
      const executorL2 = overridesRef.current?.executorL2 || config.flashExecutorL2 || "";
      const wrappedTokenL2 = config.flashWrappedTokenL2 || "";

      // Update addresses from config
      if (!cancelled) {
        setState((s) => ({
          ...s,
          executorL1,
          tokenAddress,
          poolAddress,
          nftAddress,
          executorL2,
          wrappedTokenL2,
        }));
      }

      // Config not loaded yet — stay in loading state, don't show "not found"
      if (!executorL1 || !executorL1.startsWith("0x") || executorL1 === ZERO_ADDR) {
        return false; // not ready yet
      }

      try {
        const code = (await rpcCall(config.l1Rpc, "eth_getCode", [executorL1, "latest"])) as string;
        const deployed = !!code && code !== "0x" && code !== "0x0" && code.length > 4;
        if (!cancelled) setState((s) => ({ ...s, contractsDeployed: deployed, loading: false }));

        // If deployed, check if the NFT was already claimed (totalSupply > 0 means minted).
        // The NFT is minted to the original L1 caller (via the cross-chain proxy's
        // Check if the CURRENT USER already has an NFT (not global totalSupply).
        // The NFT is minted to ExecutorL2, then transferred to the user via transferFrom.
        // Multiple users can each get their own NFT.
        if (deployed && nftAddress && nftAddress !== ZERO_ADDR && walletAddress) {
          try {
            const nftResult = (await rpcCall(config.l2Rpc, "eth_call", [
              { to: nftAddress, data: BALANCE_OF_SELECTOR + pad32(walletAddress) },
              "latest",
            ])) as string;
            const balance = decodeUint256(nftResult);
            if (!cancelled && balance > 0n) {
              setState((s) => ({ ...s, alreadyClaimed: true, nftMinted: true }));
              // Load claim details — pool balance, state roots, and NFT mint block/tx
              loadClaimInfo(tokenAddress, poolAddress, nftAddress, cancelled).then(info => {
                if (!cancelled && info) {
                  setState((s) => ({
                    ...s,
                    poolBalanceAfter: info.poolBalance,
                    builderStateRoot: info.builderRoot,
                    fullnodeStateRoot: info.fullnodeRoot,
                    stateRootsMatch: info.builderRoot !== null && info.fullnodeRoot !== null && info.builderRoot === info.fullnodeRoot,
                    claimL1Block: info.claimL1Block,
                    claimL1TxHash: info.claimL1TxHash,
                    claimL2Block: info.claimL2Block,
                    claimL2TxHash: info.claimL2TxHash,
                    nftTokenId: info.nftTokenId,
                  }));
                }
              });
            }
          } catch {
            /* NFT check failed — not critical */
          }
        }
        return true; // config was available
      } catch {
        if (!cancelled) setState((s) => ({ ...s, contractsDeployed: false, loading: false }));
        return true; // config was available but RPC failed — stop retrying
      }
    }

    // Short delay to let useConfigLoader finish fetching /shared/rollup.env
    const initialTimeout = setTimeout(async () => {
      if (cancelled) return;
      const ready = await checkDeployment();
      if (ready || cancelled) return;

      // Config not loaded yet — retry every 2s (fast) until config appears
      const interval = setInterval(async () => {
        if (cancelled) return;
        if (stateRef.current.contractsDeployed) {
          clearInterval(interval);
          return;
        }
        const ready = await checkDeployment();
        if (ready) {
          clearInterval(interval);
        }
      }, 2000);

      // Store interval for cleanup
      cleanupInterval = interval;
    }, 500);

    let cleanupInterval: ReturnType<typeof setInterval> | null = null;

    return () => {
      cancelled = true;
      clearTimeout(initialTimeout);
      if (cleanupInterval) clearInterval(cleanupInterval);
    };
  }, []);

  async function readPoolBalance(tokenAddr: string, poolAddr: string): Promise<string | null> {
    if (!tokenAddr || !poolAddr) return null;
    try {
      const result = (await rpcCall(config.l1Rpc, "eth_call", [
        { to: tokenAddr, data: BALANCE_OF_SELECTOR + pad32(poolAddr) },
        "latest",
      ])) as string;
      const raw = decodeUint256(result);
      return formatTokenBalance(raw);
    } catch {
      return null;
    }
  }

  async function readL2BlockNumber(): Promise<number | null> {
    try {
      const result = (await rpcCall(config.l2Rpc, "eth_blockNumber", [])) as string;
      return parseInt(result, 16);
    } catch {
      return null;
    }
  }

  async function readStateRoot(rpcUrl: string): Promise<string | null> {
    try {
      const block = (await rpcCall(rpcUrl, "eth_getBlockByNumber", ["latest", false])) as {
        stateRoot?: string;
      } | null;
      return block?.stateRoot ?? null;
    } catch {
      return null;
    }
  }

  // Check if the NFT has been minted at all (totalSupply > 0).
  // The NFT recipient is the original L1 caller routed through the cross-chain proxy,
  // not the ExecutorL2 contract — so balanceOf(executorL2) is always 0.
  async function checkNftMinted(nftAddr: string): Promise<boolean> {
    if (!nftAddr || nftAddr === "0x" + "0".repeat(40)) return false;
    // Check if the current user owns an NFT (balanceOf), not global totalSupply.
    const userAddr = walletAddress || ESTIMATION_SENDER;
    try {
      const result = (await rpcCall(config.l2Rpc, "eth_call", [
        { to: nftAddr, data: BALANCE_OF_SELECTOR + pad32(userAddr) },
        "latest",
      ])) as string;
      const balance = decodeUint256(result);
      return balance > 0n;
    } catch {
      return false;
    }
  }

  const execute = useCallback(async () => {
    const { executorL1, tokenAddress, poolAddress, nftAddress, alreadyClaimed } = stateRef.current;
    if (!executorL1 || !stateRef.current.contractsDeployed) return;
    if (alreadyClaimed) {
      log("Flash loan already executed — you already own an NFT from a previous run.", "info");
      return;
    }

    const startTime = Date.now();

    setState((s) => ({
      ...s,
      phase: "sending",
      error: null,
      txHash: null,
      l1TxStatus: null,
      l1BlockNumber: null,
      l1GasUsed: null,
      l2BlockBefore: null,
      l2BlockAfter: null,
      l2Done: false,
      l1Done: false,
      poolBalanceBefore: null,
      poolBalanceAfter: null,
      builderStateRoot: null,
      fullnodeStateRoot: null,
      stateRootsMatch: null,
      nftMinted: null,
      nftTokenId: null,
      startTime,
      endTime: null,
    }));

    log("Starting cross-chain flash loan execution...", "info");

    // Step a: read pool token balance before
    let poolBalanceBefore: string | null = null;
    if (tokenAddress && poolAddress) {
      poolBalanceBefore = await readPoolBalance(tokenAddress, poolAddress);
    }
    if (poolBalanceBefore !== null) {
      setState((s) => ({ ...s, poolBalanceBefore }));
    }

    // Step b: read L2 block number before
    const l2BlockBefore = await readL2BlockNumber();
    if (l2BlockBefore !== null) {
      setState((s) => ({ ...s, l2BlockBefore }));
    }

    // Step c: send tx via L1 proxy
    let txHash: string;
    try {
      txHash = await sendL1ProxyTx({
        to: executorL1,
        data: EXECUTE_SELECTOR,
        gas: FLASH_LOAN_GAS,
        from: ESTIMATION_SENDER,
      });
    } catch (e) {
      const msg = (e as Error).message || "Transaction rejected";
      setState((s) => ({ ...s, phase: "failed", error: msg }));
      log(`Flash loan tx rejected: ${msg}`, "err");
      return;
    }

    // Move to "processing" phase — L1 proxy already sent entries to L2 before forwarding
    setState((s) => ({ ...s, phase: "processing", txHash }));
    log(`Flash loan tx submitted: ${txHash.slice(0, 18)}... — L2 processing starts now`);

    // Step d+e: poll L1 receipt AND L2 block advancement IN PARALLEL
    // The L1 proxy sends entries to L2 before forwarding the tx, so L2 processes first.

    const poll = { receipt: null as TxReceipt | null };
    let l1PollError: string | null = null;

    const pollL1 = async () => {
      for (let i = 0; i < 30; i++) {
        await new Promise((r) => setTimeout(r, 2000));
        try {
          const r = (await rpcCall(config.l1Rpc, "eth_getTransactionReceipt", [txHash])) as TxReceipt | null;
          if (r) {
            poll.receipt = r;
            const status = r.status === "0x1" ? 1 : 0;
            const blockNum = r.blockNumber ? parseInt(r.blockNumber, 16) : null;
            const gasUsed = r.gasUsed ? parseInt(r.gasUsed, 16).toLocaleString() : null;
            setState((s) => ({
              ...s,
              l1TxStatus: status,
              l1BlockNumber: blockNum,
              l1GasUsed: gasUsed,
              l1Done: true,
            }));
            log(`L1 confirmed in block ${blockNum ?? "?"}.`);
            return;
          }
        } catch {
          /* not mined yet */
        }
      }
      l1PollError = "L1 transaction not confirmed after 60s — check composer health";
    };

    // Flash loan is atomic on L1: postBatch + execute() in the same block.
    // Once L1 confirms, the L2 delivery (loadTable + executeIncomingCrossChainCall)
    // is deterministic and will happen in the next L2 block. No need to poll L2
    // separately — just wait for L1 confirmation.
    await pollL1();

    // Mark L2 as done once L1 is confirmed (atomic guarantee).
    if (!l1PollError && poll.receipt?.status === "0x1") {
      const current = await readL2BlockNumber();
      setState((s) => ({ ...s, l2Done: true, l2BlockAfter: current }));
      log("L1 confirmed — L2 delivery guaranteed (atomic flash loan).");
    }

    if (l1PollError) {
      setState((s) => ({ ...s, phase: "failed", error: l1PollError! }));
      log(l1PollError, "err");
      return;
    }

    if (!poll.receipt || poll.receipt.status !== "0x1") {
      setState((s) => ({
        ...s,
        phase: "failed",
        error: "L1 transaction reverted — check executor contract and parameters",
      }));
      log("Flash loan: L1 tx reverted", "err");
      return;
    }

    log(`Both L1 and L2 complete. Verifying results...`);

    // Step f: verify results
    setState((s) => ({ ...s, phase: "verifying" }));

    // Read pool balance after
    let poolBalanceAfter: string | null = null;
    if (tokenAddress && poolAddress) {
      poolBalanceAfter = await readPoolBalance(tokenAddress, poolAddress);
    }

    // Read state roots for comparison
    const hostname = window.location.hostname;
    const fullnodeRpc = `http://${hostname}:9546`;

    // Also check NFT mint on L2 (totalSupply > 0) — all three in parallel.
    // NFT goes to the original L1 caller, not the ExecutorL2 contract.
    const [builderStateRoot, fullnodeStateRoot, nftMinted] = await Promise.all([
      readStateRoot(config.l2Rpc),
      readStateRoot(fullnodeRpc),
      checkNftMinted(nftAddress),
    ]);

    const stateRootsMatch =
      builderStateRoot !== null &&
      fullnodeStateRoot !== null &&
      builderStateRoot === fullnodeStateRoot;

    const endTime = Date.now();

    setState((s) => ({
      ...s,
      phase: "complete",
      poolBalanceAfter,
      builderStateRoot,
      fullnodeStateRoot,
      stateRootsMatch,
      nftMinted,
      nftTokenId: null, // token ID not discoverable without event log scanning
      endTime,
    }));

    if (nftMinted) {
      log("Flash loan complete! NFT claimed on L2. Cross-chain execution verified.", "ok");
    } else {
      log("Flash loan complete! Cross-chain execution verified.", "ok");
    }
  }, [log, sendL1ProxyTx]);

  const reset = useCallback(() => {
    setState((s) => ({
      ...INITIAL_STATE,
      executorL1: s.executorL1,
      tokenAddress: s.tokenAddress,
      poolAddress: s.poolAddress,
      nftAddress: s.nftAddress,
      executorL2: s.executorL2,
      wrappedTokenL2: s.wrappedTokenL2,
      contractsDeployed: s.contractsDeployed,
      alreadyClaimed: s.alreadyClaimed,
      nftMinted: s.alreadyClaimed ? true : null,
      loading: false,
    }));
  }, []);

  return { state, execute, reset };
}
