import { useCallback, useState } from "react";

export interface TxRecord {
  id: string;
  type: "deploy" | "increment" | "cross-chain-proxy" | "cross-chain-call" | "faucet";
  hash: string | null;
  status: "pending" | "confirmed" | "failed";
  label: string;
  gasUsed: string | null;
  timestamp: number;
}

const STORAGE_KEY = "txHistory";
const MAX_RECORDS = 50;

function loadHistory(): TxRecord[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const parsed = JSON.parse(raw) as TxRecord[];
    return Array.isArray(parsed) ? parsed.slice(0, MAX_RECORDS) : [];
  } catch {
    return [];
  }
}

function saveHistory(records: TxRecord[]) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(records.slice(0, MAX_RECORDS)));
  } catch { /* quota exceeded — ignore */ }
}

let idCounter = 0;

export function useTxHistory() {
  const [records, setRecords] = useState<TxRecord[]>(loadHistory);

  /** Add a new pending tx. Returns the record id for later updates. */
  const addTx = useCallback(
    (type: TxRecord["type"], label: string, hash: string | null = null): string => {
      const id = `tx-${Date.now()}-${++idCounter}`;
      const record: TxRecord = {
        id,
        type,
        hash,
        status: "pending",
        label,
        gasUsed: null,
        timestamp: Date.now(),
      };
      setRecords((prev) => {
        const next = [record, ...prev].slice(0, MAX_RECORDS);
        saveHistory(next);
        return next;
      });
      return id;
    },
    [],
  );

  /** Update an existing tx record by id */
  const updateTx = useCallback(
    (id: string, updates: Partial<Pick<TxRecord, "hash" | "status" | "gasUsed">>) => {
      setRecords((prev) => {
        const next = prev.map((r) => (r.id === id ? { ...r, ...updates } : r));
        saveHistory(next);
        return next;
      });
    },
    [],
  );

  /** Clear all history */
  const clearHistory = useCallback(() => {
    setRecords([]);
    localStorage.removeItem(STORAGE_KEY);
  }, []);

  return { records, addTx, updateTx, clearHistory };
}
