import type { EventRecord } from "../types/events";
import type { TransactionBundle, BundleDirection } from "../types/visualization";
import { actionFromEventArgs, computeActionHash } from "./actionHashDecoder";

export type CorrelatedPair = {
  actionHash: string;
  l1Event: EventRecord;
  l2Event: EventRecord;
};

export function findCorrelatedPairs(events: EventRecord[]): CorrelatedPair[] {
  const l1Consumed = new Map<string, EventRecord>();
  const l2Consumed = new Map<string, EventRecord>();
  const pairs: CorrelatedPair[] = [];

  for (const event of events) {
    if (event.eventName !== "ExecutionConsumed") continue;
    const hash = event.args.actionHash as string;
    if (!hash) continue;

    if (event.chain === "l1") {
      l1Consumed.set(hash, event);
      const match = l2Consumed.get(hash);
      if (match) {
        pairs.push({ actionHash: hash, l1Event: event, l2Event: match });
      }
    } else {
      l2Consumed.set(hash, event);
      const match = l1Consumed.get(hash);
      if (match) {
        pairs.push({ actionHash: hash, l1Event: match, l2Event: event });
      }
    }
  }

  return pairs;
}

export type CorrelatedEntry = {
  actionHash: string;
  l1EventId: string;
  l2EventId: string;
};

export function findCorrelatedEntries(events: EventRecord[]): CorrelatedEntry[] {
  const l1Hashes = new Map<string, string>();
  const l2Hashes = new Map<string, string>();
  const correlations: CorrelatedEntry[] = [];

  for (const event of events) {
    if (event.eventName === "BatchPosted" && event.chain === "l1") {
      const entries = event.args.entries as Array<{ actionHash: string }>;
      if (entries) {
        for (const entry of entries) {
          if (
            entry.actionHash !==
            "0x0000000000000000000000000000000000000000000000000000000000000000"
          ) {
            l1Hashes.set(entry.actionHash, event.id);
          }
        }
      }
    }
    if (event.eventName === "ExecutionTableLoaded" && event.chain === "l2") {
      const entries = event.args.entries as Array<{ actionHash: string }>;
      if (entries) {
        for (const entry of entries) {
          l2Hashes.set(entry.actionHash, event.id);
        }
      }
    }
  }

  for (const [hash, l1EventId] of l1Hashes) {
    const l2EventId = l2Hashes.get(hash);
    if (l2EventId) {
      correlations.push({ actionHash: hash, l1EventId, l2EventId });
    }
  }

  return correlations;
}

