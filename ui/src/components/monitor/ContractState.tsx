import React from "react";
import type { Chain } from "../../types/visualization";
import styles from "./ContractState.module.css";

type Props = {
  contractState: Record<string, string>;
  changedKeys: string[];
};

export const ContractState: React.FC<Props> = ({
  contractState,
  changedKeys,
}) => {
  const entries = Object.entries(contractState);
  if (entries.length === 0) return null;

  const l1Entries = entries.filter(([k]) => k.startsWith("Rollup"));
  const l2Entries = entries.filter(([k]) => !k.startsWith("Rollup"));

  return (
    <div className={styles.grid}>
      <StatePanel
        title="L1 Contracts"
        chain="l1"
        entries={l1Entries}
        changedKeys={changedKeys}
      />
      {l2Entries.length > 0 && (
        <StatePanel
          title="L2 Contracts"
          chain="l2"
          entries={l2Entries}
          changedKeys={changedKeys}
        />
      )}
    </div>
  );
};

const StatePanel: React.FC<{
  title: string;
  chain: Chain;
  entries: [string, string][];
  changedKeys: string[];
}> = ({ title, chain, entries, changedKeys }) => {
  const color = chain === "l1" ? "#3b82f6" : "#a855f7";
  const borderColor = chain === "l1" ? "rgba(59,130,246,0.2)" : "rgba(168,85,247,0.2)";

  return (
    <div className={styles.panel} style={{ borderColor }}>
      <h4 className={styles.panelTitle} style={{ color }}>
        {title}
      </h4>
      {entries.map(([key, value]) => {
        const isChanged = changedKeys.includes(key);
        return (
          <div key={key} className={styles.stateRow}>
            <span className={styles.stateKey}>{key}</span>
            <span
              className={styles.stateValue}
              style={{ color: isChanged ? "var(--green)" : "var(--text)" }}
            >
              {value}
            </span>
          </div>
        );
      })}
    </div>
  );
};
