import { useCallback, useEffect, useRef, useState } from "react";
import {
  createWalletClient,
  http,
  defineChain,
  type Hex,
} from "viem";
import { privateKeyToAccount } from "viem/accounts";
import { config } from "../config";
import { rpcCall } from "../rpc";

type Logger = (msg: string, type?: "ok" | "err" | "info") => void;

export type FaucetPhase =
  | "idle"
  | "sending"
  | "tx-pending"
  | "confirmed"
  | "failed";

export interface FaucetState {
  phase: FaucetPhase;
  chain: "l1" | "l2";
  txHash: string | null;
  error: string | null;
}

const INITIAL_STATE: FaucetState = {
  phase: "idle",
  chain: "l1",
  txHash: null,
  error: null,
};

// Bridge ABI: bridgeEther(uint256 _rollupId, address destinationAddress) payable
const BRIDGE_ETHER_SELECTOR = "0xf402d9f3";
const FAUCET_AMOUNT = "0.5";
const FAUCET_AMOUNT_WEI = 5n * 10n ** 17n; // 0.5 ETH

const COOLDOWN_PER_ADDRESS_CHAIN_MS = 5 * 60 * 1000; // 5 min
const COOLDOWN_GLOBAL_MS = 30 * 1000; // 30s
const COOLDOWN_STORAGE_KEY = "faucet_cooldowns";
const MAX_RECIPIENT_BALANCE = 10n * 10n ** 18n; // 10 ETH
const POLL_INTERVAL_MS = 2000;
const POLL_TIMEOUT_MS = 60000;
const FAUCET_BALANCE_POLL_MS = 30000;
// bridgeEther via L1 proxy involves cross-chain entry creation — needs ~400K+ gas.
// eth_estimateGas underestimates because it can't account for the proxy's entry logic.
const BRIDGE_GAS_LIMIT = 500_000n;

function pad32(hex: string): string {
  return hex.replace("0x", "").padStart(64, "0");
}

function formatEth(weiHex: string): string {
  const raw = BigInt(weiHex);
  const whole = raw / 10n ** 18n;
  const frac = raw % 10n ** 18n;
  if (frac === 0n) return whole.toString();
  const fracStr = frac.toString().padStart(18, "0");
  const trimmed = fracStr.slice(0, 4).replace(/0+$/, "");
  return trimmed ? `${whole}.${trimmed}` : whole.toString();
}

interface CooldownMap {
  [address: string]: { [chain: string]: number };
}

function loadCooldowns(): CooldownMap {
  try {
    return JSON.parse(localStorage.getItem(COOLDOWN_STORAGE_KEY) || "{}") as CooldownMap;
  } catch {
    return {};
  }
}

function saveCooldown(address: string, chain: string) {
  const map = loadCooldowns();
  if (!map[address]) map[address] = {};
  map[address][chain] = Date.now();
  if (!map["__global__"]) map["__global__"] = {};
  map["__global__"]["last"] = Date.now();
  localStorage.setItem(COOLDOWN_STORAGE_KEY, JSON.stringify(map));
}

function getCooldownRemaining(address: string, chain: string): number {
  const map = loadCooldowns();
  const addrTs = map[address]?.[chain] ?? 0;
  const addrRemaining = Math.max(0, COOLDOWN_PER_ADDRESS_CHAIN_MS - (Date.now() - addrTs));
  const globalTs = map["__global__"]?.["last"] ?? 0;
  const globalRemaining = Math.max(0, COOLDOWN_GLOBAL_MS - (Date.now() - globalTs));
  return Math.max(addrRemaining, globalRemaining);
}

