import type { LogEntry } from "../types";
import styles from "./EventLog.module.css";

interface Props {
  entries: LogEntry[];
}

const typeClass: Record<LogEntry["type"], string> = {
  ok: styles.ok ?? "",
  err: styles.err ?? "",
  info: styles.info ?? "",
};

export function EventLog({ entries }: Props) {
  return (
    <div className={styles.card}>
      <div className={styles.cardHeader}>
        <span className={styles.cardTitle}>Event Log</span>
      </div>
      <div className={styles.log}>
        {entries.map((entry) => (
          <div key={entry.id} className={styles.entry}>
            <span className={styles.time}>{entry.time}</span>
            <span className={typeClass[entry.type]}>{entry.message}</span>
          </div>
        ))}
      </div>
    </div>
  );
}
