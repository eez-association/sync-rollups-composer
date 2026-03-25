/**
 * Block log decoder — fetches and decodes Rollups.sol events from L1 blocks.
 * Matches the output format of DecodeExecutions.s.sol forge script.
 */

import { decodeEventLog, decodeFunctionData, decodeAbiParameters } from "viem";
import { rollupsAbi } from "../abi/rollups";
import { config } from "../config";
import { rpcCall } from "../rpc";
import type { Action, ExecutionEntry, L2BlockInfo, L2TxInfo } from "../types/chain";
import { actionTypeName, truncateAddress, truncateHex } from "./actionFormatter";
import { actionFromEventArgs } from "./actionHashDecoder";

// ─── Types ───

const KNOWN_SELECTORS: Record<string, string> = {
  // User contracts
  "0xd09de08a": "increment()",
  "0x06661abd": "counter()",
  "0x5a6a9e05": "targetCounter()",
  "0x2bf21647": "incrementProxy()",
  // L2Context
  "0xe9d68d7b": "setContext()",
  // CrossChainManagerL2
  "0x96609ad5": "loadExecutionTable()",
  "0x0f64c845": "executeIncomingCrossChainCall()",
  "0x9af53259": "executeCrossChainCall()",
  "0x92f6fe4a": "newScope()",
  // CrossChainProxy
  "0x532f0839": "executeOnBehalf()",
};

export type TxTrigger =
  | {
      type: "cross-chain";
      actionHash: string;
      proxy: string;
      sourceAddress: string;
      callData: string;
      value: bigint;
    }
  | { type: "l2tx"; actionHash: string; rollupId: bigint; rlpData: string };

export type FlowStep = {
  consumed: Action;
  consumedHash: string;
  matchedEntry: ExecutionEntry | null;
};

export type DecodedTx = {
  txHash: string;
  batchEntries: ExecutionEntry[];
  trigger: TxTrigger | null;
  consumed: { actionHash: string; action: Action }[];
  flowSteps: FlowStep[];
  summary: string;
  l2BlockNumbers: number[];
};

export type DecodedBlock = {
  blockNumber: number;
  totalLogs: number;
  txs: DecodedTx[];
  allBatchEntries: ExecutionEntry[];
  timestamp: number | null;
  l2Blocks: L2BlockInfo[];
};

// ─── Raw log type from RPC ───

type RawLog = {
  address: string;
  topics: string[];
  data: string;
  transactionHash: string;
  blockNumber: string;
  logIndex: string;
};

// ─── Decoded event holder ───

type DecodedEvent = {
  txHash: string;
  logIndex: number;
  eventName: string;
  args: Record<string, unknown>;
};

// ─── Core functions ───

function toHex(n: number): string {
  return "0x" + n.toString(16);
}

function decodeLog(log: RawLog): DecodedEvent | null {
  try {
    const decoded = decodeEventLog({
      abi: rollupsAbi,
      data: log.data as `0x${string}`,
      topics: log.topics as [`0x${string}`, ...`0x${string}`[]],
    });
    return {
      txHash: log.transactionHash,
      logIndex: parseInt(log.logIndex, 16),
      eventName: decoded.eventName,
      args: decoded.args as unknown as Record<string, unknown>,
    };
  } catch {
    return null;
  }
}

function actionFromTuple(tuple: Record<string, unknown>): Action {
  const fields = actionFromEventArgs(tuple);
  return fields as Action;
}

function entryFromTuple(tuple: Record<string, unknown>): ExecutionEntry {
  const stateDeltas = (tuple.stateDeltas as Record<string, unknown>[]).map(
    (sd) => ({
      rollupId: BigInt(sd.rollupId as bigint),
      currentState: sd.currentState as `0x${string}`,
      newState: sd.newState as `0x${string}`,
      etherDelta: BigInt(sd.etherDelta as bigint),
    }),
  );
  return {
    stateDeltas,
    actionHash: tuple.actionHash as `0x${string}`,
    nextAction: actionFromTuple(tuple.nextAction as Record<string, unknown>),
  };
}

function buildFlowSteps(
  consumed: { actionHash: string; action: Action }[],
  allBatchEntries: ExecutionEntry[],
): FlowStep[] {
  return consumed.map((c) => {
    const matchedEntry =
      allBatchEntries.find(
        (e) => e.actionHash.toLowerCase() === c.actionHash.toLowerCase(),
      ) ?? null;
    return {
      consumed: c.action,
      consumedHash: c.actionHash,
      matchedEntry,
    };
  });
}

function selectorName(data: string): string | null {
  if (!data || data.length < 10) return null;
  const sel = data.slice(0, 10).toLowerCase();
  return KNOWN_SELECTORS[sel] ?? null;
}