/** Faucet hook — sends 0.5 ETH to the connected wallet address. */
export function useFaucet(log: Logger, walletAddress: string | null) {
  const [state, setState] = useState<FaucetState>(INITIAL_STATE);
  const [faucetKey, setFaucetKey] = useState<string | null>(null);
  const [faucetReady, setFaucetReady] = useState(false);
  const [faucetBalance, setFaucetBalance] = useState<string | null>(null);
  const [cooldownRemaining, setCooldownRemaining] = useState(0);

  const stateRef = useRef(state);
  stateRef.current = state;
  const walletRef = useRef(walletAddress);
  walletRef.current = walletAddress;

  // On mount, fetch the faucet private key
  useEffect(() => {
    let cancelled = false;
    (async () => {
      try {
        const resp = await fetch("/shared/faucet.key");
        if (!resp.ok) {
          if (!cancelled) setFaucetReady(false);
          return;
        }
        const raw = (await resp.text()).trim();
        // Vite dev server returns HTML for 404 — detect that
        if (!raw || !raw.startsWith("0x") || raw.length < 64 || raw.includes("<")) {
          if (!cancelled) setFaucetReady(false);
          return;
        }
        if (!cancelled) {
          setFaucetKey(raw);
          setFaucetReady(true);
        }
      } catch {
        if (!cancelled) setFaucetReady(false);
      }
    })();
    return () => { cancelled = true; };
  }, []);

  // Poll faucet L1 balance every 30s
  useEffect(() => {
    if (!faucetKey) return;
    let cancelled = false;
    const account = privateKeyToAccount(faucetKey as Hex);
    const faucetAddr = account.address;

    async function fetchBalance() {
      try {
        const bal = (await rpcCall(config.l1Rpc, "eth_getBalance", [faucetAddr, "latest"])) as string;
        if (!cancelled) setFaucetBalance(formatEth(bal));
      } catch { /* silently fail */ }
    }

    fetchBalance();
    const interval = setInterval(fetchBalance, FAUCET_BALANCE_POLL_MS);
    return () => { cancelled = true; clearInterval(interval); };
  }, [faucetKey]);

  // Cooldown countdown timer — uses wallet address
  useEffect(() => {
    const interval = setInterval(() => {
      const addr = walletRef.current;
      const { chain } = stateRef.current;
      if (!addr) { setCooldownRemaining(0); return; }
      const remaining = getCooldownRemaining(addr.toLowerCase(), chain);
      setCooldownRemaining(Math.ceil(remaining / 1000));
    }, 1000);
    return () => clearInterval(interval);
  }, []);

  const setChain = useCallback((chain: "l1" | "l2") => {
    setState((s) => ({ ...s, chain, phase: s.phase === "failed" || s.phase === "confirmed" ? "idle" : s.phase, error: null, txHash: null }));
  }, []);

  const dismiss = useCallback(() => {
    setState((s) => ({ ...s, phase: "idle", error: null, txHash: null }));
  }, []);

  const requestFunds = useCallback(async () => {
    if (stateRef.current.phase !== "idle") return;

    const { chain } = stateRef.current;
    const recipientAddress = walletRef.current;

    if (!faucetKey || !faucetReady) {
      setState((s) => ({ ...s, phase: "failed", error: "Faucet not configured — redeploy with updated deploy.sh" }));
      return;
    }

    if (!recipientAddress || !/^0x[0-9a-fA-F]{40}$/.test(recipientAddress)) {
      setState((s) => ({ ...s, phase: "failed", error: "Connect your wallet first" }));
      return;
    }

    // Check cooldown
    const remaining = getCooldownRemaining(recipientAddress.toLowerCase(), chain);
    if (remaining > 0) {
      setState((s) => ({ ...s, phase: "failed", error: `Cooldown active. Try again in ${Math.ceil(remaining / 1000)}s` }));
      return;
    }

    // Check recipient balance on target chain
    const targetRpc = chain === "l1" ? config.l1Rpc : config.l2Rpc;
    try {
      const balHex = (await rpcCall(targetRpc, "eth_getBalance", [recipientAddress, "latest"])) as string;
      if (BigInt(balHex) > MAX_RECIPIENT_BALANCE) {
        setState((s) => ({ ...s, phase: "failed", error: `Already have ${formatEth(balHex)} ETH on ${chain.toUpperCase()} (max: 10)` }));
        return;
      }
    } catch { /* proceed anyway */ }

    setState((s) => ({ ...s, phase: "sending", error: null, txHash: null }));

    try {
      const account = privateKeyToAccount(faucetKey as Hex);
      let txHash: string;
      let rpcUrl: string;

      const l1ChainId = parseInt(
        ((await rpcCall(config.l1Rpc, "eth_chainId", [])) as string), 16,
      );

      if (chain === "l1") {
        rpcUrl = config.l1Rpc;
        const client = createWalletClient({
          account,
          chain: defineChain({ id: l1ChainId, name: "faucet", nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 }, rpcUrls: { default: { http: [rpcUrl] } } }),
          transport: http(rpcUrl),
        });
        txHash = await client.sendTransaction({ to: recipientAddress as Hex, value: FAUCET_AMOUNT_WEI });
        log(`Faucet: sending ${FAUCET_AMOUNT} ETH to ${recipientAddress.slice(0, 10)}... on L1`, "info");
      } else {
        rpcUrl = config.l1Rpc; // receipt lives on L1
        const bridgeAddr = config.l1Bridge;
        if (!bridgeAddr) {
          setState((s) => ({ ...s, phase: "failed", error: "L1 Bridge not configured" }));
          return;
        }
        const rollupId = parseInt(config.rollupId).toString(16).padStart(64, "0");
        const data = (BRIDGE_ETHER_SELECTOR + rollupId + pad32(recipientAddress)) as Hex;
        const client = createWalletClient({
          account,
          chain: defineChain({ id: l1ChainId, name: "faucet", nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 }, rpcUrls: { default: { http: [config.l1ProxyRpc] } } }),
          transport: http(config.l1ProxyRpc),
        });
        txHash = await client.sendTransaction({ to: bridgeAddr as Hex, data, value: FAUCET_AMOUNT_WEI, gas: BRIDGE_GAS_LIMIT });
        log(`Faucet: bridging ${FAUCET_AMOUNT} ETH to ${recipientAddress.slice(0, 10)}... on L2`, "info");
      }

      setState((s) => ({ ...s, phase: "tx-pending", txHash }));

      // Poll for receipt
      const startTime = Date.now();
      while (Date.now() - startTime < POLL_TIMEOUT_MS) {
        await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
        try {
          const receipt = (await rpcCall(rpcUrl, "eth_getTransactionReceipt", [txHash])) as { status?: string } | null;
          if (receipt) {
            if (receipt.status === "0x1") {
              saveCooldown(recipientAddress.toLowerCase(), chain);
              setState((s) => ({ ...s, phase: "confirmed", error: null }));
              log(chain === "l1"
                ? `Faucet: sent ${FAUCET_AMOUNT} ETH to ${recipientAddress.slice(0, 10)}...`
                : `Faucet: bridging ${FAUCET_AMOUNT} ETH to ${recipientAddress.slice(0, 10)}... — arrives in ~12-24s`, "ok");
              setTimeout(() => {
                setState((s) => (s.phase === "confirmed" ? { ...s, phase: "idle", txHash: null } : s));
              }, 8000);
            } else {
              setState((s) => ({ ...s, phase: "failed", error: "Faucet transaction reverted" }));
              log("Faucet tx reverted", "err");
            }
            return;
          }
        } catch { /* not mined yet */ }
      }

      setState((s) => ({ ...s, phase: "failed", error: "No receipt after 60s" }));
    } catch (e) {
      const msg = (e as Error).message || "Transaction failed";
      setState((s) => ({ ...s, phase: "failed", error: msg }));
      log(`Faucet failed: ${msg}`, "err");
    }
  }, [faucetKey, faucetReady, log]);

  return {
    state,
    ready: faucetReady,
    setChain,
    requestFunds,
    dismiss,
    cooldownRemaining,
    faucetBalance,
  };
}
