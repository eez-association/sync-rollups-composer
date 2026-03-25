import { useCallback, useEffect, useState } from "react";
import { config, ESTIMATION_SENDER } from "../config";
import { rpcCall } from "../rpc";
import { estimateGas, estimateCrossChainGas, gasToHex } from "../lib/gasEstimation";

type Logger = (msg: string, type?: "ok" | "err" | "info") => void;
type SendL1Tx = (params: Record<string, string>) => Promise<string>;
type SendL1ProxyTx = (params: Record<string, string>) => Promise<string>;

export type CrossChainPhase =
  | "idle"
  | "creating-proxy"
  | "proxy-pending"
  | "sending"
  | "l1-pending"
  | "confirmed"
  | "failed";

export interface CrossChainState {
  phase: CrossChainPhase;
  proxyAddress: string;
  targetAddress: string;
  calldata: string;
  txHash: string | null;
  error: string | null;
}

const IDLE: CrossChainState = {
  phase: "idle",
  proxyAddress: "",
  targetAddress: "",
  calldata: "",
  txHash: null,
  error: null,
};

/** ABI selectors for Rollups contract */
const ROLLUPS_ABI = {
  // createCrossChainProxy(address,uint256) → returns address
  createProxy: "0x2dd72120",
  // computeCrossChainProxyAddress(address,uint256) → returns address
  // NOTE: domain/chainId parameter was removed in feature/contract_updates
  computeProxy: "0xb761ba7e",
};

/** Encode address + uint256 as ABI params */
function encodeAddressUint(addr: string, num: string): string {
  const a = addr.toLowerCase().replace("0x", "").padStart(64, "0");
  const n = parseInt(num).toString(16).padStart(64, "0");
  return a + n;
}

interface TxReceipt {
  status?: string;
  gasUsed?: string;
  logs?: unknown[];
  revertReason?: string;
}

/** Try to get a revert reason by replaying the tx via eth_call */
async function fetchRevertReason(rpcUrl: string, txHash: string): Promise<string> {
  try {
    // Get the original tx params
    const tx = (await rpcCall(rpcUrl, "eth_getTransactionByHash", [txHash])) as {
      from?: string;
      to?: string;
      data?: string;
      input?: string;
      value?: string;
      blockNumber?: string;
    } | null;
    if (!tx?.to) return "";

    // Replay via eth_call at the block it was mined in
    const result = (await rpcCall(rpcUrl, "eth_call", [
      { from: tx.from, to: tx.to, data: tx.input || tx.data, value: tx.value },
      tx.blockNumber || "latest",
    ])) as string;
    return result || "";
  } catch (e) {
    // The error message from eth_call often contains the revert reason
    const msg = (e as Error).message || "";
    // Try to extract revert string from common formats
    const match = msg.match(/revert(?:ed)?:?\s*(.*)/i) || msg.match(/reason:\s*(.*)/i);
    if (match?.[1]) return match[1].trim();
    // Return raw error if short enough
    if (msg.length < 200) return msg;
    return "Reverted (reason unknown)";
  }
}