function tinyAction(action: Action): string {
  const type = actionTypeName(action.actionType);
  const dest = truncateAddress(action.destination);

  if (action.actionType === 0) {
    // CALL
    const fn = selectorName(action.data) ?? truncateHex(action.data, 4);
    return `CALL(${dest}.${fn})`;
  }
  if (action.actionType === 1) {
    // RESULT
    const ok = action.failed ? "fail" : "ok";
    const dataStr =
      action.data === "0x" ? "" : `,${truncateHex(action.data, 4)}`;
    return `RESULT(${ok}${dataStr})`;
  }
  if (action.actionType === 2) {
    // L2TX
    return `L2TX(r${action.rollupId.toString()})`;
  }
  return `${type}(${dest})`;
}

function buildSummary(
  trigger: TxTrigger | null,
  flowSteps: FlowStep[],
  batchEntries: ExecutionEntry[],
): string {
  const parts: string[] = [];

  // If we have a trigger, show it first
  if (trigger) {
    if (trigger.type === "cross-chain") {
      const fn = selectorName(trigger.callData) ?? truncateHex(trigger.callData, 4);
      parts.push(`CrossChain(${truncateAddress(trigger.proxy)}.${fn})`);
    } else {
      parts.push(`L2TX(r${trigger.rollupId.toString()})`);
    }
  }

  // Add consumed action flow
  for (const step of flowSteps) {
    parts.push(tinyAction(step.consumed));
    if (step.matchedEntry) {
      const next = step.matchedEntry.nextAction;
      // Only add next if it's not a zero-action (immediate entries)
      if (
        next.actionType !== 0 ||
        next.destination !== ("0x" + "0".repeat(40))
      ) {
        parts.push(tinyAction(next));
      }
    }
  }

  // If only batch entries (no trigger/consumed), summarize entries
  if (parts.length === 0 && batchEntries.length > 0) {
    const zeroHash = "0x" + "0".repeat(64);
    const imm = batchEntries.filter(
      (e) => e.actionHash.toLowerCase() === zeroHash,
    ).length;
    const def = batchEntries.length - imm;
    const entryParts: string[] = [];
    if (imm > 0) entryParts.push(`${imm} IMMEDIATE`);
    if (def > 0) entryParts.push(`${def} DEFERRED`);
    return `BatchPosted(${entryParts.join(" + ")})`;
  }

  return parts.join(" -> ") || "No events";
}

// ─── Public API ───

