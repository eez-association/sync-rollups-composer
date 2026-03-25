import React, { useState } from "react";
import { useMonitorStore } from "../../store";
import { initManagerNodes, resetDiscovery } from "../../lib/autoDiscovery";
import styles from "./ConnectionBar.module.css";

export const ConnectionBar: React.FC = () => {
  const l1RpcUrl = useMonitorStore((s) => s.l1RpcUrl);
  const l2RpcUrl = useMonitorStore((s) => s.l2RpcUrl);
  const l1ContractAddress = useMonitorStore((s) => s.l1ContractAddress);
  const l2ContractAddress = useMonitorStore((s) => s.l2ContractAddress);
  const connected = useMonitorStore((s) => s.connected);
  const l1Connected = useMonitorStore((s) => s.l1Connected);
  const l2Connected = useMonitorStore((s) => s.l2Connected);
  const setL1RpcUrl = useMonitorStore((s) => s.setL1RpcUrl);
  const setL2RpcUrl = useMonitorStore((s) => s.setL2RpcUrl);
  const setL1ContractAddress = useMonitorStore((s) => s.setL1ContractAddress);
  const setL2ContractAddress = useMonitorStore((s) => s.setL2ContractAddress);
  const setConnected = useMonitorStore((s) => s.setConnected);
  const addNodes = useMonitorStore((s) => s.addNodes);
  const addKnownAddresses = useMonitorStore((s) => s.addKnownAddresses);
  const clearAll = useMonitorStore((s) => s.clearAll);

  const [localL1Rpc, setLocalL1Rpc] = useState(l1RpcUrl);
  const [localL2Rpc, setLocalL2Rpc] = useState(l2RpcUrl);
  const [localL1Addr, setLocalL1Addr] = useState(l1ContractAddress);
  const [localL2Addr, setLocalL2Addr] = useState(l2ContractAddress);

  const handleConnect = () => {
    if (connected) {
      clearAll();
      resetDiscovery();
      setConnected(false, false);
      return;
    }
    setL1RpcUrl(localL1Rpc);
    setL2RpcUrl(localL2Rpc);
    setL1ContractAddress(localL1Addr);
    setL2ContractAddress(localL2Addr);

    const result = initManagerNodes(localL1Addr, localL2Addr);
    if (result.newNodes.length > 0) addNodes(result.newNodes);
    if (result.addressInfos.length > 0) addKnownAddresses(result.addressInfos);

    setConnected(!!localL1Rpc, !!localL2Rpc);
  };

  return (
    <div className={styles.bar}>
      <div className={styles.field}>
        <div
          className={`${styles.dot} ${l1Connected ? styles.dotConnected : styles.dotDisconnected}`}
        />
        <InputField
          label="L1 RPC"
          value={localL1Rpc}
          onChange={setLocalL1Rpc}
          disabled={connected}
          width={140}
        />
      </div>

      <InputField
        label="L1 Contract"
        value={localL1Addr}
        onChange={setLocalL1Addr}
        disabled={connected}
        width={280}
        placeholder="0x..."
      />

      <div className={styles.field}>
        <div
          className={`${styles.dot} ${l2Connected ? styles.dotConnected : styles.dotDisconnected}`}
        />
        <InputField
          label="L2 RPC"
          value={localL2Rpc}
          onChange={setLocalL2Rpc}
          disabled={connected}
          width={140}
        />
      </div>

      <InputField
        label="L2 Contract"
        value={localL2Addr}
        onChange={setLocalL2Addr}
        disabled={connected}
        width={280}
        placeholder="0x..."
      />

      <button
        onClick={handleConnect}
        className={`${styles.connectBtn} ${connected ? styles.connectBtnDisconnect : styles.connectBtnActive}`}
      >
        {connected ? "Disconnect" : "Connect"}
      </button>
    </div>
  );
};

const InputField: React.FC<{
  label: string;
  value: string;
  onChange: (v: string) => void;
  disabled: boolean;
  width: number;
  placeholder?: string;
}> = ({ label, value, onChange, disabled, width, placeholder }) => (
  <div className={styles.inputGroup}>
    <span className={styles.label}>{label}</span>
    <input
      value={value}
      onChange={(e) => onChange(e.target.value)}
      disabled={disabled}
      placeholder={placeholder}
      className={styles.input}
      style={{ width }}
    />
  </div>
);
