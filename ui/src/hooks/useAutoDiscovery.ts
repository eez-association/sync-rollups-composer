import { useEffect, useRef } from "react";
import { useMonitorStore } from "../store";
import { initManagerNodes } from "../lib/autoDiscovery";

export function useAutoDiscovery() {
  const l1ContractAddress = useMonitorStore((s) => s.l1ContractAddress);
  const l2ContractAddress = useMonitorStore((s) => s.l2ContractAddress);
  const addNodes = useMonitorStore((s) => s.addNodes);
  const addKnownAddresses = useMonitorStore((s) => s.addKnownAddresses);
  const seededRef = useRef(false);

  useEffect(() => {
    if (seededRef.current) return;
    if (!l1ContractAddress && !l2ContractAddress) return;
    seededRef.current = true;

    const result = initManagerNodes(l1ContractAddress, l2ContractAddress);
    if (result.newNodes.length > 0) addNodes(result.newNodes);
    if (result.addressInfos.length > 0) addKnownAddresses(result.addressInfos);
  }, [l1ContractAddress, l2ContractAddress, addNodes, addKnownAddresses]);
}