export async function fetchBlockLogs(
  blockNumber: number,
): Promise<DecodedBlock> {
  const hex = toHex(blockNumber);

  // Fetch logs and block header in parallel
  const [logs, blockHeader] = await Promise.all([
    rpcCall(config.l1Rpc, "eth_getLogs", [
      {
        address: config.rollupsAddress,
        fromBlock: hex,
        toBlock: hex,
      },
    ]) as Promise<RawLog[]>,
    rpcCall(config.l1Rpc, "eth_getBlockByNumber", [hex, false]).catch(
      () => null,
    ) as Promise<{ timestamp: string } | null>,
  ]);
  const timestamp = blockHeader?.timestamp
    ? parseInt(blockHeader.timestamp, 16)
    : null;

  // Decode all logs
  const decoded: DecodedEvent[] = [];
  for (const log of logs) {
    const d = decodeLog(log);
    if (d) decoded.push(d);
  }

  // Group by txHash, maintaining order
  const txOrder: string[] = [];
  const txMap = new Map<string, DecodedEvent[]>();
  for (const ev of decoded) {
    if (!txMap.has(ev.txHash)) {
      txOrder.push(ev.txHash);
      txMap.set(ev.txHash, []);
    }
    txMap.get(ev.txHash)!.push(ev);
  }

  // First pass: collect ALL batch entries across all txs
  const allBatchEntries: ExecutionEntry[] = [];
  for (const events of txMap.values()) {
    for (const ev of events) {
      if (ev.eventName === "BatchPosted") {
        const entries = ev.args.entries as Record<string, unknown>[];
        for (const e of entries) {
          allBatchEntries.push(entryFromTuple(e));
        }
      }
    }
  }

  // Second pass: build DecodedTx per tx
  const txs: DecodedTx[] = [];
  for (const txHash of txOrder) {
    const events = txMap.get(txHash)!;
    const batchEntries: ExecutionEntry[] = [];
    let trigger: TxTrigger | null = null;
    const consumed: { actionHash: string; action: Action }[] = [];

    for (const ev of events) {
      if (ev.eventName === "BatchPosted") {
        const entries = ev.args.entries as Record<string, unknown>[];
        for (const e of entries) {
          batchEntries.push(entryFromTuple(e));
        }
      } else if (ev.eventName === "CrossChainCallExecuted") {
        trigger = {
          type: "cross-chain",
          actionHash: ev.args.actionHash as string,
          proxy: ev.args.proxy as string,
          sourceAddress: ev.args.sourceAddress as string,
          callData: ev.args.callData as string,
          value: BigInt(ev.args.value as bigint),
        };
      } else if (ev.eventName === "L2TXExecuted") {
        trigger = {
          type: "l2tx",
          actionHash: ev.args.actionHash as string,
          rollupId: BigInt(ev.args.rollupId as bigint),
          rlpData: ev.args.rlpEncodedTx as string,
        };
      } else if (ev.eventName === "ExecutionConsumed") {
        const action = actionFromTuple(
          ev.args.action as Record<string, unknown>,
        );
        consumed.push({
          actionHash: ev.args.actionHash as string,
          action,
        });
      }
    }

    const flowSteps = buildFlowSteps(consumed, allBatchEntries);
    const summary = buildSummary(trigger, flowSteps, batchEntries);

    txs.push({ txHash, batchEntries, trigger, consumed, flowSteps, summary, l2BlockNumbers: [] });
  }

  // ─── Phase 2: Fetch L1 tx calldata for postBatch txs, decode L2 block numbers ───
  const batchTxHashes = txs
    .filter((tx) => tx.batchEntries.length > 0)
    .map((tx) => tx.txHash);

  if (batchTxHashes.length > 0) {
    const l1Txs = await Promise.all(
      batchTxHashes.map((hash) =>
        rpcCall(config.l1Rpc, "eth_getTransactionByHash", [hash]).catch(() => null) as Promise<{ input: string } | null>
      ),
    );

    for (let i = 0; i < batchTxHashes.length; i++) {
      const l1Tx = l1Txs[i];
      if (!l1Tx?.input) continue;
      try {
        const decoded = decodeFunctionData({ abi: rollupsAbi, data: l1Tx.input as `0x${string}` });
        if (decoded.functionName !== "postBatch") continue;
        const callData = decoded.args[2] as `0x${string}`;
        if (!callData || callData === "0x" || callData.length <= 2) continue;
        const [blockNumbers] = decodeAbiParameters(
          [{ type: "uint256[]" }, { type: "bytes[]" }],
          callData,
        );
        const nums = (blockNumbers as bigint[]).map((n) => Number(n));
        const tx = txs.find((t) => t.txHash === batchTxHashes[i]);
        if (tx) tx.l2BlockNumbers = nums;
      } catch {
        // Calldata decode failed — skip
      }
    }
  }

  // ─── Phase 3: Fetch L2 blocks ───
  const allL2BlockNumbers = [...new Set(txs.flatMap((tx) => tx.l2BlockNumbers))].sort((a, b) => a - b);
  let l2Blocks: L2BlockInfo[] = [];

  if (allL2BlockNumbers.length > 0) {
    const l2BlockResults = await Promise.all(
      allL2BlockNumbers.map((num) =>
        rpcCall(config.l2Rpc, "eth_getBlockByNumber", [toHex(num), true]).catch(() => null),
      ),
    );

    for (let i = 0; i < allL2BlockNumbers.length; i++) {
      const blockNum = allL2BlockNumbers[i]!;
      const raw = l2BlockResults[i] as Record<string, unknown> | null;
      if (!raw) {
        l2Blocks.push({
          number: blockNum,
          timestamp: 0,
          hash: "",
          parentHash: "",
          stateRoot: "",
          gasUsed: 0,
          gasLimit: 0,
          txCount: -1,
          transactions: [],
        });
        continue;
      }
      const rawTxs = (raw.transactions || []) as Record<string, unknown>[];
      // Detect protocol txs: first tx's from address is the builder
      const builderAddr = rawTxs.length > 0 ? (rawTxs[0]!.from as string)?.toLowerCase() : "";
      const transactions: L2TxInfo[] = rawTxs.map((tx) => ({
        hash: tx.hash as string,
        from: tx.from as string,
        to: (tx.to as string) || null,
        value: tx.value as string,
        data: tx.input as string,
        isProtocol: (tx.from as string)?.toLowerCase() === builderAddr,
      }));
      l2Blocks.push({
        number: parseInt(raw.number as string, 16),
        timestamp: parseInt(raw.timestamp as string, 16),
        hash: raw.hash as string,
        parentHash: raw.parentHash as string,
        stateRoot: raw.stateRoot as string,
        gasUsed: parseInt(raw.gasUsed as string, 16),
        gasLimit: parseInt(raw.gasLimit as string, 16),
        txCount: rawTxs.length,
        transactions,
      });
    }
  }

  return {
    blockNumber,
    totalLogs: logs.length,
    txs,
    allBatchEntries,
    timestamp,
    l2Blocks,
  };
}

