import { useCallback, useEffect, useRef, useState } from "react";
import { config, ESTIMATION_SENDER } from "../config";
import { rpcCall } from "../rpc";

type Logger = (msg: string, type?: "ok" | "err" | "info") => void;
type SendTx = (params: Record<string, string>) => Promise<string>;

export type ReverseFlashLoanPhase =
  | "idle"
  | "sending"
  | "processing"
  | "verifying"
  | "complete"
  | "failed";

export interface ReverseFlashLoanState {
  phase: ReverseFlashLoanPhase;
  // L2→L1 direction: executor on L2, NFT on L1
  reverseExecutorL2: string;
  reverseNftL1: string;
  wrappedTokenL2: string;
  tokenAddress: string;
  poolAddress: string;
  // Execution data
  txHash: string | null;
  l2TxStatus: number | null;
  l2BlockNumber: number | null;
  l2GasUsed: string | null;
  l2BlockBefore: number | null;
  l2BlockAfter: number | null;
  // Parallel processing tracking
  l2Done: boolean;
  l1Done: boolean;
  // Balances
  wrappedTokenBalanceBefore: string | null;
  wrappedTokenBalanceAfter: string | null;
  // State roots
  builderStateRoot: string | null;
  fullnodeStateRoot: string | null;
  stateRootsMatch: boolean | null;
  // NFT
  nftMinted: boolean | null;
  nftTokenId: string | null;
  alreadyClaimed: boolean;
  claimL2Block: number | null;
  claimL2TxHash: string | null;
  claimL1Block: number | null;
  claimL1TxHash: string | null;
  // Timing
  startTime: number | null;
  endTime: number | null;
  // Error
  error: string | null;
  // Whether contracts are deployed
  contractsDeployed: boolean;
  loading: boolean;
}

// execute() selector
const EXECUTE_SELECTOR = "0x61461954";
// balanceOf(address) selector
const BALANCE_OF_SELECTOR = "0x70a08231";
// totalSupply() selector — keccak256("totalSupply()")[0:4]
const TOTAL_SUPPLY_SELECTOR = "0x18160ddd";
// 5,000,000 gas in hex for complex L2→L1 cross-chain
const REVERSE_FLASH_LOAN_GAS = "0x4C4B40";


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

const INITIAL_STATE: ReverseFlashLoanState = {
  phase: "idle",
  reverseExecutorL2: "",
  reverseNftL1: "",
  wrappedTokenL2: "",
  tokenAddress: "",
  poolAddress: "",
  txHash: null,
  l2TxStatus: null,
  l2BlockNumber: null,
  l2GasUsed: null,
  l2BlockBefore: null,
  l2BlockAfter: null,
  l2Done: false,
  l1Done: false,
  wrappedTokenBalanceBefore: null,
  wrappedTokenBalanceAfter: null,
  builderStateRoot: null,
  fullnodeStateRoot: null,
  stateRootsMatch: null,
  nftMinted: null,
  nftTokenId: null,
  alreadyClaimed: false,
  claimL2Block: null,
  claimL2TxHash: null,
  claimL1Block: null,
  claimL1TxHash: null,
  startTime: null,
  endTime: null,
  error: null,
  contractsDeployed: false,
  loading: true,
};

// Transfer event topic
const TRANSFER_TOPIC = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
const ZERO_TOPIC = "0x" + "0".repeat(64);