export function buildTransactionBundles(events: EventRecord[]): TransactionBundle[] {
  const hashToEvents = new Map<string, Set<string>>();
  const eventToHashes = new Map<string, Set<string>>();

  const ZERO_HASH = "0x0000000000000000000000000000000000000000000000000000000000000000";

  function link(eventId: string, actionHash: string) {
    if (!actionHash || actionHash === ZERO_HASH) return;
    const h = actionHash.toLowerCase();
    if (!hashToEvents.has(h)) hashToEvents.set(h, new Set());
    hashToEvents.get(h)!.add(eventId);
    if (!eventToHashes.has(eventId)) eventToHashes.set(eventId, new Set());
    eventToHashes.get(eventId)!.add(h);
  }

  for (const event of events) {
    switch (event.eventName) {
      case "BatchPosted": {
        const entries = event.args.entries as Array<{ actionHash: string; nextAction?: Record<string, unknown> }> | undefined;
        if (entries) {
          for (const entry of entries) {
            link(event.id, entry.actionHash);
            if (entry.nextAction) {
              try {
                const fields = actionFromEventArgs(entry.nextAction);
                const nextHash = computeActionHash(fields);
                link(event.id, nextHash);
              } catch { /* skip */ }
            }
          }
        }
        break;
      }
      case "ExecutionTableLoaded": {
        const entries = event.args.entries as Array<{ actionHash: string; nextAction?: Record<string, unknown> }> | undefined;
        if (entries) {
          for (const entry of entries) {
            link(event.id, entry.actionHash);
            if (entry.nextAction) {
              try {
                const fields = actionFromEventArgs(entry.nextAction);
                const nextHash = computeActionHash(fields);
                link(event.id, nextHash);
              } catch { /* skip */ }
            }
          }
        }
        break;
      }
      case "ExecutionConsumed":
        link(event.id, event.args.actionHash as string);
        break;
      case "CrossChainCallExecuted":
        link(event.id, event.args.actionHash as string);
        break;
      case "L2TXExecuted":
        link(event.id, event.args.actionHash as string);
        break;
      case "IncomingCrossChainCallExecuted":
        link(event.id, event.args.actionHash as string);
        break;
    }
  }

  const parent = new Map<string, string>();
  function find(x: string): string {
    if (!parent.has(x)) parent.set(x, x);
    while (parent.get(x) !== x) {
      parent.set(x, parent.get(parent.get(x)!)!);
      x = parent.get(x)!;
    }
    return x;
  }
  function union(a: string, b: string) {
    const ra = find(a), rb = find(b);
    if (ra !== rb) parent.set(ra, rb);
  }

  for (const [, eventIds] of hashToEvents) {
    const arr = [...eventIds];
    for (let i = 1; i < arr.length; i++) {
      union(arr[0]!, arr[i]!);
    }
  }

  const groups = new Map<string, Set<string>>();
  for (const eventId of eventToHashes.keys()) {
    const root = find(eventId);
    if (!groups.has(root)) groups.set(root, new Set());
    groups.get(root)!.add(eventId);
  }

  const eventMap = new Map(events.map((e) => [e.id, e]));
  const bundledEventIds = new Set(eventToHashes.keys());

  const bundles: TransactionBundle[] = [];
  let bundleIdx = 0;

  for (const [, eventIds] of groups) {
    const groupEvents = [...eventIds]
      .map((id) => eventMap.get(id))
      .filter((e): e is EventRecord => e !== undefined)
      .sort((a, b) => {
        if (a.blockNumber !== b.blockNumber) return a.blockNumber < b.blockNumber ? -1 : 1;
        return a.logIndex - b.logIndex;
      });

    if (groupEvents.length === 0) continue;

    const allHashes = new Set<string>();
    for (const eid of eventIds) {
      const hashes = eventToHashes.get(eid);
      if (hashes) hashes.forEach((h) => allHashes.add(h));
    }

    const chains = new Set<"l1" | "l2">();
    const txHashes = new Set<string>();
    let minBlock = groupEvents[0]!.blockNumber;
    let maxBlock = groupEvents[0]!.blockNumber;
    for (const e of groupEvents) {
      chains.add(e.chain);
      txHashes.add(e.transactionHash);
      if (e.blockNumber < minBlock) minBlock = e.blockNumber;
      if (e.blockNumber > maxBlock) maxBlock = e.blockNumber;
    }

    const direction = inferDirection(groupEvents);

    const consumedHashes = new Set<string>();
    for (const e of groupEvents) {
      if (e.eventName === "ExecutionConsumed") {
        consumedHashes.add((e.args.actionHash as string).toLowerCase());
      }
    }
    const loadedHashes = new Set<string>();
    for (const e of groupEvents) {
      if (e.eventName === "BatchPosted") {
        const entries = e.args.entries as Array<{ actionHash: string }> | undefined;
        if (entries) entries.forEach((en) => {
          if (en.actionHash !== ZERO_HASH) loadedHashes.add(en.actionHash.toLowerCase());
        });
      }
      if (e.eventName === "ExecutionTableLoaded") {
        const entries = e.args.entries as Array<{ actionHash: string }> | undefined;
        if (entries) entries.forEach((en) => loadedHashes.add(en.actionHash.toLowerCase()));
      }
    }
    const allConsumed = loadedHashes.size > 0 && [...loadedHashes].every((h) => consumedHashes.has(h));
    const status = allConsumed ? "complete" : "in-progress";

    const title = generateBundleTitle(groupEvents, direction);

    bundles.push({
      id: `bundle-${bundleIdx++}`,
      direction,
      title,
      actionHashes: [...allHashes],
      events: groupEvents.map((e) => e.id),
      chains,
      blockRange: { from: minBlock, to: maxBlock },
      txHashes,
      status,
    });
  }

  for (const event of events) {
    if (bundledEventIds.has(event.id)) continue;
    bundles.push({
      id: `bundle-${bundleIdx++}`,
      direction: event.chain === "l1" ? "L1" : "L2",
      title: event.eventName,
      actionHashes: [],
      events: [event.id],
      chains: new Set([event.chain]),
      blockRange: { from: event.blockNumber, to: event.blockNumber },
      txHashes: new Set([event.transactionHash]),
      status: "complete",
    });
  }

  bundles.sort((a, b) => {
    if (a.blockRange.from !== b.blockRange.from) return a.blockRange.from < b.blockRange.from ? -1 : 1;
    return 0;
  });

  return bundles;
}

function inferDirection(events: EventRecord[]): BundleDirection {
  const actionEvents = events.filter((e) =>
    e.eventName === "ExecutionConsumed" ||
    e.eventName === "CrossChainCallExecuted" ||
    e.eventName === "L2TXExecuted" ||
    e.eventName === "IncomingCrossChainCallExecuted"
  );

  if (actionEvents.length === 0) {
    const chains = new Set(events.map((e) => e.chain));
    if (chains.size === 1) return chains.has("l1") ? "L1" : "L2";
    return "mixed";
  }

  const seq: string[] = [];
  for (const e of actionEvents) {
    if (seq.length === 0 || seq[seq.length - 1] !== e.chain) {
      seq.push(e.chain);
    }
  }

  if (seq.length === 1) return seq[0] === "l1" ? "L1" : "L2";
  if (seq.length === 2) {
    if (seq[0] === "l1" && seq[1] === "l2") return "L1->L2";
    if (seq[0] === "l2" && seq[1] === "l1") return "L2->L1";
  }
  if (seq.length === 3) {
    if (seq[0] === "l1" && seq[1] === "l2" && seq[2] === "l1") return "L1->L2->L1";
    if (seq[0] === "l2" && seq[1] === "l1" && seq[2] === "l2") return "L2->L1->L2";
  }
  return "mixed";
}

function generateBundleTitle(events: EventRecord[], direction: BundleDirection): string {
  const ccCall = events.find((e) => e.eventName === "CrossChainCallExecuted");
  if (ccCall) {
    const src = ccCall.args.sourceAddress as string;
    const proxy = ccCall.args.proxy as string;
    return `Cross-chain call ${src?.slice(0, 8)}... via proxy ${proxy?.slice(0, 8)}...`;
  }

  const l2tx = events.find((e) => e.eventName === "L2TXExecuted");
  if (l2tx) return `L2TX execution (rollup ${String(l2tx.args.rollupId)})`;

  const incoming = events.find((e) => e.eventName === "IncomingCrossChainCallExecuted");
  if (incoming) return `Incoming cross-chain call to ${(incoming.args.destination as string)?.slice(0, 8)}...`;

  const batch = events.find((e) => e.eventName === "BatchPosted");
  const load = events.find((e) => e.eventName === "ExecutionTableLoaded");
  if (batch && load) return `${direction} cross-chain batch`;
  if (batch) return "Batch posted";
  if (load) return "Execution table loaded";

  return `${direction} transaction`;
}
