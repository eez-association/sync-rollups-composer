import { useEffect, useRef, useState } from "react";
import { config } from "../config";

export interface AbiFunction {
  name: string;
  inputs: { name: string; type: string }[];
  outputs?: { name: string; type: string }[];
  stateMutability: string;
}

interface AbiResult {
  abi: AbiFunction[] | null;
  contractName: string | null;
  loading: boolean;
  error: string | null;
}

interface CacheEntry {
  abi: AbiFunction[];
  name: string | null;
}

export function useBlockscoutAbi(address: string): AbiResult {
  const [state, setState] = useState<AbiResult>({
    abi: null,
    contractName: null,
    loading: false,
    error: null,
  });
  const cache = useRef<Map<string, CacheEntry>>(new Map());

  useEffect(() => {
    const addr = address.trim().toLowerCase();
    if (!addr || !/^0x[0-9a-f]{40}$/i.test(addr)) {
      setState({ abi: null, contractName: null, loading: false, error: null });
      return;
    }

    // Check cache
    const cached = cache.current.get(addr);
    if (cached) {
      setState({ abi: cached.abi, contractName: cached.name, loading: false, error: null });
      return;
    }

    setState({ abi: null, contractName: null, loading: true, error: null });

    const timer = setTimeout(async () => {
      try {
        const base = config.l2ExplorerApi;

        // Fetch ABI
        const abiRes = await fetch(
          `${base}/api?module=contract&action=getabi&address=${addr}`
        );
        if (!abiRes.ok) throw new Error(`Blockscout returned ${abiRes.status}`);
        const abiJson = await abiRes.json();

        if (abiJson.status !== "1" || !abiJson.result) {
          // Not verified or error
          setState({ abi: null, contractName: null, loading: false, error: null });
          return;
        }

        const rawAbi = typeof abiJson.result === "string"
          ? JSON.parse(abiJson.result)
          : abiJson.result;

        const functions: AbiFunction[] = rawAbi
          .filter((item: { type?: string }) => item.type === "function")
          .map((item: AbiFunction) => ({
            name: item.name,
            inputs: item.inputs || [],
            outputs: item.outputs || [],
            stateMutability: item.stateMutability || "nonpayable",
          }));

        // Fetch contract name
        let contractName: string | null = null;
        try {
          const srcRes = await fetch(
            `${base}/api?module=contract&action=getsourcecode&address=${addr}`
          );
          if (srcRes.ok) {
            const srcJson = await srcRes.json();
            if (srcJson.status === "1" && srcJson.result?.[0]?.ContractName) {
              contractName = srcJson.result[0].ContractName;
            }
          }
        } catch {
          /* contract name is optional */
        }

        cache.current.set(addr, { abi: functions, name: contractName });
        setState({ abi: functions, contractName, loading: false, error: null });
      } catch (e) {
        setState({
          abi: null,
          contractName: null,
          loading: false,
          error: (e as Error).message || "Failed to fetch ABI",
        });
      }
    }, 500); // debounce

    return () => clearTimeout(timer);
  }, [address]);

  return state;
}
