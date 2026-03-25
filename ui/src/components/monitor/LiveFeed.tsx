/**
 * LiveFeed — Real-time feed of L1 blocks containing cross-chain transactions.
 * Starts from the latest block and polls forward for new events.
 * Older events are loaded on demand.
 */

import { useState, useEffect, useRef, useCallback } from "react";
import { createPublicClient, http, decodeFunctionData, decodeAbiParameters, type PublicClient } from "viem";
import { foundry } from "viem/chains";
import { config } from "../../config";
import { rollupsAbi } from "../../abi/rollups";
import { rpcCall } from "../../rpc";
import { ActionType } from "../../types/chain";
import type { ExecutionEntry } from "../../types/chain";
import styles from "./LiveFeed.module.css";

/* ── Types ─────────────────────────────────────────────────────────── */

type Direction = "L1\u2192L2" | "L2\u2192L1";

type CrossChainTx = {
  actionHash: string;
  direction: Direction;
  destination: string;
  sourceAddress: string;
  actionType: string;
};

type BlockEntry = {
  l1BlockNumber: bigint;
  l1TxHash: string;
  timestamp: number | null;
  crossChainTxs: CrossChainTx[];
  l2BlockNumbers: number[];
  isNew: boolean;
};

type Filter = "all" | "l1-to-l2" | "l2-to-l1";

/** Enrichment data from CrossChainCallExecuted events */
type CallInfo = { proxy: string; sourceAddress: string };

interface Props {
  onNavigateToBlock: (blockNumber: number) => void;
}

/* ── Constants ─────────────────────────────────────────────────────── */

const ZERO_HASH =
  "0x0000000000000000000000000000000000000000000000000000000000000000";
const ZERO_ADDR = "0x0000000000000000000000000000000000000000";
const POLL_INTERVAL = 4_000;
const PAGE_SIZE = 25;
/** How many L1 blocks to scan per window (wide enough to catch sparse events) */
const SCAN_WINDOW = 500n;

/* ── Helpers ───────────────────────────────────────────────────────── */

/**
 * Classify a BatchPosted execution entry, enriching with CrossChainCallExecuted data.
 *
 * Direction detection:
 * - L2→L1 (withdrawals): nested format — nextAction has non-empty scope, or
 *   sourceRollup > 0 indicating the action originates from our rollup
 * - L1→L2 (deposits/cross-chain calls): all other entries
 *
 * RESULT entries have destination=0x0, sourceAddress=0x0 — we enrich these
 * from CrossChainCallExecuted events which have the real proxy and sourceAddress.
 */
function classifyEntry(
  entry: ExecutionEntry,
  callInfoMap: Map<string, CallInfo>,
): CrossChainTx | null {
  if (entry.actionHash === ZERO_HASH) return null;

  const act = entry.nextAction;

  // L2→L1 withdrawals use nested format: nextAction has non-empty scope,
  // or sourceRollup > 0 indicating origin from our rollup
  const isL2ToL1 = act.scope.length > 0 || act.sourceRollup > 0n;
  const direction: Direction = isL2ToL1 ? "L2\u2192L1" : "L1\u2192L2";

  let actionType = "CALL";
  if (act.actionType === ActionType.L2TX) {
    actionType = "L2TX";
  } else if (
    act.actionType === ActionType.REVERT ||
    act.actionType === ActionType.REVERT_CONTINUE
  ) {
    actionType = "REVERT";
  }

  // Use real addresses from CrossChainCallExecuted when available
  // (RESULT entries have destination=0x0, sourceAddress=0x0 in BatchPosted)
  const enrichment = callInfoMap.get(entry.actionHash);
  const destination =
    enrichment?.proxy ||
    (act.destination !== ZERO_ADDR ? act.destination : "");
  const sourceAddress =
    enrichment?.sourceAddress ||
    (act.sourceAddress !== ZERO_ADDR ? act.sourceAddress : "");

  return {
    actionHash: entry.actionHash,
    direction,
    destination,
    sourceAddress,
    actionType,
  };
}

