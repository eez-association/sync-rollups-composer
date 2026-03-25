import React from "react";
import type { TableEntry } from "../../types/visualization";
import { TablePanel } from "./TablePanel";
import styles from "./ExecutionTables.module.css";

type Props = {
  l1Entries: TableEntry[];
  l2Entries: TableEntry[];
};

export const ExecutionTables: React.FC<Props> = ({ l1Entries, l2Entries }) => {
  return (
    <div className={styles.grid}>
      <TablePanel
        title="L1 Execution Table (Rollups)"
        chain="l1"
        entries={l1Entries}
      />
      <TablePanel
        title="L2 Execution Table (ManagerL2)"
        chain="l2"
        entries={l2Entries}
      />
    </div>
  );
};
