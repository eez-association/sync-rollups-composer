import { useCallback, useRef, useState } from "react";
import type { LogEntry } from "../types";

const MAX_ENTRIES = 100;

export function useLog() {
  const [entries, setEntries] = useState<LogEntry[]>([]);
  const idRef = useRef(0);

  const log = useCallback(
    (message: string, type: LogEntry["type"] = "ok") => {
      const entry: LogEntry = {
        id: idRef.current++,
        time: new Date().toLocaleTimeString(),
        message,
        type,
      };
      setEntries((prev) => [entry, ...prev].slice(0, MAX_ENTRIES));
    },
    [],
  );

  return { entries, log };
}