function timeAgo(ts: number): string {
  const diff = Math.floor(Date.now() / 1000) - ts;
  if (diff < 5) return "just now";
  if (diff < 60) return `${diff}s ago`;
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`;
  return `${Math.floor(diff / 3600)}h ago`;
}

function truncateHex(hex: string): string {
  if (!hex || hex.length <= 16) return hex || "\u2014";
  return `${hex.slice(0, 10)}\u2026${hex.slice(-6)}`;
}

function matchesFilter(block: BlockEntry, filter: Filter): boolean {
  if (filter === "all") return true;
  const target: Direction =
    filter === "l1-to-l2" ? "L1\u2192L2" : "L2\u2192L1";
  return block.crossChainTxs.some((tx) => tx.direction === target);
}

/** Fetch CrossChainCallExecuted events and build an enrichment map keyed by actionHash */
async function fetchCallInfoMap(
  client: PublicClient,
  fromBlock: bigint,
  toBlock: bigint,
): Promise<Map<string, CallInfo>> {
  const map = new Map<string, CallInfo>();
  try {
    const logs = await client.getContractEvents({
      address: config.rollupsAddress as `0x${string}`,
      abi: rollupsAbi,
      eventName: "CrossChainCallExecuted",
      fromBlock,
      toBlock,
    });
    for (const log of logs) {
      const args = (log as any).args;
      if (args?.actionHash) {
        map.set(args.actionHash, {
          proxy: args.proxy ?? "",
          sourceAddress: args.sourceAddress ?? "",
        });
      }
    }
  } catch {
    // Non-critical — entries will just show empty addresses
  }
  return map;
}

/** Parse BatchPosted logs into BlockEntry[], skipping already-seen txs. Returns newest-first. */
function parseLogs(
  logs: any[],
  seenTxs: Set<string>,
  callInfoMap: Map<string, CallInfo>,
  isNew: boolean,
): BlockEntry[] {
  const results: BlockEntry[] = [];
  for (const log of logs) {
    const txHash = log.transactionHash ?? "";
    if (seenTxs.has(txHash)) continue;
    seenTxs.add(txHash);

    const entries = (log as any).args?.entries as ExecutionEntry[] | undefined;
    if (!entries) continue;

    const crossChainTxs = entries
      .map((e) => classifyEntry(e, callInfoMap))
      .filter((t): t is CrossChainTx => t !== null);
    if (crossChainTxs.length === 0) continue;

    results.push({
      l1BlockNumber: log.blockNumber!,
      l1TxHash: txHash,
      timestamp: null,
      crossChainTxs,
      l2BlockNumbers: [],
      isNew,
    });
  }
  // Return newest-first (logs come in ascending order from RPC)
  results.reverse();
  return results;
}

/** Fetch timestamps for blocks that don't have one yet (non-blocking). */
async function fillTimestamps(
  client: PublicClient,
  blocks: BlockEntry[],
  setBlocks: React.Dispatch<React.SetStateAction<BlockEntry[]>>,
) {
  const need = blocks.filter((b) => b.timestamp === null);
  if (need.length === 0) return;

  const unique = new Map<string, bigint>();
  for (const b of need)
    unique.set(b.l1BlockNumber.toString(), b.l1BlockNumber);

  const tsMap = new Map<string, number>();
  await Promise.all(
    [...unique.entries()].map(async ([key, num]) => {
      try {
        const block = await client.getBlock({ blockNumber: num });
        tsMap.set(key, Number(block.timestamp));
      } catch {
        /* non-critical */
      }
    }),
  );

  if (tsMap.size === 0) return;
  setBlocks((prev) =>
    prev.map((b) => {
      const ts = tsMap.get(b.l1BlockNumber.toString());
      return ts != null && b.timestamp === null ? { ...b, timestamp: ts } : b;
    }),
  );
}

/**
 * Fetch L2 block numbers for blocks by decoding postBatch calldata from L1 txs.
 * Same approach as blockLogDecoder.ts Phase 2.
 */
async function fillL2BlockNumbers(
  blocks: BlockEntry[],
  setBlocks: React.Dispatch<React.SetStateAction<BlockEntry[]>>,
) {
  const need = blocks.filter((b) => b.l2BlockNumbers.length === 0);
  if (need.length === 0) return;

  // Deduplicate by tx hash
  const unique = new Map<string, BlockEntry>();
  for (const b of need) unique.set(b.l1TxHash, b);

  const results = new Map<string, number[]>();
  await Promise.all(
    [...unique.entries()].map(async ([txHash]) => {
      try {
        const l1Tx = (await rpcCall(config.l1Rpc, "eth_getTransactionByHash", [txHash])) as { input: string } | null;
        if (!l1Tx?.input) return;
        const decoded = decodeFunctionData({ abi: rollupsAbi, data: l1Tx.input as `0x${string}` });
        if (decoded.functionName !== "postBatch") return;
        const callData = decoded.args[2] as `0x${string}`;
        if (!callData || callData === "0x" || callData.length <= 2) return;
        const [blockNumbers] = decodeAbiParameters(
          [{ type: "uint256[]" }, { type: "bytes[]" }],
          callData,
        );
        results.set(txHash, (blockNumbers as bigint[]).map((n) => Number(n)));
      } catch {
        /* decode failed — skip */
      }
    }),
  );

  if (results.size === 0) return;
  setBlocks((prev) =>
    prev.map((b) => {
      const nums = results.get(b.l1TxHash);
      return nums && b.l2BlockNumbers.length === 0 ? { ...b, l2BlockNumbers: nums } : b;
    }),
  );
}

/* ── Component ─────────────────────────────────────────────────────── */

export function LiveFeed({ onNavigateToBlock }: Props) {
  const [blocks, setBlocks] = useState<BlockEntry[]>([]);
  const [filter, setFilter] = useState<Filter>("all");
  const [status, setStatus] = useState<
    "connecting" | "live" | "error"
  >("connecting");
  const [errorMsg, setErrorMsg] = useState<string | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);
  const [reachedGenesis, setReachedGenesis] = useState(false);

  const clientRef = useRef<PublicClient | null>(null);
  const seenTxRef = useRef<Set<string>>(new Set());
  // Track the range we've scanned: [oldestScanned, newestScanned]
  const newestScannedRef = useRef<bigint>(0n);
  const oldestScannedRef = useRef<bigint>(0n);
  // Refs for guards — avoid stale closures in callbacks
  const loadingMoreRef = useRef(false);
  const reachedGenesisRef = useRef(false);
  const feedRef = useRef<HTMLDivElement>(null);

  /* ── Initial connect: fetch latest page ── */
  useEffect(() => {
    if (!config.l1Rpc || !config.rollupsAddress) {
      setStatus("error");
      setErrorMsg("Waiting for config (L1 RPC / Rollups address)...");
      return;
    }

    const client = createPublicClient({
      chain: { ...foundry, id: 31337 },
      transport: http(config.l1Rpc),
    });
    clientRef.current = client;

    let cancelled = false;

    const init = async () => {
      try {
        const latestBlock = await client.getBlockNumber();
        newestScannedRef.current = latestBlock;

        // Scan backwards in windows until we have PAGE_SIZE entries or hit genesis
        let cursor = latestBlock;
        const collected: BlockEntry[] = [];

        while (collected.length < PAGE_SIZE && cursor > 0n) {
          const from = cursor > SCAN_WINDOW ? cursor - SCAN_WINDOW : 0n;

          // Fetch BatchPosted + CrossChainCallExecuted in parallel
          const [batchLogs, callInfoMap] = await Promise.all([
            client.getContractEvents({
              address: config.rollupsAddress as `0x${string}`,
              abi: rollupsAbi,
              eventName: "BatchPosted",
              fromBlock: from,
              toBlock: cursor,
            }),
            fetchCallInfoMap(client, from, cursor),
          ]);

          const parsed = parseLogs(
            batchLogs,
            seenTxRef.current,
            callInfoMap,
            false,
          );
          collected.push(...parsed);

          if (from === 0n) {
            reachedGenesisRef.current = true;
            setReachedGenesis(true);
            break;
          }
          cursor = from - 1n;
        }

        oldestScannedRef.current = cursor > 0n ? cursor : 0n;
        if (!cancelled) {
          // Show all collected (seenTxRef stays in sync — no data gap)
          setBlocks(collected);
          setStatus("live");
          fillTimestamps(client, collected, setBlocks);
          fillL2BlockNumbers(collected, setBlocks);
        }
      } catch (err: any) {
        if (!cancelled) {
          setStatus("error");
          setErrorMsg(
            err?.message?.slice(0, 120) || "Failed to connect to L1",
          );
        }
      }
    };
    init();

    return () => {
      cancelled = true;
    };
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  /* ── Poll for new events (forward) ── */
  useEffect(() => {
    if (status !== "live") return;
    const client = clientRef.current;
    if (!client) return;

    const poll = async () => {
      try {
        const from = newestScannedRef.current + 1n;
        const latestBlock = await client.getBlockNumber();
        if (latestBlock < from) return;

        // Fetch BatchPosted + CrossChainCallExecuted in parallel
        const [batchLogs, callInfoMap] = await Promise.all([
          client.getContractEvents({
            address: config.rollupsAddress as `0x${string}`,
            abi: rollupsAbi,
            eventName: "BatchPosted",
            fromBlock: from,
            toBlock: latestBlock,
          }),
          fetchCallInfoMap(client, from, latestBlock),
        ]);

        newestScannedRef.current = latestBlock;

        const parsed = parseLogs(
          batchLogs,
          seenTxRef.current,
          callInfoMap,
          true,
        );
        if (parsed.length > 0) {
          setBlocks((prev) => [...parsed, ...prev]);
          fillTimestamps(client, parsed, setBlocks);
          fillL2BlockNumbers(parsed, setBlocks);
        }
      } catch {
        // Silent — next poll will retry
      }
    };

    // Run first poll immediately to catch events during initial scan
    poll();
    const timer = setInterval(poll, POLL_INTERVAL);
    return () => clearInterval(timer);
  }, [status]);

  /* ── Load more (backwards / older) ── */
  const loadMore = useCallback(async () => {
    const client = clientRef.current;
    if (!client || loadingMoreRef.current || reachedGenesisRef.current) return;

    loadingMoreRef.current = true;
    setLoadingMore(true);
    try {
      const cursor = oldestScannedRef.current;
      if (cursor <= 0n) {
        reachedGenesisRef.current = true;
        setReachedGenesis(true);
        return;
      }

      const collected: BlockEntry[] = [];
      let scanCursor = cursor;

      while (collected.length < PAGE_SIZE && scanCursor > 0n) {
        const from =
          scanCursor > SCAN_WINDOW ? scanCursor - SCAN_WINDOW : 0n;

        const [batchLogs, callInfoMap] = await Promise.all([
          client.getContractEvents({
            address: config.rollupsAddress as `0x${string}`,
            abi: rollupsAbi,
            eventName: "BatchPosted",
            fromBlock: from,
            toBlock: scanCursor,
          }),
          fetchCallInfoMap(client, from, scanCursor),
        ]);

        const parsed = parseLogs(
          batchLogs,
          seenTxRef.current,
          callInfoMap,
          false,
        );
        collected.push(...parsed);

        if (from === 0n) {
          reachedGenesisRef.current = true;
          setReachedGenesis(true);
          break;
        }
        scanCursor = from - 1n;
      }

      oldestScannedRef.current = scanCursor > 0n ? scanCursor : 0n;

      if (collected.length > 0) {
        setBlocks((prev) => [...prev, ...collected]);
        fillTimestamps(client, collected, setBlocks);
        fillL2BlockNumbers(collected, setBlocks);
      } else if (scanCursor <= 0n) {
        reachedGenesisRef.current = true;
        setReachedGenesis(true);
      }
    } finally {
      loadingMoreRef.current = false;
      setLoadingMore(false);
    }
  }, []); // No deps — uses only refs and stable state setters

  /* ── Tick for relative timestamps ── */
  const [, setTick] = useState(0);
  useEffect(() => {
    const t = setInterval(() => setTick((n) => n + 1), 5_000);
    return () => clearInterval(t);
  }, []);

  /* ── Derived state ── */
  const filtered =
    filter === "all"
      ? blocks
      : blocks.filter((b) => matchesFilter(b, filter));

  const filterCounts = {
    all: blocks.length,
    "l1-to-l2": blocks.filter((b) =>
      b.crossChainTxs.some((tx) => tx.direction === "L1\u2192L2"),
    ).length,
    "l2-to-l1": blocks.filter((b) =>
      b.crossChainTxs.some((tx) => tx.direction === "L2\u2192L1"),
    ).length,
  };

  return (
    <div className={styles.root}>
      {/* Status bar */}
      <div className={styles.statusBar}>
        <div className={styles.statusLeft}>
          {status === "live" && (
            <>
              <span className={styles.liveDot} />
              <span className={styles.liveLabel}>LIVE</span>
            </>
          )}
          {status === "connecting" && (
            <>
              <span className={styles.spinner} />
              <span className={styles.connectingLabel}>Connecting...</span>
            </>
          )}
          {status === "error" && (
            <span className={styles.errorLabel}>
              {errorMsg || "Error"}
            </span>
          )}
        </div>
        <span className={styles.statusRight}>
          {filtered.length} block{filtered.length !== 1 ? "s" : ""}
        </span>
      </div>

      {/* Filter buttons */}
      <div className={styles.filters}>
        {(
          [
            ["all", "All", filterCounts.all],
            ["l1-to-l2", "L1\u2192L2", filterCounts["l1-to-l2"]],
            ["l2-to-l1", "L2\u2192L1", filterCounts["l2-to-l1"]],
          ] as [Filter, string, number][]
        ).map(([key, label, count]) => (
          <button
            key={key}
            className={`${styles.filterBtn} ${filter === key ? styles.filterBtnActive : ""}`}
            onClick={() => setFilter(key)}
          >
            {label}
            <span className={styles.filterCount}>{count}</span>
          </button>
        ))}
      </div>

      {/* Loading skeleton */}
      {status === "connecting" && (
        <div className={styles.feed}>
          {[0, 1, 2].map((i) => (
            <div key={i} className={styles.skeleton}>
              <div className={styles.skelLine} style={{ width: "40%" }} />
              <div className={styles.skelLine} style={{ width: "70%" }} />
              <div className={styles.skelLine} style={{ width: "55%" }} />
            </div>
          ))}
        </div>
      )}

      {/* Empty state */}
      {filtered.length === 0 && status === "live" && (
        <div className={styles.empty}>
          {filter === "all"
            ? "No cross-chain transactions yet. Waiting for activity..."
            : `No ${filter === "l1-to-l2" ? "L1\u2192L2" : "L2\u2192L1"} transactions found.`}
        </div>
      )}

      {/* Feed — newest first */}
      <div className={styles.feed} ref={feedRef}>
        {filtered.map((block) => (
          <BlockCard
            key={block.l1TxHash}
            block={block}
            onNavigateToBlock={onNavigateToBlock}
          />
        ))}

        {/* Load more / end-of-list */}
        {status === "live" && filtered.length > 0 && (
          <div className={styles.loadMoreRow}>
            {reachedGenesis ? (
              <span className={styles.endLabel}>No older events</span>
            ) : (
              <button
                className={styles.loadMoreBtn}
                onClick={loadMore}
                disabled={loadingMore}
              >
                {loadingMore ? (
                  <>
                    <span className={styles.spinner} /> Loading...
                  </>
                ) : (
                  "Load older events"
                )}
              </button>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

/* ── BlockCard ─────────────────────────────────────────────────────── */

function BlockCard({
  block,
  onNavigateToBlock,
}: {
  block: BlockEntry;
  onNavigateToBlock: (blockNumber: number) => void;
}) {
  const directions = new Set(block.crossChainTxs.map((t) => t.direction));
  const dirLabel = [...directions].join(", ");

  return (
    <div
      className={`${styles.card} ${block.isNew ? styles.cardNew : ""}`}
    >
      <div className={styles.cardHeader}>
        <button
          className={styles.blockLink}
          onClick={() => onNavigateToBlock(Number(block.l1BlockNumber))}
          title="View in Block Explorer"
        >
          L1 #{block.l1BlockNumber.toString()}
        </button>
        {block.l2BlockNumbers.length > 0 && (
          <span className={styles.l2Badge}>
            L2 #{block.l2BlockNumbers.length === 1
              ? block.l2BlockNumbers[0]!.toLocaleString()
              : `${block.l2BlockNumbers[0]!.toLocaleString()}\u2013${block.l2BlockNumbers[block.l2BlockNumbers.length - 1]!.toLocaleString()}`}
          </span>
        )}
        <span className={styles.dirBadge}>{dirLabel}</span>
        <span className={styles.countBadge}>
          {block.crossChainTxs.length} tx
          {block.crossChainTxs.length !== 1 ? "s" : ""}
        </span>
        {block.timestamp != null && (
          <span className={styles.timestamp}>
            {timeAgo(block.timestamp)}
          </span>
        )}
      </div>

      <div className={styles.txList}>
        {block.crossChainTxs.map((tx, i) => (
          <div key={`${tx.actionHash}-${i}`} className={styles.txRow}>
            <span
              className={`${styles.typeBadge} ${
                tx.direction === "L1\u2192L2"
                  ? styles.typeBadgeL1
                  : styles.typeBadgeL2
              }`}
            >
              {tx.actionType}
            </span>
            <span className={styles.txAddr}>
              {tx.destination ? (
                <a
                  className={styles.addrLink}
                  href={`${config.l1Explorer}/address/${tx.destination}`}
                  target="_blank"
                  rel="noopener noreferrer"
                  title={tx.destination}
                >
                  {truncateHex(tx.destination)}
                </a>
              ) : (
                "\u2014"
              )}
            </span>
            <span className={styles.arrow}>{tx.direction}</span>
            {tx.sourceAddress && (
              <span className={styles.source}>
                from{" "}
                <a
                  className={styles.addrLink}
                  href={`${config.l1Explorer}/address/${tx.sourceAddress}`}
                  target="_blank"
                  rel="noopener noreferrer"
                  title={tx.sourceAddress}
                >
                  {truncateHex(tx.sourceAddress)}
                </a>
              </span>
            )}
          </div>
        ))}
      </div>

      <div className={styles.cardFooter}>
        <button
          className={styles.footerBtn}
          onClick={() => onNavigateToBlock(Number(block.l1BlockNumber))}
          title="Open this block in the Block Explorer tab"
        >
          <svg
            width="12"
            height="12"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="2"
            strokeLinecap="round"
            strokeLinejoin="round"
          >
            <rect x="3" y="3" width="18" height="18" rx="2" ry="2" />
            <line x1="3" y1="9" x2="21" y2="9" />
            <line x1="9" y1="21" x2="9" y2="9" />
          </svg>
          View in Block Explorer
        </button>
        <span className={styles.txHash} title={block.l1TxHash}>
          {truncateHex(block.l1TxHash)}
        </span>
      </div>
    </div>
  );
}
