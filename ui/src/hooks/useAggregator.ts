import { useCallback, useEffect, useRef, useState } from "react";
import { config } from "../config";
import { rpcCall } from "../rpc";
import type { Address } from "viem";
import {
  encodeDeposit,
  encodeWithdraw,
  encodeAggregatedSwap,
  encodeApprove,
  encodeBalanceOf,
  encodeAllowance,
  encodeReserveA,
  encodeReserveB,
  encodeGetAmountOut,
  decodeUint256Result,
  formatTokenAmount,
  parseTokenAmount,
  MAX_UINT256,
  WRAP_GAS,
  APPROVE_GAS,
  SWAP_GAS,
} from "../lib/aggregatorAbi";

type Logger = (msg: string, type?: "ok" | "err" | "info") => void;
type SendTx = (params: Record<string, string>) => Promise<string>;

export type AggregatorPhase =
  | "idle"
  | "wrapping"
  | "approving"
  | "sending"
  | "processing"
  | "verifying"
  | "complete"
  | "failed";

export interface AggregatorState {
  phase: AggregatorPhase;
  // Pool state (polled every 3s)
  l1ReserveA: string | null;
  l1ReserveB: string | null;
  l2ReserveA: string | null;
  l2ReserveB: string | null;
  l1Quote: string | null;
  l2Quote: string | null;
  singlePoolQuote: string | null;
  // User balances
  ethBalance: string | null;
  wethBalance: string | null;
  usdcBalance: string | null;
  // Input
  totalAmount: string;
  splitPercent: number;
  // Execution
  txHash: string | null;
  l1TxStatus: number | null;
  l1BlockNumber: number | null;
  l1GasUsed: string | null;
  l2BlockBefore: number | null;
  l2BlockAfter: number | null;
  l2BlockNumber: number | null;
  l2TxHashes: string[];
  l1Done: boolean;
  l2Done: boolean;
  // Results
  localOutput: string | null;
  remoteOutput: string | null;
  totalOutput: string | null;
  improvement: string | null;
  usdcBalanceBefore: string | null;
  usdcBalanceAfter: string | null;
  // Viz
  vizPhase: number;
  startTime: number | null;
  endTime: number | null;
  error: string | null;
  contractsDeployed: boolean;
  loading: boolean;
}

interface TxReceipt {
  status?: string;
  blockNumber?: string;
  gasUsed?: string;
}

const WETH_DECIMALS = 18;
const USDC_DECIMALS = 6;

const INITIAL_STATE: AggregatorState = {
  phase: "idle",
  l1ReserveA: null,
  l1ReserveB: null,
  l2ReserveA: null,
  l2ReserveB: null,
  l1Quote: null,
  l2Quote: null,
  singlePoolQuote: null,
  ethBalance: null,
  wethBalance: null,
  usdcBalance: null,
  totalAmount: "1",
  splitPercent: 50,
  txHash: null,
  l1TxStatus: null,
  l1BlockNumber: null,
  l1GasUsed: null,
  l2BlockBefore: null,
  l2BlockAfter: null,
  l2BlockNumber: null,
  l2TxHashes: [],
  l1Done: false,
  l2Done: false,
  localOutput: null,
  remoteOutput: null,
  totalOutput: null,
  improvement: null,
  usdcBalanceBefore: null,
  usdcBalanceAfter: null,
  vizPhase: 0,
  startTime: null,
  endTime: null,
  error: null,
  contractsDeployed: false,
  loading: true,
};

async function pollReceipt(
  rpcUrl: string,
  txHash: string,
  maxAttempts: number,
  intervalMs: number,
): Promise<TxReceipt | null> {
  for (let i = 0; i < maxAttempts; i++) {
    await new Promise((r) => setTimeout(r, intervalMs));
    try {
      const r = (await rpcCall(rpcUrl, "eth_getTransactionReceipt", [txHash])) as TxReceipt | null;
      if (r) return r;
    } catch {
      /* not mined yet */
    }
  }
  return null;
}