export function useCrossChain(log: Logger, sendL1Tx: SendL1Tx, sendL1ProxyTx: SendL1ProxyTx) {
  const [state, setState] = useState<CrossChainState>(IDLE);
  const [savedProxies, setSavedProxies] = useState<
    Record<string, string>
  >(() => {
    try {
      return JSON.parse(localStorage.getItem("crossChainProxies") || "{}") as Record<string, string>;
    } catch {
      return {};
    }
  });

  // On mount, prune cached proxies that no longer have code on L1
  // (e.g. after chain wipe / fresh deploy)
  useEffect(() => {
    (async () => {
      const entries = Object.entries(savedProxies);
      if (entries.length === 0) return;
      const valid: Record<string, string> = {};
      for (const [target, proxy] of entries) {
        try {
          const code = (await rpcCall(config.l1Rpc, "eth_getCode", [
            proxy,
            "latest",
          ])) as string;
          if (code && code !== "0x" && code !== "0x0") {
            valid[target] = proxy;
          }
        } catch {
          /* skip */
        }
      }
      if (Object.keys(valid).length !== entries.length) {
        setSavedProxies(valid);
        localStorage.setItem("crossChainProxies", JSON.stringify(valid));
      }
    })();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  /** Compute the deterministic proxy address for a given L2 target */
  const computeProxyAddress = useCallback(
    async (targetAddr: string): Promise<string | null> => {
      if (!config.rollupsAddress || !config.rollupId) return null;

      try {
        // computeCrossChainProxyAddress(address originalAddress, uint256 originalRollupId)
        // The domain/chainId parameter was removed in feature/contract_updates
        const result = (await rpcCall(config.l1Rpc, "eth_call", [
          {
            to: config.rollupsAddress,
            data:
              ROLLUPS_ABI.computeProxy +
              encodeAddressUint(targetAddr, config.rollupId),
          },
          "latest",
        ])) as string;

        if (result && result.length >= 66) {
          return "0x" + result.slice(26, 66);
        }
      } catch {
        /* Rollups contract might not support this */
      }
      return null;
    },
    [],
  );

  /** Check if a proxy already exists (has code) */
  const proxyExists = useCallback(
    async (proxyAddr: string): Promise<boolean> => {
      try {
        const code = (await rpcCall(config.l1Rpc, "eth_getCode", [
          proxyAddr,
          "latest",
        ])) as string;
        return code !== "0x" && code !== "0x0";
      } catch {
        return false;
      }
    },
    [],
  );

  /** Create a CrossChainProxy on L1 for a given L2 target address */
  const createProxy = useCallback(
    async (targetAddr: string) => {
      if (!config.rollupsAddress || !config.rollupId) {
        log("Rollups contract not configured", "err");
        return;
      }

      setState({
        phase: "creating-proxy",
        proxyAddress: "",
        targetAddress: targetAddr,
        calldata: "",
        txHash: null,
        error: null,
      });
      log(`Creating CrossChainProxy for ${targetAddr.slice(0, 10)}...`, "info");

      try {
        // First compute the expected proxy address
        const expectedAddr = await computeProxyAddress(targetAddr);

        // Check if it already exists
        if (expectedAddr && (await proxyExists(expectedAddr))) {
          const proxy = expectedAddr;
          setState({
            phase: "confirmed",
            proxyAddress: proxy,
            targetAddress: targetAddr,
            calldata: "",
            txHash: null,
            error: null,
          });

          // Save mapping
          const updated = { ...savedProxies, [targetAddr.toLowerCase()]: proxy };
          setSavedProxies(updated);
          localStorage.setItem("crossChainProxies", JSON.stringify(updated));

          log(`Proxy already exists at ${proxy}`);
          setTimeout(
            () => setState((s) => (s.phase === "confirmed" ? IDLE : s)),
            5000,
          );
          return;
        }

        // Create via Rollups.createCrossChainProxy(address, uint256)
        const createData = ROLLUPS_ABI.createProxy +
          encodeAddressUint(targetAddr, config.rollupId);

        let gasHex: string | undefined;
        try {
          const est = await estimateGas({
            rpcUrl: config.l1Rpc,
            to: config.rollupsAddress,
            data: createData,
            from: ESTIMATION_SENDER,
          });
          gasHex = gasToHex(est.gasLimit);
        } catch {
          // Estimation failed — let the node use its default gas limit
        }

        const txHash = await sendL1Tx({
          to: config.rollupsAddress,
          data: createData,
          ...(gasHex ? { gas: gasHex } : {}),
        });

        setState((s) => ({
          ...s,
          phase: "proxy-pending",
          txHash,
        }));
        log(`Create proxy tx: ${txHash.slice(0, 18)}...`);

        // Wait for receipt
        for (let i = 0; i < 30; i++) {
          await new Promise((r) => setTimeout(r, 1000));
          try {
            const receipt = (await rpcCall(
              config.l1Rpc,
              "eth_getTransactionReceipt",
              [txHash],
            )) as TxReceipt | null;
            if (receipt) {
              if (receipt.status === "0x1") {
                // Get the proxy address
                const proxy = expectedAddr || (await computeProxyAddress(targetAddr)) || "";
                setState({
                  phase: "confirmed",
                  proxyAddress: proxy,
                  targetAddress: targetAddr,
                  calldata: "",
                  txHash,
                  error: null,
                });

                const updated = { ...savedProxies, [targetAddr.toLowerCase()]: proxy };
                setSavedProxies(updated);
                localStorage.setItem(
                  "crossChainProxies",
                  JSON.stringify(updated),
                );

                log(`Proxy created at ${proxy}`);
                setTimeout(
                  () =>
                    setState((s) => (s.phase === "confirmed" ? IDLE : s)),
                  5000,
                );
              } else {
                const reason = await fetchRevertReason(config.l1Rpc, txHash);
                const errorMsg = reason ? `Reverted: ${reason}` : "Transaction reverted";
                setState({
                  phase: "failed",
                  proxyAddress: "",
                  targetAddress: targetAddr,
                  calldata: "",
                  txHash,
                  error: errorMsg,
                });
                log(`Create proxy tx reverted${reason ? `: ${reason}` : ""}`, "err");
              }
              return;
            }
          } catch {
            /* not mined yet */
          }
        }

        setState((s) => ({
          ...s,
          phase: "failed",
          error: "No receipt after 30s",
        }));
      } catch (e) {
        const msg = (e as Error).message;
        setState({
          phase: "failed",
          proxyAddress: "",
          targetAddress: targetAddr,
          calldata: "",
          txHash: null,
          error: msg,
        });
        log(`Create proxy failed: ${msg}`, "err");
      }
    },
    [log, sendL1Tx, computeProxyAddress, proxyExists, savedProxies],
  );

  /**
   * Send a cross-chain call through the L1 proxy.
   *
   * The L1 proxy does NOT forward the tx to L1 immediately. Instead:
   * 1. Proxy traces the tx, detects the cross-chain call
   * 2. Proxy queues execution entries + raw L1 tx with the builder
   * 3. Proxy returns a pre-computed tx hash (from the raw tx bytes)
   * 4. Builder later includes entries in an L2 block, submits postBatch to L1,
   *    then forwards the queued user L1 tx — both land in the same L1 block
   *
   * This means the tx hash we get back may not exist on L1 for 24-36s
   * (builder needs to build a block + submit postBatch + L1 block time).
   * If the builder is stuck, the tx may never reach L1 at all.
   */
  const sendCrossChainCall = useCallback(
    async (proxyAddr: string, calldata: string, targetAddr?: string, _value?: string, gas?: string) => {
      setState({
        phase: "sending",
        proxyAddress: proxyAddr,
        targetAddress: targetAddr || "",
        calldata,
        txHash: null,
        error: null,
      });
      log(
        `Sending cross-chain call to ${proxyAddr.slice(0, 10)}...`,
        "info",
      );

      try {
        // Use pre-estimated gas if provided, otherwise estimate now
        let gasHex = gas;
        if (!gasHex) {
          try {
            const est = await estimateCrossChainGas({
              l1Rpc: config.l1Rpc,
              proxyAddress: proxyAddr,
              calldata,
              from: ESTIMATION_SENDER,
            });
            gasHex = gasToHex(est.gasLimit);
            log(`Gas estimated: ${Number(est.rawEstimate).toLocaleString()} (limit: ${Number(est.gasLimit).toLocaleString()}, method: ${est.method})`);
          } catch (e) {
            log(`Gas estimation failed: ${(e as Error).message}`, "err");
          }
        }

        // Build tx params — always include gas to prevent wallet from
        // re-estimating (which fails for cross-chain calls and produces
        // incorrect values like Rabby's 2M fallback).
        const txParams: Record<string, string> = {
          to: proxyAddr,
          data: calldata,
        };
        if (_value) txParams.value = _value;
        if (gasHex) txParams.gas = gasHex;

        // Send through L1 proxy — queues execution entries + raw tx with the builder.
        // Returns a pre-computed tx hash (tx is NOT on L1 yet).
        const txHash = await sendL1ProxyTx(txParams);

        setState({
          phase: "l1-pending",
          proxyAddress: proxyAddr,
          targetAddress: targetAddr || "",
          calldata,
          txHash,
          error: null,
        });
        log(`Cross-chain call queued: ${txHash.slice(0, 18)}... (waiting for composer to submit)`);

        // Poll for L1 receipt.
        // The tx hash is pre-computed — it won't appear on L1 until the builder
        // forwards it alongside a postBatch (typically 24-36s). We poll for 60s
        // and periodically check if the tx has even been broadcast to L1.
        let txSeenOnL1 = false;
        for (let i = 0; i < 60; i++) {
          await new Promise((r) => setTimeout(r, 1000));
          try {
            const receipt = (await rpcCall(
              config.l1Rpc,
              "eth_getTransactionReceipt",
              [txHash],
            )) as TxReceipt | null;
            if (receipt) {
              if (receipt.status === "0x1") {
                setState({
                  phase: "confirmed",
                  proxyAddress: proxyAddr,
                  targetAddress: targetAddr || "",
                  calldata,
                  txHash,
                  error: null,
                });
                log("Cross-chain call confirmed on L1 — L2 state updated");
                setTimeout(
                  () =>
                    setState((s) => (s.phase === "confirmed" ? IDLE : s)),
                  5000,
                );
              } else {
                const reason = await fetchRevertReason(config.l1Rpc, txHash);
                const errorMsg = reason ? `Reverted: ${reason}` : "L1 transaction reverted";
                setState({
                  phase: "failed",
                  proxyAddress: proxyAddr,
                  targetAddress: targetAddr || "",
                  calldata,
                  txHash,
                  error: errorMsg,
                });
                log(`Cross-chain call reverted${reason ? `: ${reason}` : ""}`, "err");
              }
              return;
            }
          } catch {
            /* not mined yet */
          }

          // Every 12s, check if the tx even exists on L1 (has it been broadcast?)
          if (i > 0 && i % 12 === 0 && !txSeenOnL1) {
            try {
              const tx = await rpcCall(config.l1Rpc, "eth_getTransactionByHash", [txHash]);
              if (tx) {
                txSeenOnL1 = true;
                log("Transaction seen on L1, waiting for confirmation...");
              } else if (i >= 36) {
                // After 36s with no sign of the tx on L1, the builder is likely stuck
                const errorMsg = "Transaction not broadcast to L1 — composer may be unable to submit batches. Check node health.";
                setState({
                  phase: "failed",
                  proxyAddress: proxyAddr,
                  targetAddress: targetAddr || "",
                  calldata,
                  txHash,
                  error: errorMsg,
                });
                log(errorMsg, "err");
                return;
              }
            } catch {
              /* ignore check errors */
            }
          }
        }

        // Final timeout — check why
        const finalErrorMsg = txSeenOnL1
          ? "L1 transaction pending but not confirmed after 60s — it may still confirm. Check the explorer."
          : "Transaction not broadcast to L1 after 60s — composer may be unable to submit batches. Check node health.";
        setState((s) => ({
          ...s,
          phase: "failed",
          error: finalErrorMsg,
        }));
        log(finalErrorMsg, "err");
      } catch (e) {
        const msg = (e as Error).message;
        setState({
          phase: "failed",
          proxyAddress: proxyAddr,
          targetAddress: targetAddr || "",
          calldata,
          txHash: null,
          error: msg,
        });
        log(`Cross-chain call failed: ${msg}`, "err");
      }
    },
    [log, sendL1ProxyTx],
  );

  /** Look up saved proxy for a target address (reads localStorage fresh for cross-instance sync) */
  const getProxy = useCallback(
    (targetAddr: string): string | null => {
      // Check in-memory first
      const inMemory = savedProxies[targetAddr.toLowerCase()];
      if (inMemory) return inMemory;
      // Also check localStorage for proxies created by other hook instances
      try {
        const stored = JSON.parse(localStorage.getItem("crossChainProxies") || "{}") as Record<string, string>;
        return stored[targetAddr.toLowerCase()] || null;
      } catch {
        return null;
      }
    },
    [savedProxies],
  );

  const reset = useCallback(() => setState(IDLE), []);

  return {
    state,
    savedProxies,
    createProxy,
    sendCrossChainCall,
    computeProxyAddress,
    getProxy,
    reset,
  };
}