/** Fetch a single L2 block by number from the L2 RPC. */
export async function fetchL2Block(blockNumber: number): Promise<L2BlockInfo> {
  const hex = toHex(blockNumber);
  const raw = (await rpcCall(config.l2Rpc, "eth_getBlockByNumber", [hex, true])) as Record<string, unknown> | null;

  if (!raw) {
    return {
      number: blockNumber,
      timestamp: 0,
      hash: "",
      parentHash: "",
      stateRoot: "",
      gasUsed: 0,
      gasLimit: 0,
      txCount: -1,
      transactions: [],
    };
  }

  const rawTxs = (raw.transactions || []) as Record<string, unknown>[];
  // Detect protocol txs: first tx's from address is the builder
  const builderAddr =
    rawTxs.length > 0 ? (rawTxs[0]!.from as string)?.toLowerCase() : "";
  const transactions: L2TxInfo[] = rawTxs.map((tx) => ({
    hash: tx.hash as string,
    from: tx.from as string,
    to: (tx.to as string) || null,
    value: tx.value as string,
    data: tx.input as string,
    isProtocol: (tx.from as string)?.toLowerCase() === builderAddr,
  }));

  return {
    number: parseInt(raw.number as string, 16),
    timestamp: parseInt(raw.timestamp as string, 16),
    hash: raw.hash as string,
    parentHash: raw.parentHash as string,
    stateRoot: raw.stateRoot as string,
    gasUsed: parseInt(raw.gasUsed as string, 16),
    gasLimit: parseInt(raw.gasLimit as string, 16),
    txCount: rawTxs.length,
    transactions,
  };
}

/** setContext(uint256,bytes32,uint256,uint256) selector */
const SET_CONTEXT_SELECTOR = "0xe9d68d7b";

/**
 * Extract the L1 block number from an L2 block's first transaction (setContext call).
 * Returns null if the block has no setContext tx or can't be parsed.
 */
export function extractL1BlockFromL2(block: L2BlockInfo): number | null {
  if (block.transactions.length === 0) return null;
  const firstTx = block.transactions[0]!;
  if (!firstTx.data || firstTx.data.length < 74) return null; // 10 (selector) + 64 (uint256)
  if (firstTx.data.slice(0, 10).toLowerCase() !== SET_CONTEXT_SELECTOR) return null;
  const l1BlockHex = firstTx.data.slice(10, 74);
  const l1Block = parseInt(l1BlockHex, 16);
  return isNaN(l1Block) ? null : l1Block;
}

/**
 * Find the L1 block that posted a given L2 block via postBatch.
 * The builder builds L2 block N against L1 parent P (setContext.l1BlockNumber = P),
 * then submits postBatch in the next L1 block (P+1). We try P+1 first,
 * then a few nearby offsets. Fast: at most 2 RPC calls for the common case.
 */
export async function findL1BlockForL2Block(l2BlockNum: number): Promise<number | null> {
  const l2Block = await fetchL2Block(l2BlockNum);
  const l1Parent = extractL1BlockFromL2(l2Block);
  if (l1Parent == null) return null;
  // postBatch lands in the next L1 block after the parent (most common),
  // or occasionally +2/+3 if there was a delay.
  return l1Parent + 1;
}

/**
 * Find the latest L2 block that has been posted to L1 via postBatch.
 * Returns { l1Block, l2Block } so the caller can display the L1 block directly.
 * Uses findLatestEventBlock + fetchBlockLogs — same data path as L1 "Latest".
 */
export async function findLatestPostedL2Block(): Promise<{ l1Block: number; l2Block: number } | null> {
  const latestL1 = await findLatestEventBlock();
  if (latestL1 == null) return null;

  const decoded = await fetchBlockLogs(latestL1);

  // Extract highest L2 block number from postBatch calldata
  let maxL2 = -1;
  for (const tx of decoded.txs) {
    for (const num of tx.l2BlockNumbers) {
      if (num > maxL2) maxL2 = num;
    }
  }

  if (maxL2 < 0) return null;
  return { l1Block: latestL1, l2Block: maxL2 };
}

export async function findLatestEventBlock(): Promise<number | null> {
  const currentHex = (await rpcCall(
    config.l1Rpc,
    "eth_blockNumber",
    [],
  )) as string;
  const current = parseInt(currentHex, 16);

  // Try last 50 blocks first
  for (const range of [50, 200]) {
    const from = Math.max(0, current - range);
    const logs = (await rpcCall(config.l1Rpc, "eth_getLogs", [
      {
        address: config.rollupsAddress,
        fromBlock: toHex(from),
        toBlock: toHex(current),
      },
    ])) as RawLog[];
    if (logs.length > 0) {
      // Return the highest block number
      let max = 0;
      for (const log of logs) {
        const bn = parseInt(log.blockNumber, 16);
        if (bn > max) max = bn;
      }
      return max;
    }
  }
  return null;
}
