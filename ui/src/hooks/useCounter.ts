import { useCallback, useEffect, useState } from "react";
import { config, COUNTER_ABI, COUNTER_BYTECODE, ESTIMATION_SENDER } from "../config";
import { rpcCall } from "../rpc";
import { estimateGas, gasToHex } from "../lib/gasEstimation";

type Logger = (msg: string, type?: "ok" | "err" | "info") => void;
type SendTx = (params: Record<string, string>) => Promise<string>;

export type TxPhase =
  | "idle"
  | "sending"
  | "pending"
  | "confirming"
  | "confirmed"
  | "failed";

export interface TxStatus {
  phase: TxPhase;
  hash: string | null;
  gasUsed: string | null;
  error: string | null;
}

const IDLE_TX: TxStatus = {
  phase: "idle",
  hash: null,
  gasUsed: null,
  error: null,
};

interface TxReceipt {
  contractAddress?: string;
  status?: string;
  gasUsed?: string;
}

export function useCounter(log: Logger, sendTx: SendTx) {
  const [address, setAddress] = useState(
    () => localStorage.getItem("counterAddress") || "",
  );
  // Validate cached counter address — clear if no code on L2 (chain was wiped)
  useEffect(() => {
    if (!address) return;
    (async () => {
      try {
        const code = (await rpcCall(config.l2Rpc, "eth_getCode", [
          address,
          "latest",
        ])) as string;
        if (!code || code === "0x" || code === "0x0") {
          setAddress("");
          localStorage.removeItem("counterAddress");
        }
      } catch {
        /* keep cached value if RPC is down */
      }
    })();
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  const [count, setCount] = useState<number | null>(null);
  const [prevCount, setPrevCount] = useState<number | null>(null);
  const [deploying, setDeploying] = useState(false);
  const [incrementing, setIncrementing] = useState(false);
  const [txStatus, setTxStatus] = useState<TxStatus>(IDLE_TX);
  const [totalIncrements, setTotalIncrements] = useState(0);

  const refresh = useCallback(async () => {
    if (!address || !address.startsWith("0x")) return;
    try {
      const result = (await rpcCall(config.l2Rpc, "eth_call", [
        { to: address, data: COUNTER_ABI.getCount },
        "latest",
      ])) as string;
      // eth_call returns "0x" for non-existent contracts
      if (!result || result === "0x" || result.length < 4) {
        setCount(null);
        return;
      }
      const newCount = parseInt(result, 16);
      if (Number.isNaN(newCount)) {
        setCount(null);
        return;
      }
      setCount((prev) => {
        if (prev !== null && newCount !== prev) {
          setPrevCount(prev);
        }
        return newCount;
      });
    } catch {
      setCount(null);
    }
  }, [address]);

  async function waitForReceipt(
    txHash: string,
  ): Promise<TxReceipt | null> {
    setTxStatus({ phase: "pending", hash: txHash, gasUsed: null, error: null });

    for (let i = 0; i < 30; i++) {
      await new Promise((r) => setTimeout(r, 1000));
      try {
        const receipt = (await rpcCall(
          config.l2Rpc,
          "eth_getTransactionReceipt",
          [txHash],
        )) as TxReceipt | null;
        if (receipt) {
          setTxStatus({
            phase: "confirming",
            hash: txHash,
            gasUsed: receipt.gasUsed
              ? parseInt(receipt.gasUsed, 16).toLocaleString()
              : null,
            error: null,
          });
          return receipt;
        }
      } catch {
        /* not mined yet */
      }
    }
    return null;
  }

  const deploy = useCallback(async () => {
    setDeploying(true);
    setTxStatus({ phase: "sending", hash: null, gasUsed: null, error: null });
    log("Deploying Counter contract on L2...", "info");

    try {
      // Estimate gas for contract deployment
      let gasHex: string | undefined;
      try {
        const est = await estimateGas({
          rpcUrl: config.l2Rpc,
          to: "0x0000000000000000000000000000000000000000",
          data: COUNTER_BYTECODE,
          from: ESTIMATION_SENDER,
        });
        gasHex = gasToHex(est.gasLimit);
      } catch { /* let node decide */ }

      const txHash = await sendTx({
        data: COUNTER_BYTECODE,
        ...(gasHex ? { gas: gasHex } : {}),
      });
      log(`Deploy tx: ${txHash.slice(0, 18)}...`);

      const receipt = await waitForReceipt(txHash);

      if (receipt?.contractAddress) {
        setAddress(receipt.contractAddress);
        localStorage.setItem("counterAddress", receipt.contractAddress);
        setTxStatus({
          phase: "confirmed",
          hash: txHash,
          gasUsed: receipt.gasUsed
            ? parseInt(receipt.gasUsed, 16).toLocaleString()
            : null,
          error: null,
        });
        log(`Counter deployed at ${receipt.contractAddress}`);

        // Auto-clear status after 5s
        setTimeout(() => setTxStatus(IDLE_TX), 5000);
      } else {
        setTxStatus({
          phase: "failed",
          hash: txHash,
          gasUsed: null,
          error: "No receipt after 30s",
        });
        log("Deploy tx sent but no receipt yet", "err");
      }
    } catch (e) {
      const msg = (e as Error).message;
      setTxStatus({ phase: "failed", hash: null, gasUsed: null, error: msg });
      log(`Deploy failed: ${msg}`, "err");
    } finally {
      setDeploying(false);
    }
  }, [log, sendTx]);

  const increment = useCallback(async () => {
    if (!address || !address.startsWith("0x")) {
      log("Set counter address first (or click Deploy)", "err");
      return;
    }
    setIncrementing(true);
    setTxStatus({ phase: "sending", hash: null, gasUsed: null, error: null });

    try {
      // Estimate gas for increment call
      let gasHex: string | undefined;
      try {
        const est = await estimateGas({
          rpcUrl: config.l2Rpc,
          to: address,
          data: COUNTER_ABI.increment,
          from: ESTIMATION_SENDER,
        });
        gasHex = gasToHex(est.gasLimit);
      } catch { /* let node decide */ }

      const txHash = await sendTx({
        to: address,
        data: COUNTER_ABI.increment,
        ...(gasHex ? { gas: gasHex } : {}),
      });
      log(`Increment tx: ${txHash.slice(0, 18)}...`);

      const receipt = await waitForReceipt(txHash);

      if (receipt) {
        setTxStatus({
          phase: "confirmed",
          hash: txHash,
          gasUsed: receipt.gasUsed
            ? parseInt(receipt.gasUsed, 16).toLocaleString()
            : null,
          error: null,
        });
        setTotalIncrements((n) => n + 1);
        await refresh();
        log("Counter incremented successfully");

        setTimeout(() => setTxStatus(IDLE_TX), 5000);
      } else {
        setTxStatus({
          phase: "failed",
          hash: txHash,
          gasUsed: null,
          error: "No receipt after 30s",
        });
        log("Increment tx sent but no receipt yet", "err");
      }
    } catch (e) {
      const msg = (e as Error).message;
      setTxStatus({ phase: "failed", hash: null, gasUsed: null, error: msg });
      log(`Increment failed: ${msg}`, "err");
    } finally {
      setIncrementing(false);
    }
  }, [address, log, sendTx, refresh]);

  // Auto-refresh count every poll cycle
  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 3000);
    return () => clearInterval(interval);
  }, [refresh]);

  return {
    address,
    setAddress,
    count,
    prevCount,
    deploying,
    incrementing,
    txStatus,
    totalIncrements,
    deploy,
    increment,
    refresh,
  };
}
