import { useCallback, useEffect, useState } from "react";
import { config } from "../config";
import { rpcCall } from "../rpc";
import type { ChainStats, L2Stats } from "../types";

interface BlockResult {
  number: string;
  timestamp: string;
  gasUsed: string;
  gasLimit: string;
  transactions: unknown[];
}

function parseBlockStats(block: BlockResult): Partial<ChainStats> {
  return {
    blockNumber: parseInt(block.number, 16),
    timestamp: parseInt(block.timestamp, 16),
    gasUsed: parseInt(block.gasUsed, 16),
    gasLimit: parseInt(block.gasLimit, 16),
    txCount: block.transactions.length,
  };
}

export function useDashboard() {
  const [l1, setL1] = useState<ChainStats>({
    blockNumber: null,
    stateRoot: "",
    timestamp: null,
    gasUsed: null,
    gasLimit: null,
    txCount: null,
  });
  const [l2, setL2] = useState<L2Stats>({
    blockNumber: null,
    stateRoot: "",
    timestamp: null,
    gasUsed: null,
    gasLimit: null,
    txCount: null,
    synced: null,
    gasPrice: null,
  });
  const [connected, setConnected] = useState(false);

  const refreshL1 = useCallback(async () => {
    try {
      const block = (await rpcCall(config.l1Rpc, "eth_getBlockByNumber", [
        "latest",
        false,
      ])) as BlockResult;
      const stats = parseBlockStats(block);
      setL1((s) => ({ ...s, ...stats }));
    } catch {
      setL1((s) => ({ ...s, blockNumber: null }));
    }

    // Query L1 state root from Rollups contract
    if (config.rollupsAddress && config.rollupId) {
      try {
        const paddedId = parseInt(config.rollupId)
          .toString(16)
          .padStart(64, "0");
        const result = (await rpcCall(config.l1Rpc, "eth_call", [
          {
            to: config.rollupsAddress,
            data: "0xdbc1697b" + paddedId,
          },
          "latest",
        ])) as string;
        if (result && result.length >= 194) {
          setL1((s) => ({
            ...s,
            stateRoot: "0x" + result.slice(130, 194),
          }));
        }
      } catch {
        /* Rollups contract might not be deployed */
      }
    }
  }, []);

  const refreshL2 = useCallback(async () => {
    try {
      const [block, stateRoot, synced, gasPriceHex] = await Promise.all([
        rpcCall(config.l2Rpc, "eth_getBlockByNumber", [
          "latest",
          false,
        ]) as Promise<BlockResult>,
        rpcCall(config.l2Rpc, "syncrollups_getStateRoot") as Promise<string>,
        rpcCall(config.l2Rpc, "syncrollups_isSynced") as Promise<boolean>,
        rpcCall(config.l2Rpc, "eth_gasPrice") as Promise<string>,
      ]);

      const stats = parseBlockStats(block);
      const gasPriceGwei = (parseInt(gasPriceHex, 16) / 1e9).toFixed(2);

      setL2({
        ...stats,
        stateRoot,
        synced,
        gasPrice: gasPriceGwei,
        blockNumber: stats.blockNumber ?? null,
        timestamp: stats.timestamp ?? null,
        gasUsed: stats.gasUsed ?? null,
        gasLimit: stats.gasLimit ?? null,
        txCount: stats.txCount ?? null,
      });
      setConnected(true);
    } catch {
      setL2((s) => ({ ...s, blockNumber: null }));
      setConnected(false);
    }
  }, []);

  const refreshAll = useCallback(async () => {
    await Promise.all([refreshL1(), refreshL2()]);
  }, [refreshL1, refreshL2]);

  // Auto-poll every 3 seconds
  useEffect(() => {
    refreshAll();
    const interval = setInterval(refreshAll, 3000);
    return () => clearInterval(interval);
  }, [refreshAll]);

  return { l1, l2, connected, refreshAll };
}
