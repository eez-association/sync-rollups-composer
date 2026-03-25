import { useState, useEffect, useRef } from "react";
import { decodeFunctionData, decodeAbiParameters } from "viem";
import type { TxRecord } from "../hooks/useTxHistory";
import { config } from "../config";
import { rollupsAbi } from "../abi/rollups";
import { TxLink } from "./TxLink";
import styles from "./TxHistoryPanel.module.css";

interface Props {
  records: TxRecord[];
  onClear: () => void;
  onDebug?: (txHash: string) => void;
  onViewBlock?: (blockNumber: number) => void;
}

type BlockInfo = { l1?: number; l2?: number };

const TYPE_LABELS: Record<TxRecord["type"], string> = {
  deploy: "Deploy",
  increment: "Increment",
  "cross-chain-proxy": "Create Proxy",
  "cross-chain-call": "Cross-Chain",
  faucet: "Faucet",
};

const TYPE_COLORS: Record<TxRecord["type"], string> = {
  deploy: "var(--accent-light)",
  increment: "var(--green)",
  "cross-chain-proxy": "var(--yellow)",
  "cross-chain-call": "var(--accent)",
  faucet: "var(--cyan)",
};

function timeAgo(ts: number): string {
  const diff = Math.floor((Date.now() - ts) / 1000);
  if (diff < 5) return "just now";
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`;
  return new Date(ts).toLocaleDateString();
}

function StatusBadge({ status }: { status: TxRecord["status"] }) {
  return (
    <span className={`${styles.badge} ${styles[status]}`}>
      {status === "pending" && <span className={styles.spinner} />}
      {status === "confirmed" && (
        <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
      )}
      {status === "failed" && (
        <svg width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
      )}
      {status}
    </span>
  );
}

/** Determine which chain a tx lives on based on its type */
function txChain(type: TxRecord["type"]): "l1" | "l2" {
  return type === "deploy" || type === "increment" ? "l2" : "l1";
}
// Note: faucet txs always go through L1 (direct transfer or bridge deposit), so "l1" is correct

/** Low-level JSON-RPC helper */
async function rpc(url: string, method: string, params: unknown[]): Promise<any> {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }),
  });
  const json = await res.json();
  return json?.result ?? null;
}

/**
 * Extract L2 block numbers from postBatch calldata in a given L1 block.
 * postBatch(rollupId, entries, callData, proof) — callData encodes abi.encode(uint256[], bytes[])
 * with L2 block numbers as the first array.
 */
async function extractL2BlocksFromL1Block(l1BlockNum: number): Promise<number[]> {
  const blockHex = "0x" + l1BlockNum.toString(16);
  const logs = await rpc(config.l1Rpc, "eth_getLogs", [{
    address: config.rollupsAddress,
    fromBlock: blockHex,
    toBlock: blockHex,
  }]);
  if (!Array.isArray(logs) || logs.length === 0) return [];

  // Collect unique tx hashes from logs
  const txHashes = new Set<string>();
  for (const log of logs) {
    if (log.transactionHash) txHashes.add(log.transactionHash);
  }

  // Try each tx — decode as postBatch to get L2 block numbers
  for (const txHash of txHashes) {
    try {
      const l1Tx = await rpc(config.l1Rpc, "eth_getTransactionByHash", [txHash]);
      if (!l1Tx?.input) continue;
      const decoded = decodeFunctionData({ abi: rollupsAbi, data: l1Tx.input as `0x${string}` });
      if (decoded.functionName !== "postBatch") continue;
      const callData = decoded.args[2] as `0x${string}`;
      if (!callData || callData === "0x" || callData.length <= 2) continue;
      const [blockNumbers] = decodeAbiParameters(
        [{ type: "uint256[]" }, { type: "bytes[]" }],
        callData,
      );
      return (blockNumbers as bigint[]).map((n) => Number(n));
    } catch { /* try next tx */ }
  }
  return [];
}

/**
 * Fetch both L1 and L2 block numbers for a transaction.
 *
 * - L1 txs (proxy/call): receipt gives L1 block; L2 blocks extracted from
 *   postBatch calldata in the same L1 block (they land together by design).
 * - L2 txs (deploy/increment): receipt gives L2 block; the L2 block header's
 *   `mixHash` (prevRandao) carries the L1 block number (see CLAUDE.md).
 */
async function fetchBlockInfo(
  hash: string,
  chain: "l1" | "l2",
): Promise<BlockInfo | null> {
  const rpcUrl = chain === "l1" ? config.l1Rpc : config.l2Rpc;
  if (!rpcUrl) return null;
  try {
    const receipt = await rpc(rpcUrl, "eth_getTransactionReceipt", [hash]);
    if (!receipt?.blockNumber) return null;
    const blockNum = parseInt(receipt.blockNumber, 16);

    if (chain === "l1") {
      const info: BlockInfo = { l1: blockNum };
      // Extract L2 block numbers from postBatch in the same L1 block
      try {
        const l2Blocks = await extractL2BlocksFromL1Block(blockNum);
        if (l2Blocks.length > 0) info.l2 = l2Blocks[0];
      } catch { /* L2 extraction is best-effort */ }
      return info;
    }

    // L2 tx: also extract L1 block from the L2 block header's mixHash/prevRandao
    const info: BlockInfo = { l2: blockNum };
    try {
      const blockHex = "0x" + blockNum.toString(16);
      const l2Block = await rpc(config.l2Rpc, "eth_getBlockByNumber", [blockHex, false]);
      if (l2Block?.mixHash) {
        const l1Num = parseInt(l2Block.mixHash, 16);
        if (l1Num > 0) info.l1 = l1Num;
      }
    } catch { /* L1 block derivation is best-effort */ }
    return info;
  } catch {
    return null;
  }
}

export function TxHistoryPanel({ records, onClear, onDebug, onViewBlock }: Props) {
  // Lazily fetched block numbers keyed by tx hash
  const [blockCache, setBlockCache] = useState<Map<string, BlockInfo>>(
    () => new Map(),
  );
  const fetchingRef = useRef<Set<string>>(new Set());

  // Fetch block numbers for confirmed txs with hashes not yet cached
  useEffect(() => {
    for (const tx of records) {
      if (tx.status !== "confirmed" || !tx.hash) continue;
      if (blockCache.has(tx.hash) || fetchingRef.current.has(tx.hash)) continue;
      fetchingRef.current.add(tx.hash);

      const chain = txChain(tx.type);
      const hash = tx.hash;
      fetchBlockInfo(hash, chain).then((info) => {
        if (!info) return;
        setBlockCache((prev) => {
          const next = new Map(prev);
          next.set(hash, info);
          return next;
        });
      });
    }
  }, [records]); // eslint-disable-line react-hooks/exhaustive-deps

  if (records.length === 0) return null;

  return (
    <div className={styles.card}>
      <div className={styles.cardHeader}>
        <span className={styles.cardTitle}>Transaction History</span>
        <div className={styles.headerRight}>
          <span className={styles.count}>{records.length} tx{records.length !== 1 ? "s" : ""}</span>
          <button className="btn btn-sm btn-outline" onClick={onClear}>Clear</button>
        </div>
      </div>

      <div className={styles.list}>
        {records.map((tx) => {
          const info = tx.hash ? blockCache.get(tx.hash) : undefined;
          return (
            <div key={tx.id} className={styles.row}>
              <div className={styles.typeCol}>
                <span
                  className={styles.typeDot}
                  style={{ background: TYPE_COLORS[tx.type] }}
                />
                <span className={styles.typeLabel}>{TYPE_LABELS[tx.type]}</span>
              </div>

              <div className={styles.labelCol}>{tx.label}</div>

              <div className={styles.hashCol}>
                {tx.hash ? (
                  <TxLink
                    hash={tx.hash}
                    chain={txChain(tx.type)}
                    className={styles.hash}
                  />
                ) : (
                  <span className={styles.noHash}>&mdash;</span>
                )}
              </div>

              <div className={styles.blockCol}>
                <span className={styles.blockLabel}>L1</span>
                <span className={styles.blockNum}>
                  {info?.l1 != null ? info.l1.toLocaleString() : "\u2014"}
                </span>
              </div>

              <div className={styles.blockCol}>
                <span className={styles.blockLabel}>L2</span>
                <span className={styles.blockNum}>
                  {info?.l2 != null ? info.l2.toLocaleString() : "\u2014"}
                </span>
              </div>

              <div className={styles.gasCol}>
                {tx.gasUsed ? (
                  <span className={styles.gas}>{tx.gasUsed}</span>
                ) : null}
              </div>

              <div className={styles.statusCol}>
                <StatusBadge status={tx.status} />
              </div>

              <div className={styles.timeCol}>
                <span className={styles.time}>{timeAgo(tx.timestamp)}</span>
              </div>

              <div className={styles.actionsCol}>
                {onViewBlock && info?.l1 != null && (
                  <button
                    className={styles.explorerBtn}
                    onClick={() => onViewBlock(info.l1!)}
                    title="View in Crosschain Explorer"
                  >
                    <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                      <rect x="3" y="3" width="18" height="18" rx="2" ry="2" />
                      <line x1="3" y1="9" x2="21" y2="9" />
                      <line x1="9" y1="21" x2="9" y2="9" />
                    </svg>
                    Explorer
                  </button>
                )}
                {onDebug && tx.hash && tx.type === "cross-chain-call" && (
                  <button
                    className="btn btn-sm btn-yellow btn-tint"
                    onClick={() => onDebug(tx.hash!)}
                    title="Debug in Visualizer"
                  >
                    <svg width="11" height="11" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                      <circle cx="11" cy="11" r="8" />
                      <line x1="21" y1="21" x2="16.65" y2="16.65" />
                    </svg>
                    Debug
                  </button>
                )}
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}
