import React, { useState, useCallback, useEffect, useRef } from "react";
import { config } from "../../config";
import { useMonitorStore } from "../../store";
import { useEventStream } from "../../hooks/useEventStream";
import { useDerivedState } from "../../hooks/useDerivedState";
import { useAutoDiscovery } from "../../hooks/useAutoDiscovery";
import { initManagerNodes } from "../../lib/autoDiscovery";
import { ConnectionBar } from "./ConnectionBar";
import { ArchitectureDiagram } from "./ArchitectureDiagram";
import { ExecutionTables } from "./ExecutionTables";
import { ContractState } from "./ContractState";
import { EventTimeline } from "./EventTimeline";
import { EventInfoBanner } from "./EventInfoBanner";
import { BundleDetail } from "./BundleDetail";
import type { TransactionBundle } from "../../types/visualization";
import styles from "./MonitorView.module.css";

export const MonitorView: React.FC = () => {
  useAutoDiscovery();
  useEventStream();

  // Auto-connect on mount once config has loaded valid addresses.
  // config.rollupsAddress is mutated by useConfigLoader after fetching /shared/rollup.env,
  // so we poll briefly to pick it up (the store snapshot may still be "").
  const autoConnectedRef = useRef(false);
  const connected = useMonitorStore((s) => s.connected);
  const setL1RpcUrl = useMonitorStore((s) => s.setL1RpcUrl);
  const setL2RpcUrl = useMonitorStore((s) => s.setL2RpcUrl);
  const setL1ContractAddress = useMonitorStore((s) => s.setL1ContractAddress);
  const setL2ContractAddress = useMonitorStore((s) => s.setL2ContractAddress);
  const setConnected = useMonitorStore((s) => s.setConnected);
  const addNodes = useMonitorStore((s) => s.addNodes);
  const addKnownAddresses = useMonitorStore((s) => s.addKnownAddresses);

  useEffect(() => {
    if (autoConnectedRef.current || connected) return;

    const tryAutoConnect = () => {
      if (autoConnectedRef.current) return true;
      const l1Rpc = config.l1Rpc;
      const l2Rpc = config.l2Rpc;
      const rollupsAddr = config.rollupsAddress;
      const hasL1 = !!l1Rpc && !!rollupsAddr;
      const hasL2 = !!l2Rpc;
      if (!hasL1 && !hasL2) return false;

      autoConnectedRef.current = true;
      // Push live config values into the store
      setL1RpcUrl(l1Rpc);
      setL2RpcUrl(l2Rpc);
      if (rollupsAddr) setL1ContractAddress(rollupsAddr);

      // Seed architecture graph
      const result = initManagerNodes(rollupsAddr, "0x4200000000000000000000000000000000000003");
      if (result.newNodes.length > 0) addNodes(result.newNodes);
      if (result.addressInfos.length > 0) addKnownAddresses(result.addressInfos);

      setConnected(hasL1, hasL2);
      return true;
    };

    // Try immediately, then retry a few times to wait for useConfigLoader
    if (tryAutoConnect()) return;
    let attempts = 0;
    const timer = setInterval(() => {
      attempts++;
      if (tryAutoConnect() || attempts >= 10) clearInterval(timer);
    }, 300);
    return () => clearInterval(timer);
  }, [connected, setL1RpcUrl, setL2RpcUrl, setL1ContractAddress, setL2ContractAddress, setConnected, addNodes, addKnownAddresses]);
  const changedKeys = useMonitorStore((s) => s.changedKeys);
  const { l1Table, l2Table, contractState, activeNodes, activeEdges } =
    useDerivedState();
  const [selectedBundle, setSelectedBundle] = useState<TransactionBundle | null>(null);

  const handleSelectBundle = useCallback((bundle: TransactionBundle) => {
    setSelectedBundle(bundle);
  }, []);

  const handleCloseBundle = useCallback(() => {
    setSelectedBundle(null);
  }, []);

  return (
    <div className={styles.root}>
      <header className={styles.header}>
        <h1 className={styles.title}>Cross-Chain Execution Visualizer</h1>
        <p className={styles.subtitle}>
          Execution table evolution across L1 &amp; L2 — live event stream
        </p>
      </header>

      <ConnectionBar />

      <div className={styles.body}>
        <div className={styles.main}>
          {!connected ? (
            <div className={styles.empty}>
              Enter RPC URLs and contract addresses, then click Connect
            </div>
          ) : (
            <>
              <EventInfoBanner />
              <div className={styles.section}>
                <ArchitectureDiagram
                  activeNodes={activeNodes}
                  activeEdges={activeEdges}
                />
              </div>
              <div className={styles.section}>
                <ExecutionTables l1Entries={l1Table} l2Entries={l2Table} />
              </div>
              <ContractState
                contractState={contractState}
                changedKeys={changedKeys}
              />
            </>
          )}
        </div>

        {connected && (
          <div className={styles.sidebar}>
            <EventTimeline onSelectBundle={handleSelectBundle} />
          </div>
        )}
      </div>

      {selectedBundle && (
        <BundleDetail bundle={selectedBundle} onClose={handleCloseBundle} />
      )}
    </div>
  );
};
