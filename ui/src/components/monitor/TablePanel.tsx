import React from "react";
import type { TableEntry, Chain } from "../../types/visualization";
import { TableEntryRow } from "./TableEntryRow";
import styles from "./TablePanel.module.css";

type Props = {
  title: string;
  chain: Chain;
  entries: TableEntry[];
};

export const TablePanel: React.FC<Props> = ({ title, chain, entries }) => {
  const activeCount = entries.filter(
    (e) => e.status === "ok" || e.status === "ja",
  ).length;

  const color = chain === "l1" ? "#3b82f6" : "#a855f7";
  const borderColor = chain === "l1" ? "rgba(59,130,246,0.2)" : "rgba(168,85,247,0.2)";
  const bgColor = chain === "l1" ? "rgba(59,130,246,0.06)" : "rgba(168,85,247,0.06)";

  return (
    <div className={styles.panel} style={{ borderColor }}>
      <div className={styles.panelHeader} style={{ background: bgColor }}>
        <span style={{ color }}>{title}</span>
        <span className={styles.countBadge}>{activeCount} entries</span>
      </div>
      <div className={styles.panelBody}>
        {entries.length === 0 ? (
          <div className={styles.empty}>(empty)</div>
        ) : (
          entries.map((e, i) => (
            <TableEntryRow key={e.id} entry={e} index={i} />
          ))
        )}
      </div>
    </div>
  );
};
