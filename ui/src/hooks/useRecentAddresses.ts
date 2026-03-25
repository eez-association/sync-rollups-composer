import { useCallback, useState } from "react";

const STORAGE_KEY = "recentL2Addresses";
const MAX_ENTRIES = 8;

function load(): string[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return [];
    const arr = JSON.parse(raw);
    return Array.isArray(arr) ? arr : [];
  } catch {
    return [];
  }
}

export function useRecentAddresses() {
  const [addresses, setAddresses] = useState<string[]>(load);

  const addAddress = useCallback((addr: string) => {
    const normalized = addr.trim().toLowerCase();
    if (!normalized || !/^0x[0-9a-f]{40}$/i.test(normalized)) return;
    setAddresses((prev) => {
      const filtered = prev.filter((a) => a !== normalized);
      const updated = [normalized, ...filtered].slice(0, MAX_ENTRIES);
      localStorage.setItem(STORAGE_KEY, JSON.stringify(updated));
      return updated;
    });
  }, []);

  const clear = useCallback(() => {
    setAddresses([]);
    localStorage.removeItem(STORAGE_KEY);
  }, []);

  return { addresses, addAddress, clear };
}
