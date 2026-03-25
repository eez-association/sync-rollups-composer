import { useMemo } from "react";
import { useMonitorStore } from "../store";
import type { TableEntry } from "../types/visualization";
import { processEventForTables, extractRollupState } from "../lib/eventProcessor";
import { truncateHex } from "../lib/actionFormatter";
import type { EventRecord } from "../types/events";

function computeActiveForEvent(
  event: EventRecord,
  l1Manager: string,
  l2Manager: string,
): { activeNodes: string[]; activeEdges: string[] } {
  const activeNodes: string[] = [];
  const activeEdges: string[] = [];
  const args = event.args;

  const managerAddr =
    event.chain === "l1"
      ? l1Manager.toLowerCase()
      : l2Manager.toLowerCase();

  switch (event.eventName) {
    case "BatchPosted":
      if (managerAddr) activeNodes.push(managerAddr);
      activeNodes.push("__prover__");
      activeEdges.push(`__prover__->${managerAddr}`);
      break;

    case "ExecutionTableLoaded":
      if (managerAddr) activeNodes.push(managerAddr);
      activeNodes.push("__system__");
      activeEdges.push(`__system__->${managerAddr}`);
      break;

    case "CrossChainCallExecuted": {
      const proxy = (args.proxy as string)?.toLowerCase();
      const src = (args.sourceAddress as string)?.toLowerCase();
      if (src) activeNodes.push(src);
      if (proxy) activeNodes.push(proxy);
      if (managerAddr) activeNodes.push(managerAddr);
      if (src && proxy) activeEdges.push(`${src}->${proxy}`);
      if (proxy && managerAddr) activeEdges.push(`${proxy}->${managerAddr}`);
      break;
    }

    case "IncomingCrossChainCallExecuted": {
      const dest = (args.destination as string)?.toLowerCase();
      if (managerAddr) activeNodes.push(managerAddr);
      if (dest) activeNodes.push(dest);
      if (managerAddr && dest) activeEdges.push(`${managerAddr}->${dest}`);
      break;
    }

    case "CrossChainProxyCreated": {
      const proxy = (args.proxy as string)?.toLowerCase();
      if (proxy) activeNodes.push(proxy);
      if (managerAddr) activeNodes.push(managerAddr);
      if (proxy && managerAddr) activeEdges.push(`${proxy}->${managerAddr}`);
      break;
    }

    case "ExecutionConsumed":
    case "L2ExecutionPerformed":
    case "RollupCreated":
    case "StateUpdated":
      if (managerAddr) activeNodes.push(managerAddr);
      break;

    default:
      if (managerAddr) activeNodes.push(managerAddr);
      break;
  }

  return { activeNodes, activeEdges };
}

export function useDerivedState() {
  const events = useMonitorStore((s) => s.events);
  const selectedEventId = useMonitorStore((s) => s.selectedEventId);
  const l1Table = useMonitorStore((s) => s.l1Table);
  const l2Table = useMonitorStore((s) => s.l2Table);
  const contractState = useMonitorStore((s) => s.contractState);
  const storeActiveNodes = useMonitorStore((s) => s.activeNodes);
  const storeActiveEdges = useMonitorStore((s) => s.activeEdges);
  const l1ContractAddress = useMonitorStore((s) => s.l1ContractAddress);
  const l2ContractAddress = useMonitorStore((s) => s.l2ContractAddress);

  return useMemo(() => {
    if (selectedEventId === null) {
      return {
        l1Table,
        l2Table,
        contractState,
        eventsUpTo: events,
        activeNodes: storeActiveNodes,
        activeEdges: storeActiveEdges,
      };
    }

    const selectedIdx = events.findIndex((e) => e.id === selectedEventId);
    if (selectedIdx === -1) {
      return {
        l1Table,
        l2Table,
        contractState,
        eventsUpTo: events,
        activeNodes: storeActiveNodes,
        activeEdges: storeActiveEdges,
      };
    }

    const eventsUpTo = events.slice(0, selectedIdx + 1);
    const replayL1: TableEntry[] = [];
    const replayL2: TableEntry[] = [];
    const replayState: Record<string, string> = {};

    for (let i = 0; i <= selectedIdx; i++) {
      const event = events[i]!;
      const isCurrent = i === selectedIdx;
      const mutations = processEventForTables(event);

      for (const e of replayL1) {
        if (e.status === "ja") e.status = "ok";
      }
      for (const e of replayL2) {
        if (e.status === "ja") e.status = "ok";
      }

      for (const te of mutations.l1Adds) {
        replayL1.push({ ...te, status: isCurrent ? "ja" : "ok" });
      }
      for (const te of mutations.l2Adds) {
        replayL2.push({ ...te, status: isCurrent ? "ja" : "ok" });
      }

      for (const info of mutations.l1Consumes) {
        const truncated = truncateHex(info.actionHash);
        const entry = replayL1.find((e) => e.actionHash === truncated);
        if (entry) {
          entry.status = isCurrent ? "jc" : "consumed";
          if (info.actionDetail && Object.keys(info.actionDetail).length > 0) {
            entry.actionDetail = info.actionDetail;
          }
        }
      }
      for (const info of mutations.l2Consumes) {
        const truncated = truncateHex(info.actionHash);
        const entry = replayL2.find((e) => e.actionHash === truncated);
        if (entry) {
          entry.status = isCurrent ? "jc" : "consumed";
          if (info.actionDetail && Object.keys(info.actionDetail).length > 0) {
            entry.actionDetail = info.actionDetail;
          }
        }
      }

      const stateUpdates = extractRollupState(event);
      for (const { key, value } of stateUpdates) {
        replayState[key] = value;
      }
    }

    const selectedEvent = events[selectedIdx]!;
    const { activeNodes, activeEdges } = computeActiveForEvent(
      selectedEvent,
      l1ContractAddress,
      l2ContractAddress,
    );

    return {
      l1Table: replayL1,
      l2Table: replayL2,
      contractState: replayState,
      eventsUpTo,
      activeNodes: new Set(activeNodes),
      activeEdges: new Set(activeEdges),
    };
  }, [
    events,
    selectedEventId,
    l1Table,
    l2Table,
    contractState,
    storeActiveNodes,
    storeActiveEdges,
    l1ContractAddress,
    l2ContractAddress,
  ]);
}
