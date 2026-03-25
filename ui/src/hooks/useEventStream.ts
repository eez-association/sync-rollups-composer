import { useCallback } from "react";
import { useMonitorStore } from "../store";
import { rollupsAbi } from "../abi/rollups";
import { crossChainManagerL2Abi } from "../abi/crossChainManagerL2";
import { useChainWatcher } from "./useChainWatcher";
import { processEventForTables, extractRollupState } from "../lib/eventProcessor";
import { discoverFromEvent } from "../lib/autoDiscovery";
import type { EventRecord } from "../types/events";
import { truncateHex } from "../lib/actionFormatter";

export function useEventStream() {
  const l1RpcUrl = useMonitorStore((s) => s.l1RpcUrl);
  const l2RpcUrl = useMonitorStore((s) => s.l2RpcUrl);
  const l1ContractAddress = useMonitorStore((s) => s.l1ContractAddress);
  const l2ContractAddress = useMonitorStore((s) => s.l2ContractAddress);
  const connected = useMonitorStore((s) => s.connected);

  const addEvent = useMonitorStore((s) => s.addEvent);
  const addL1Entries = useMonitorStore((s) => s.addL1Entries);
  const addL2Entries = useMonitorStore((s) => s.addL2Entries);
  const consumeL1Entry = useMonitorStore((s) => s.consumeL1Entry);
  const consumeL2Entry = useMonitorStore((s) => s.consumeL2Entry);
  const addNodes = useMonitorStore((s) => s.addNodes);
  const addEdges = useMonitorStore((s) => s.addEdges);
  const addKnownAddresses = useMonitorStore((s) => s.addKnownAddresses);
  const updateContractState = useMonitorStore((s) => s.updateContractState);

  const handleEvent = useCallback(
    (event: EventRecord) => {
      addEvent(event);

      const tableMutations = processEventForTables(event);
      if (tableMutations.l1Adds.length > 0) addL1Entries(tableMutations.l1Adds);
      if (tableMutations.l2Adds.length > 0) addL2Entries(tableMutations.l2Adds);
      for (const info of tableMutations.l1Consumes) consumeL1Entry(truncateHex(info.actionHash), info.actionDetail);
      for (const info of tableMutations.l2Consumes) consumeL2Entry(truncateHex(info.actionHash), info.actionDetail);

      const store = useMonitorStore.getState();
      const existingNodeIds = new Set([
        ...store.l1Nodes.map((n) => n.id),
        ...store.l2Nodes.map((n) => n.id),
      ]);
      const discovery = discoverFromEvent(
        event,
        store.knownAddresses,
        existingNodeIds,
        l1ContractAddress,
        l2ContractAddress,
      );
      if (discovery.newNodes.length > 0) addNodes(discovery.newNodes);
      if (discovery.newEdges.length > 0) addEdges(discovery.newEdges);
      if (discovery.addressInfos.length > 0)
        addKnownAddresses(discovery.addressInfos);

      const stateUpdates = extractRollupState(event);
      for (const { key, value } of stateUpdates) {
        updateContractState(key, value);
      }

      const activeNodes: string[] = [];
      const activeEdges: string[] = [];
      if (event.args.proxy) {
        activeNodes.push((event.args.proxy as string).toLowerCase());
      }
      if (event.args.sourceAddress) {
        activeNodes.push((event.args.sourceAddress as string).toLowerCase());
      }
      if (event.args.destination) {
        activeNodes.push((event.args.destination as string).toLowerCase());
      }
      const managerAddr =
        event.chain === "l1"
          ? l1ContractAddress.toLowerCase()
          : l2ContractAddress.toLowerCase();
      if (managerAddr) activeNodes.push(managerAddr);

      if (activeNodes.length > 0) {
        store.setActiveNodes(activeNodes);
        for (let i = 0; i < activeNodes.length - 1; i++) {
          activeEdges.push(`${activeNodes[i]}->${activeNodes[i + 1]}`);
        }
        store.setActiveEdges(activeEdges);
      }
    },
    [
      addEvent,
      addL1Entries,
      addL2Entries,
      consumeL1Entry,
      consumeL2Entry,
      addNodes,
      addEdges,
      addKnownAddresses,
      updateContractState,
      l1ContractAddress,
      l2ContractAddress,
    ],
  );

  useChainWatcher({
    rpcUrl: l1RpcUrl,
    contractAddress: l1ContractAddress as `0x${string}`,
    abi: rollupsAbi,
    chain: "l1",
    onEvent: handleEvent,
    enabled: connected && !!l1ContractAddress,
  });

  useChainWatcher({
    rpcUrl: l2RpcUrl,
    contractAddress: l2ContractAddress as `0x${string}`,
    abi: crossChainManagerL2Abi,
    chain: "l2",
    onEvent: handleEvent,
    enabled: connected && !!l2ContractAddress,
  });
}
