import { useCallback, useEffect, useState } from "react";

export interface HealthData {
  healthy: boolean;
  mode: string;
  l2_head: number;
  l1_derivation_head: number;
  pending_submissions: number;
  consecutive_rewind_cycles: number;
  commit?: string;
}

const HOST = window.location.hostname;
const PROTO = window.location.protocol;
const params = new URLSearchParams(window.location.search);
const IS_PROXIED =
  PROTO === "https:" ||
  (HOST !== "localhost" && HOST !== "127.0.0.1" && !window.location.port);
const HEALTH_URL = params.get("health") ||
  (IS_PROXIED ? `${PROTO}//${window.location.host}/health` : `http://${HOST}:9560/health`);

export function useHealth() {
  const [health, setHealth] = useState<HealthData | null>(null);

  const refresh = useCallback(async () => {
    try {
      const res = await fetch(HEALTH_URL);
      if (res.ok) {
        const data = (await res.json()) as HealthData;
        setHealth(data);
      } else {
        setHealth(null);
      }
    } catch {
      setHealth(null);
    }
  }, []);

  useEffect(() => {
    refresh();
    const interval = setInterval(refresh, 3000);
    return () => clearInterval(interval);
  }, [refresh]);

  return health;
}