export function useAggregator(
  log: Logger,
  sendL1Tx: SendTx,
  sendL1ProxyTx: SendTx,
  walletAddress: string | null,
) {
  const [state, setState] = useState<AggregatorState>({ ...INITIAL_STATE });
  const stateRef = useRef(state);
  stateRef.current = state;

  // Refs for viz timer cleanup
  const vizTimersRef = useRef<ReturnType<typeof setTimeout>[]>([]);

  // ── Contract deployment check ──
  useEffect(() => {
    let cancelled = false;
    const ZERO_ADDR = "0x" + "0".repeat(40);

    async function checkDeployment(): Promise<boolean> {
      const aggregator = config.aggAggregator || "";
      if (!aggregator || !aggregator.startsWith("0x") || aggregator === ZERO_ADDR) {
        return false;
      }
      try {
        const code = (await rpcCall(config.l1Rpc, "eth_getCode", [aggregator, "latest"])) as string;
        const deployed = !!code && code !== "0x" && code !== "0x0" && code.length > 4;
        if (!cancelled) setState((s) => ({ ...s, contractsDeployed: deployed, loading: false }));
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
    }, 500);

    let cleanupInterval: ReturnType<typeof setInterval> | null = null;

    return () => {
      cancelled = true;
      clearTimeout(initialTimeout);
      if (cleanupInterval) clearInterval(cleanupInterval);
    };
  }, []);

  // ── Pool reserves + user balance polling (every 3s) ──
  useEffect(() => {
    if (!state.contractsDeployed) return;
    let cancelled = false;

    async function pollData() {
      if (cancelled) return;

      const l1Amm = config.aggL1Amm;
      const l2Amm = config.aggL2Amm;
      const weth = config.aggWeth as Address | "";
      const wrappedWeth = config.aggWrappedWethL2 as Address | "";
      const usdc = config.aggUsdc;

      const s = stateRef.current;
      const totalWei = parseTokenAmount(s.totalAmount || "0", WETH_DECIMALS);
      const localWei = (totalWei * BigInt(s.splitPercent)) / 100n;
      const remoteWei = totalWei - localWei;

      try {
        // Build parallel RPC calls
        const calls: Array<Promise<unknown>> = [];

        // 0: L1 reserveA
        calls.push(
          l1Amm
            ? rpcCall(config.l1Rpc, "eth_call", [{ to: l1Amm, data: encodeReserveA() }, "latest"])
            : Promise.resolve(null),
        );
        // 1: L1 reserveB
        calls.push(
          l1Amm
            ? rpcCall(config.l1Rpc, "eth_call", [{ to: l1Amm, data: encodeReserveB() }, "latest"])
            : Promise.resolve(null),
        );
        // 2: L2 reserveA
        calls.push(
          l2Amm
            ? rpcCall(config.l2Rpc, "eth_call", [{ to: l2Amm, data: encodeReserveA() }, "latest"])
            : Promise.resolve(null),
        );
        // 3: L2 reserveB
        calls.push(
          l2Amm
            ? rpcCall(config.l2Rpc, "eth_call", [{ to: l2Amm, data: encodeReserveB() }, "latest"])
            : Promise.resolve(null),
        );
        // 4: L1 quote (local amount)
        calls.push(
          l1Amm && weth && localWei > 0n
            ? rpcCall(config.l1Rpc, "eth_call", [
                { to: l1Amm, data: encodeGetAmountOut(weth as Address, localWei) },
                "latest",
              ])
            : Promise.resolve(null),
        );
        // 5: L2 quote (remote amount, using wrapped WETH)
        calls.push(
          l2Amm && wrappedWeth && remoteWei > 0n
            ? rpcCall(config.l2Rpc, "eth_call", [
                { to: l2Amm, data: encodeGetAmountOut(wrappedWeth as Address, remoteWei) },
                "latest",
              ])
            : Promise.resolve(null),
        );
        // 6: single pool quote (total on L1 only)
        calls.push(
          l1Amm && weth && totalWei > 0n
            ? rpcCall(config.l1Rpc, "eth_call", [
                { to: l1Amm, data: encodeGetAmountOut(weth as Address, totalWei) },
                "latest",
              ])
            : Promise.resolve(null),
        );
        // 7: ETH balance
        calls.push(
          walletAddress
            ? rpcCall(config.l1Rpc, "eth_getBalance", [walletAddress, "latest"])
            : Promise.resolve(null),
        );
        // 8: WETH balance
        calls.push(
          walletAddress && weth
            ? rpcCall(config.l1Rpc, "eth_call", [
                { to: weth, data: encodeBalanceOf(walletAddress as Address) },
                "latest",
              ])
            : Promise.resolve(null),
        );
        // 9: USDC balance
        calls.push(
          walletAddress && usdc
            ? rpcCall(config.l1Rpc, "eth_call", [
                { to: usdc, data: encodeBalanceOf(walletAddress as Address) },
                "latest",
              ])
            : Promise.resolve(null),
        );

        const results = await Promise.all(calls);

        if (cancelled) return;

        const l1ResA = results[0] ? decodeUint256Result(results[0] as string) : null;
        const l1ResB = results[1] ? decodeUint256Result(results[1] as string) : null;
        const l2ResA = results[2] ? decodeUint256Result(results[2] as string) : null;
        const l2ResB = results[3] ? decodeUint256Result(results[3] as string) : null;
        const l1QuoteRaw = results[4] ? decodeUint256Result(results[4] as string) : null;
        const l2QuoteRaw = results[5] ? decodeUint256Result(results[5] as string) : null;
        const singleQuoteRaw = results[6] ? decodeUint256Result(results[6] as string) : null;
        const ethRaw = results[7] ? BigInt(results[7] as string) : null;
        const wethRaw = results[8] ? decodeUint256Result(results[8] as string) : null;
        const usdcRaw = results[9] ? decodeUint256Result(results[9] as string) : null;

        // Quote handling at extremes — when local or remote amount is 0
        // (slider at 0%/100% or empty input), we MUST set the corresponding
        // quote to "0" rather than leaving the stale previous value behind.
        const l1QuoteFinal =
          l1QuoteRaw !== null
            ? formatTokenAmount(l1QuoteRaw, USDC_DECIMALS)
            : localWei === 0n
              ? "0"
              : null;
        const l2QuoteFinal =
          l2QuoteRaw !== null
            ? formatTokenAmount(l2QuoteRaw, USDC_DECIMALS)
            : remoteWei === 0n
              ? "0"
              : null;
        const singleFinal =
          singleQuoteRaw !== null
            ? formatTokenAmount(singleQuoteRaw, USDC_DECIMALS)
            : totalWei === 0n
              ? "0"
              : null;

        setState((prev) => ({
          ...prev,
          l1ReserveA: l1ResA !== null ? formatTokenAmount(l1ResA, WETH_DECIMALS) : prev.l1ReserveA,
          l1ReserveB: l1ResB !== null ? formatTokenAmount(l1ResB, USDC_DECIMALS) : prev.l1ReserveB,
          l2ReserveA: l2ResA !== null ? formatTokenAmount(l2ResA, WETH_DECIMALS) : prev.l2ReserveA,
          l2ReserveB: l2ResB !== null ? formatTokenAmount(l2ResB, USDC_DECIMALS) : prev.l2ReserveB,
          l1Quote: l1QuoteFinal !== null ? l1QuoteFinal : prev.l1Quote,
          l2Quote: l2QuoteFinal !== null ? l2QuoteFinal : prev.l2Quote,
          singlePoolQuote: singleFinal !== null ? singleFinal : prev.singlePoolQuote,
          ethBalance: ethRaw !== null ? formatTokenAmount(ethRaw, WETH_DECIMALS) : prev.ethBalance,
          wethBalance:
            wethRaw !== null ? formatTokenAmount(wethRaw, WETH_DECIMALS) : prev.wethBalance,
          usdcBalance:
            usdcRaw !== null ? formatTokenAmount(usdcRaw, USDC_DECIMALS) : prev.usdcBalance,
        }));
      } catch {
        /* poll failure -- skip this tick */
      }
    }

    pollData();
    const interval = setInterval(pollData, 3000);

    return () => {
      cancelled = true;
      clearInterval(interval);
    };
  }, [state.contractsDeployed, walletAddress, state.totalAmount, state.splitPercent]);

  // ── Viz phase progression ──
  function startVizTimers() {
    clearVizTimers();
    const delays = [0, 2000, 4000, 6000, 8000, 10000];
    delays.forEach((delay, i) => {
      const t = setTimeout(() => {
        setState((s) => {
          if (s.vizPhase < i + 1) return { ...s, vizPhase: i + 1 };
          return s;
        });
      }, delay);
      vizTimersRef.current.push(t);
    });
  }

  function fastForwardViz(targetPhase: number) {
    clearVizTimers();
    const current = stateRef.current.vizPhase;
    for (let p = current + 1; p <= targetPhase; p++) {
      const delay = (p - current) * 100;
      const phase = p;
      const t = setTimeout(() => {
        setState((s) => (s.vizPhase < phase ? { ...s, vizPhase: phase } : s));
      }, delay);
      vizTimersRef.current.push(t);
    }
  }

  function clearVizTimers() {
    vizTimersRef.current.forEach(clearTimeout);
    vizTimersRef.current = [];
  }

  // Cleanup viz timers on unmount
  useEffect(() => {
    return () => clearVizTimers();
  }, []);

  // ── wrapEth ──
  const wrapEth = useCallback(
    async (amount: string) => {
      const weth = config.aggWeth;
      if (!weth) {
        log("WETH address not configured", "err");
        return;
      }
      const weiAmount = parseTokenAmount(amount, WETH_DECIMALS);
      const valueHex = "0x" + weiAmount.toString(16);
      log(`Wrapping ${amount} ETH to WETH...`, "info");
      try {
        const txHash = await sendL1Tx({
          to: weth,
          data: encodeDeposit(),
          value: valueHex,
          gas: WRAP_GAS,
        });
        log(`Wrap tx submitted: ${txHash.slice(0, 18)}...`);
        const receipt = await pollReceipt(config.l1Rpc, txHash, 30, 2000);
        if (receipt && receipt.status === "0x1") {
          log("ETH wrapped to WETH successfully.", "ok");
        } else {
          log("Wrap tx failed or timed out.", "err");
        }
      } catch (e) {
        log(`Wrap failed: ${(e as Error).message}`, "err");
      }
    },
    [log, sendL1Tx],
  );

  // ── unwrapWeth ──
  const unwrapWeth = useCallback(
    async (amount: string) => {
      const weth = config.aggWeth;
      if (!weth) {
        log("WETH address not configured", "err");
        return;
      }
      const weiAmount = parseTokenAmount(amount, WETH_DECIMALS);
      log(`Unwrapping ${amount} WETH to ETH...`, "info");
      try {
        const txHash = await sendL1Tx({
          to: weth,
          data: encodeWithdraw(weiAmount),
          gas: WRAP_GAS,
        });
        log(`Unwrap tx submitted: ${txHash.slice(0, 18)}...`);
        const receipt = await pollReceipt(config.l1Rpc, txHash, 30, 2000);
        if (receipt && receipt.status === "0x1") {
          log("WETH unwrapped to ETH successfully.", "ok");
        } else {
          log("Unwrap tx failed or timed out.", "err");
        }
      } catch (e) {
        log(`Unwrap failed: ${(e as Error).message}`, "err");
      }
    },
    [log, sendL1Tx],
  );

  // ── execute ──
  const execute = useCallback(
    async (totalAmount: string, splitPercent: number) => {
      if (!stateRef.current.contractsDeployed) {
        log("Aggregator contracts not deployed.", "err");
        return;
      }

      const aggregator = config.aggAggregator;
      const weth = config.aggWeth;
      const usdc = config.aggUsdc;
      if (!aggregator || !weth || !usdc) {
        log("Missing contract addresses in config.", "err");
        return;
      }

      const startTime = Date.now();
      const totalWei = parseTokenAmount(totalAmount, WETH_DECIMALS);
      const localWei = (totalWei * BigInt(splitPercent)) / 100n;

      // Snapshot the predicted quotes BEFORE the swap. The polling loop
      // updates `state.l1Quote` / `state.l2Quote` / `state.singlePoolQuote`
      // continuously based on current AMM reserves, so by the time we read
      // them in the verify phase the reserves have already changed. We need
      // the BEFORE values for the improvement calculation and for the
      // L1/L2 output rows in the results card.
      const l1QuoteSnapshot = stateRef.current.l1Quote;
      const l2QuoteSnapshot = stateRef.current.l2Quote;
      const singlePoolSnapshot = stateRef.current.singlePoolQuote;

      // Reset execution state
      setState((s) => ({
        ...s,
        phase: "wrapping",
        error: null,
        txHash: null,
        l1TxStatus: null,
        l1BlockNumber: null,
        l1GasUsed: null,
        l2BlockBefore: null,
        l2BlockAfter: null,
        l2BlockNumber: null,
        l2TxHashes: [],
        l1Done: false,
        l2Done: false,
        localOutput: null,
        remoteOutput: null,
        totalOutput: null,
        improvement: null,
        usdcBalanceBefore: null,
        usdcBalanceAfter: null,
        vizPhase: 0,
        startTime,
        endTime: null,
      }));

      log("Starting aggregated cross-chain swap...", "info");

      // ── Phase 1: wrapping ──
      try {
        const wethResult = (await rpcCall(config.l1Rpc, "eth_call", [
          { to: weth, data: encodeBalanceOf(walletAddress as Address) },
          "latest",
        ])) as string;
        const currentWeth = decodeUint256Result(wethResult);

        if (currentWeth < totalWei) {
          const diff = totalWei - currentWeth;
          const diffHex = "0x" + diff.toString(16);
          log(`Wrapping ${formatTokenAmount(diff, WETH_DECIMALS)} ETH to WETH...`, "info");
          const wrapHash = await sendL1Tx({
            to: weth,
            data: encodeDeposit(),
            value: diffHex,
            gas: WRAP_GAS,
          });
          log(`Wrap tx: ${wrapHash.slice(0, 18)}...`);
          const receipt = await pollReceipt(config.l1Rpc, wrapHash, 30, 2000);
          if (!receipt || receipt.status !== "0x1") {
            setState((s) => ({ ...s, phase: "failed", error: "WETH wrap failed" }));
            log("WETH wrap transaction failed.", "err");
            return;
          }
          log("WETH wrapped.", "ok");
        } else {
          log("Sufficient WETH balance, skipping wrap.", "info");
        }
      } catch (e) {
        const msg = (e as Error).message || "Failed to check/wrap WETH";
        setState((s) => ({ ...s, phase: "failed", error: msg }));
        log(`Wrap step failed: ${msg}`, "err");
        return;
      }

      // ── Phase 2: approving ──
      setState((s) => ({ ...s, phase: "approving" }));
      try {
        const allowanceResult = (await rpcCall(config.l1Rpc, "eth_call", [
          {
            to: weth,
            data: encodeAllowance(walletAddress as Address, aggregator as Address),
          },
          "latest",
        ])) as string;
        const currentAllowance = decodeUint256Result(allowanceResult);

        if (currentAllowance < totalWei) {
          log("Approving WETH spend for aggregator...", "info");
          const approveHash = await sendL1Tx({
            to: weth,
            data: encodeApprove(aggregator as Address, MAX_UINT256),
            gas: APPROVE_GAS,
          });
          log(`Approve tx: ${approveHash.slice(0, 18)}...`);
          const receipt = await pollReceipt(config.l1Rpc, approveHash, 30, 2000);
          if (!receipt || receipt.status !== "0x1") {
            setState((s) => ({ ...s, phase: "failed", error: "WETH approval failed" }));
            log("WETH approval transaction failed.", "err");
            return;
          }
          log("WETH approved.", "ok");
        } else {
          log("Sufficient WETH allowance, skipping approve.", "info");
        }
      } catch (e) {
        const msg = (e as Error).message || "Failed to check/approve WETH";
        setState((s) => ({ ...s, phase: "failed", error: msg }));
        log(`Approve step failed: ${msg}`, "err");
        return;
      }

      // ── Record USDC balance before ──
      let usdcBefore = 0n;
      try {
        const usdcResult = (await rpcCall(config.l1Rpc, "eth_call", [
          { to: usdc, data: encodeBalanceOf(walletAddress as Address) },
          "latest",
        ])) as string;
        usdcBefore = decodeUint256Result(usdcResult);
        setState((s) => ({
          ...s,
          usdcBalanceBefore: formatTokenAmount(usdcBefore, USDC_DECIMALS),
        }));
      } catch {
        /* non-critical */
      }

      // ── Record L2 block before ──
      let l2BlockBefore: number | null = null;
      try {
        const blockResult = (await rpcCall(config.l2Rpc, "eth_blockNumber", [])) as string;
        l2BlockBefore = parseInt(blockResult, 16);
        setState((s) => ({ ...s, l2BlockBefore }));
      } catch {
        /* non-critical */
      }

      // ── Phase 3: sending ──
      setState((s) => ({ ...s, phase: "sending" }));
      log("Sending aggregatedSwap via cross-chain composer...", "info");

      let txHash: string;
      try {
        const calldata = encodeAggregatedSwap(weth as Address, totalWei, localWei);
        txHash = await sendL1ProxyTx({
          to: aggregator,
          data: calldata,
          gas: SWAP_GAS,
        });
      } catch (e) {
        const msg = (e as Error).message || "Transaction rejected";
        setState((s) => ({ ...s, phase: "failed", error: msg }));
        log(`Swap tx rejected: ${msg}`, "err");
        return;
      }

      setState((s) => ({ ...s, phase: "processing", txHash }));
      log(`Swap tx submitted: ${txHash.slice(0, 18)}... -- cross-chain processing begins`);

      // Start viz phase progression
      startVizTimers();

      // ── Phase 4: processing -- parallel poll L1 receipt + L2 block ──
      const poll = { receipt: null as TxReceipt | null };
      let l1PollError: string | null = null;

      const pollL1 = async () => {
        for (let i = 0; i < 60; i++) {
          await new Promise((r) => setTimeout(r, 2000));
          try {
            const r = (await rpcCall(
              config.l1Rpc,
              "eth_getTransactionReceipt",
              [txHash],
            )) as TxReceipt | null;
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
              fastForwardViz(7);
              log(`L1 confirmed in block ${blockNum ?? "?"}.`);
              return;
            }
          } catch {
            /* not mined yet */
          }
        }
        l1PollError = "L1 transaction not confirmed after 120s";
      };

      const pollL2 = async () => {
        if (l2BlockBefore === null) return;
        for (let i = 0; i < 40; i++) {
          await new Promise((r) => setTimeout(r, 3000));
          try {
            const blockResult = (await rpcCall(
              config.l2Rpc,
              "eth_blockNumber",
              [],
            )) as string;
            const current = parseInt(blockResult, 16);
            if (current >= l2BlockBefore + 3) {
              setState((s) => ({ ...s, l2Done: true, l2BlockAfter: current }));
              log(`L2 advanced to block ${current}.`);
              return;
            }
          } catch {
            /* retry */
          }
        }
      };

      await Promise.all([pollL1(), pollL2()]);

      if (l1PollError) {
        setState((s) => ({ ...s, phase: "failed", error: l1PollError! }));
        log(l1PollError, "err");
        clearVizTimers();
        return;
      }

      if (!poll.receipt || poll.receipt.status !== "0x1") {
        setState((s) => ({
          ...s,
          phase: "failed",
          error: "L1 transaction reverted",
        }));
        log("Aggregated swap: L1 tx reverted.", "err");
        clearVizTimers();
        return;
      }

      // ── Phase 5: verifying ──
      setState((s) => ({ ...s, phase: "verifying" }));
      log("Verifying results...", "info");

      let usdcAfter = 0n;
      try {
        const usdcResult = (await rpcCall(config.l1Rpc, "eth_call", [
          { to: usdc, data: encodeBalanceOf(walletAddress as Address) },
          "latest",
        ])) as string;
        usdcAfter = decodeUint256Result(usdcResult);
      } catch {
        /* non-critical */
      }

      const totalOutput = usdcAfter - usdcBefore;
      const totalOutputStr = formatTokenAmount(totalOutput > 0n ? totalOutput : 0n, USDC_DECIMALS);
      const usdcAfterStr = formatTokenAmount(usdcAfter, USDC_DECIMALS);

      // Use the BEFORE snapshot for the improvement calculation, not the
      // current polled value (which was computed against post-swap reserves).
      let improvementStr: string | null = null;
      if (singlePoolSnapshot && totalOutput > 0n) {
        try {
          const singleQuoteWei = parseTokenAmount(singlePoolSnapshot, USDC_DECIMALS);
          if (singleQuoteWei > 0n) {
            const diff = totalOutput - singleQuoteWei;
            const pctX1000 = (diff * 100000n) / singleQuoteWei;
            const pctFloat = Number(pctX1000) / 1000;
            const sign = pctFloat >= 0 ? "+" : "";
            improvementStr = `${sign}${pctFloat.toFixed(1)}%`;
          }
        } catch {
          /* ignore parse errors */
        }
      }

      // Fetch the SINGLE L2 cross-chain delivery tx and its block. The
      // builder posts an `executeIncomingCrossChainCall` (selector 0x0f64c845)
      // tx targeting the CCM that delivers our cross-chain calls. We scan
      // L2 blocks in [l2BlockBefore+1, l2BlockAfter] for the first such tx.
      const EXEC_INCOMING_SELECTOR = "0x0f64c845";
      const ccmL2 = config.ccmL2Address.toLowerCase();
      let l2TxHash: string | null = null;
      let l2BlockHit: number | null = null;
      try {
        const before = stateRef.current.l2BlockBefore;
        const after = stateRef.current.l2BlockAfter;
        if (before !== null && after !== null && after > before && ccmL2) {
          for (let bn = before + 1; bn <= after && !l2TxHash; bn++) {
            try {
              const block = (await rpcCall(config.l2Rpc, "eth_getBlockByNumber", [
                "0x" + bn.toString(16),
                true,
              ])) as {
                transactions?: Array<{ hash: string; to: string | null; input: string }>;
              } | null;
              if (block?.transactions) {
                for (const tx of block.transactions) {
                  if (
                    tx.to?.toLowerCase() === ccmL2 &&
                    tx.input?.toLowerCase().startsWith(EXEC_INCOMING_SELECTOR)
                  ) {
                    l2TxHash = tx.hash;
                    l2BlockHit = bn;
                    break;
                  }
                }
              }
            } catch {
              /* skip block on error */
            }
          }
        }
      } catch {
        /* non-critical */
      }
      const l2TxHashes = l2TxHash ? [l2TxHash] : [];

      const endTime = Date.now();

      fastForwardViz(8);

      setState((s) => ({
        ...s,
        phase: "complete",
        // Local/remote outputs are the BEFORE snapshots (the predicted quotes).
        // For an atomic single-tx swap with no concurrent activity these are
        // exactly what the AMMs delivered.
        localOutput: l1QuoteSnapshot,
        remoteOutput: l2QuoteSnapshot,
        totalOutput: totalOutputStr,
        usdcBalanceAfter: usdcAfterStr,
        improvement: improvementStr,
        l2TxHashes,
        l2BlockNumber: l2BlockHit,
        endTime,
      }));

      log(
        `Aggregated swap complete. Output: ${totalOutputStr} USDC${improvementStr ? ` (${improvementStr} vs single pool)` : ""}.`,
        "ok",
      );
    },
    [log, sendL1Tx, sendL1ProxyTx, walletAddress],
  );

  // ── reset ──
  const reset = useCallback(() => {
    clearVizTimers();
    setState((s) => ({
      ...INITIAL_STATE,
      contractsDeployed: s.contractsDeployed,
      totalAmount: s.totalAmount,
      splitPercent: s.splitPercent,
      l1ReserveA: s.l1ReserveA,
      l1ReserveB: s.l1ReserveB,
      l2ReserveA: s.l2ReserveA,
      l2ReserveB: s.l2ReserveB,
      l1Quote: s.l1Quote,
      l2Quote: s.l2Quote,
      singlePoolQuote: s.singlePoolQuote,
      ethBalance: s.ethBalance,
      wethBalance: s.wethBalance,
      usdcBalance: s.usdcBalance,
      loading: false,
    }));
  }, []);

  // ── setSplit ──
  const setSplit = useCallback((percent: number) => {
    const clamped = Math.max(0, Math.min(100, percent));
    setState((s) => ({ ...s, splitPercent: clamped }));
  }, []);

  // ── setAmount ──
  const setAmount = useCallback((amount: string) => {
    setState((s) => ({ ...s, totalAmount: amount }));
  }, []);

  return {
    state,
    execute,
    wrapEth,
    unwrapWeth,
    reset,
    setSplit,
    setAmount,
  };
}