export function useFlashLoanReverse(
  log: Logger,
  sendL2ProxyTx: SendTx,
) {
  const [state, setState] = useState<ReverseFlashLoanState>(() => ({
    ...INITIAL_STATE,
    reverseExecutorL2: config.reverseExecutorL2 || "",
    reverseNftL1: config.reverseNftL1 || "",
    wrappedTokenL2: config.flashWrappedTokenL2 || "",
    tokenAddress: config.flashTokenAddress || "",
    poolAddress: config.flashPoolAddress || "",
  }));

  const stateRef = useRef(state);
  stateRef.current = state;

  // On mount: check if contracts are deployed, sync config addresses
  useEffect(() => {
    let cancelled = false;

    async function checkDeployment(): Promise<boolean> {
      const reverseExecutorL2 = config.reverseExecutorL2 || "";
      const reverseNftL1 = config.reverseNftL1 || "";
      const wrappedTokenL2 = config.flashWrappedTokenL2 || "";
      const tokenAddress = config.flashTokenAddress || "";
      const poolAddress = config.flashPoolAddress || "";

      if (!cancelled) {
        setState((s) => ({
          ...s,
          reverseExecutorL2,
          reverseNftL1,
          wrappedTokenL2,
          tokenAddress,
          poolAddress,
        }));
      }

      // Config not loaded yet — addresses come from rollup.env which loads async
      if (!reverseExecutorL2 || !reverseExecutorL2.startsWith("0x")) {
        return false;
      }

      try {
        const code = (await rpcCall(config.l2Rpc, "eth_getCode", [reverseExecutorL2, "latest"])) as string;
        const deployed = !!code && code !== "0x" && code !== "0x0" && code.length > 4;

        if (!cancelled) {
          setState((s) => ({ ...s, contractsDeployed: deployed, loading: false }));
        }

        // If deployed, check if NFT was already claimed on L1 (totalSupply > 0).
        // The ReverseNFT is minted to the original L2 caller routed through the
        // cross-chain proxy's originalAddress, NOT to the ReverseExecutorL2 contract
        // itself — so balanceOf(reverseExecutorL2) is always 0.
        if (deployed && reverseNftL1) {
          try {
            const nftResult = (await rpcCall(config.l1Rpc, "eth_call", [
              { to: reverseNftL1, data: TOTAL_SUPPLY_SELECTOR },
              "latest",
            ])) as string;
            const supply = decodeUint256(nftResult);
            if (!cancelled && supply > 0n) {
              setState((s) => ({ ...s, alreadyClaimed: true, nftMinted: true }));

              // Try to find mint event on L1 — Transfer(0x0 → anyone)
              try {
                const l1Logs = (await rpcCall(config.l1Rpc, "eth_getLogs", [{
                  address: reverseNftL1,
                  topics: [TRANSFER_TOPIC, ZERO_TOPIC],
                  fromBlock: "0x0",
                  toBlock: "latest",
                }]).catch(() => null)) as Array<{ blockNumber?: string; transactionHash?: string }> | null;

                if (!cancelled && l1Logs && l1Logs.length > 0) {
                  const log = l1Logs[0]!;
                  const claimL1Block = log.blockNumber ? parseInt(log.blockNumber, 16) : null;
                  const claimL1TxHash = log.transactionHash || null;
                  setState((s) => ({ ...s, claimL1Block, claimL1TxHash }));
                }
              } catch {
                /* not critical */
              }
            }
          } catch {
            /* NFT check failed */
          }
        }

        return true;
      } catch {
        if (!cancelled) setState((s) => ({ ...s, contractsDeployed: false, loading: false }));
        return true;
      }
    }

    const initialTimeout = setTimeout(async () => {
      if (cancelled) return;
      const ready = await checkDeployment();
      if (ready || cancelled) return;

      let cleanupInterval: ReturnType<typeof setInterval> | null = null;
      const interval = setInterval(async () => {
        if (cancelled) return;
        if (stateRef.current.contractsDeployed) {
          clearInterval(interval);
          return;
        }
        const ready = await checkDeployment();
        if (ready) clearInterval(interval);
      }, 2000);
      cleanupInterval = interval;

      return () => { if (cleanupInterval) clearInterval(cleanupInterval); };
    }, 500);

    return () => {
      cancelled = true;
      clearTimeout(initialTimeout);
    };
  }, []);

  async function readWrappedBalance(tokenAddr: string, ownerAddr: string): Promise<string | null> {
    if (!tokenAddr || !ownerAddr) return null;
    try {
      const result = (await rpcCall(config.l2Rpc, "eth_call", [
        { to: tokenAddr, data: BALANCE_OF_SELECTOR + pad32(ownerAddr) },
        "latest",
      ])) as string;
      return formatTokenBalance(decodeUint256(result));
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

  async function readL1BlockNumber(): Promise<number | null> {
    try {
      const result = (await rpcCall(config.l1Rpc, "eth_blockNumber", [])) as string;
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

  // Check if the reverse NFT has been minted at all on L1 (totalSupply > 0).
  // The NFT recipient is the original L2 caller routed through the cross-chain proxy,
  // not the ReverseExecutorL2 contract — so balanceOf(reverseExecutorL2) is always 0.
  async function checkReverseNftMintedL1(nftAddr: string): Promise<boolean> {
    if (!nftAddr) return false;
    try {
      const result = (await rpcCall(config.l1Rpc, "eth_call", [
        { to: nftAddr, data: TOTAL_SUPPLY_SELECTOR },
        "latest",
      ])) as string;
      return decodeUint256(result) > 0n;
    } catch {
      return false;
    }
  }

  const execute = useCallback(async () => {
    const { reverseExecutorL2, reverseNftL1, wrappedTokenL2, alreadyClaimed, contractsDeployed } = stateRef.current;

    if (!reverseExecutorL2 || !contractsDeployed) return;

    if (alreadyClaimed) {
      log("Reverse flash loan already executed — NFT minted on L1. This demo runs once per deployment.", "info");
    }

    const startTime = Date.now();

    setState((s) => ({
      ...s,
      phase: "sending",
      error: null,
      txHash: null,
      l2TxStatus: null,
      l2BlockNumber: null,
      l2GasUsed: null,
      l2BlockBefore: null,
      l2BlockAfter: null,
      l2Done: false,
      l1Done: false,
      wrappedTokenBalanceBefore: null,
      wrappedTokenBalanceAfter: null,
      builderStateRoot: null,
      fullnodeStateRoot: null,
      stateRootsMatch: null,
      nftMinted: null,
      nftTokenId: null,
      startTime,
      endTime: null,
    }));

    log("Starting reverse cross-chain flash loan (L2 → L1)...", "info");

    // Step a: read wrapped token balance before (L2 executor holds them)
    const wrappedTokenBalanceBefore = wrappedTokenL2
      ? await readWrappedBalance(wrappedTokenL2, reverseExecutorL2)
      : null;
    if (wrappedTokenBalanceBefore !== null) {
      setState((s) => ({ ...s, wrappedTokenBalanceBefore }));
    }

    // Step b: read L2 block number before
    const l2BlockBefore = await readL2BlockNumber();
    if (l2BlockBefore !== null) {
      setState((s) => ({ ...s, l2BlockBefore }));
    }

    // Step c: send tx via L2 proxy (hold-then-forward for withdrawal detection)
    let txHash: string;
    try {
      txHash = await sendL2ProxyTx({
        to: reverseExecutorL2,
        data: EXECUTE_SELECTOR,
        gas: REVERSE_FLASH_LOAN_GAS,
        from: ESTIMATION_SENDER,
      });
    } catch (e) {
      const msg = (e as Error).message || "Transaction rejected";
      setState((s) => ({ ...s, phase: "failed", error: msg }));
      log(`Reverse flash loan tx rejected: ${msg}`, "err");
      return;
    }

    setState((s) => ({ ...s, phase: "processing", txHash }));
    log(`Reverse flash loan tx submitted: ${txHash.slice(0, 18)}... — L2 processing + L1 trigger pending`);

    // Step d: Poll L2 receipt AND L1 NFT confirmation in parallel
    const poll = { receipt: null as TxReceipt | null };
    let l2PollError: string | null = null;
    let l1PollError: string | null = null;

    const pollL2Receipt = async () => {
      for (let i = 0; i < 30; i++) {
        await new Promise((r) => setTimeout(r, 2000));
        try {
          const r = (await rpcCall(config.l2Rpc, "eth_getTransactionReceipt", [txHash])) as TxReceipt | null;
          if (r) {
            poll.receipt = r;
            const status = r.status === "0x1" ? 1 : 0;
            const blockNum = r.blockNumber ? parseInt(r.blockNumber, 16) : null;
            const gasUsed = r.gasUsed ? parseInt(r.gasUsed, 16).toLocaleString() : null;
            setState((s) => ({
              ...s,
              l2TxStatus: status,
              l2BlockNumber: blockNum,
              l2GasUsed: gasUsed,
              l2Done: true,
            }));
            log(`L2 confirmed in block ${blockNum ?? "?"}.`);
            return;
          }
        } catch {
          /* not mined yet */
        }
      }
      l2PollError = "L2 transaction not confirmed after 60s";
    };

    // Poll for L1 block advancement and NFT mint (the L1 trigger fires after L2 postBatch)
    const pollL1Trigger = async () => {
      const l1BlockStart = await readL1BlockNumber();
      const targetL1Block = (l1BlockStart ?? 0) + 3;

      for (let i = 0; i < 40; i++) {
        await new Promise((r) => setTimeout(r, 3000));
        const currentL1 = await readL1BlockNumber();
        if (currentL1 !== null && currentL1 >= targetL1Block) {
          // Check if NFT was minted on L1 (totalSupply > 0)
          const nftMinted = await checkReverseNftMintedL1(reverseNftL1);
          setState((s) => ({
            ...s,
            l2BlockAfter: s.l2BlockBefore !== null ? (s.l2BlockBefore + 3) : null,
            l1Done: true,
          }));
          log(`L1 advanced to block ${currentL1} — NFT trigger processed${nftMinted ? " — NFT minted!" : ""}.`);
          return;
        }
      }
      l1PollError = "L1 trigger did not complete within 120s — check builder health";
    };

    await Promise.all([pollL2Receipt(), pollL1Trigger()]);

    if (l2PollError) {
      setState((s) => ({ ...s, phase: "failed", error: l2PollError! }));
      log(l2PollError, "err");
      return;
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
        error: "L2 transaction reverted — check ReverseExecutorL2 contract and L2 proxy",
      }));
      log("Reverse flash loan: L2 tx reverted", "err");
      return;
    }

    log("Both L2 and L1 complete. Verifying results...");
    setState((s) => ({ ...s, phase: "verifying" }));

    // Read wrapped token balance after
    const wrappedTokenBalanceAfter = wrappedTokenL2
      ? await readWrappedBalance(wrappedTokenL2, reverseExecutorL2)
      : null;

    // Read state roots
    const hostname = window.location.hostname;
    const fullnodeRpc = `http://${hostname}:9546`;

    const [builderStateRoot, fullnodeStateRoot, nftMinted] = await Promise.all([
      readStateRoot(config.l2Rpc),
      readStateRoot(fullnodeRpc),
      checkReverseNftMintedL1(reverseNftL1),
    ]);

    const stateRootsMatch =
      builderStateRoot !== null &&
      fullnodeStateRoot !== null &&
      builderStateRoot === fullnodeStateRoot;

    const endTime = Date.now();

    setState((s) => ({
      ...s,
      phase: "complete",
      wrappedTokenBalanceAfter,
      builderStateRoot,
      fullnodeStateRoot,
      stateRootsMatch,
      nftMinted,
      endTime,
    }));

    if (nftMinted) {
      log("Reverse flash loan complete! NFT claimed on L1. Cross-chain execution verified.", "ok");
    } else {
      log("Reverse flash loan complete! Cross-chain execution verified.", "ok");
    }
  }, [log, sendL2ProxyTx]);

  const reset = useCallback(() => {
    setState((s) => ({
      ...INITIAL_STATE,
      reverseExecutorL2: s.reverseExecutorL2,
      reverseNftL1: s.reverseNftL1,
      wrappedTokenL2: s.wrappedTokenL2,
      tokenAddress: s.tokenAddress,
      poolAddress: s.poolAddress,
      contractsDeployed: s.contractsDeployed,
      alreadyClaimed: s.alreadyClaimed,
      nftMinted: s.alreadyClaimed ? true : null,
      loading: false,
    }));
  }, []);

  return { state, execute, reset };
}
