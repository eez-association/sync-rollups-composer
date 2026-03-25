import { useCallback, useEffect, useRef, useState } from "react";
import { config, ESTIMATION_SENDER } from "../config";
import { rpcCall } from "../rpc";
import { estimateGas, estimateCrossChainGas, gasToHex } from "../lib/gasEstimation";

type Logger = (msg: string, type?: "ok" | "err" | "info") => void;
type SendTx = (params: Record<string, string>) => Promise<string>;

export type BridgeDirection = "l1-to-l2" | "l2-to-l1";
export type BridgeAsset = "eth" | "erc20";
export type BridgePhase =
  | "idle"
  | "approving"
  | "approve-pending"
  | "sending"
  | "tx-pending"
  | "confirmed"
  | "failed";

export interface TokenMeta {
  name: string;
  symbol: string;
  decimals: number;
  address: string;
}

export interface BridgeGasState {
  status: "idle" | "estimating" | "estimated" | "error";
  estimate: number | null;
  estimateWithBuffer: number | null;
  gasHex: string | null;
  method: string | null;
  errorMessage: string | null;
}

export interface BridgeState {
  phase: BridgePhase;
  direction: BridgeDirection;
  asset: BridgeAsset;
  amount: string;
  /** Custom destination address. Empty string means "use my wallet address" */
  destinationAddress: string;
  tokenAddress: string;
  tokenMeta: TokenMeta | null;
  txHash: string | null;
  error: string | null;
  sourceBalance: string | null;
  sourceBalanceRaw: bigint | null;
  allowance: bigint | null;
  l1BridgeReady: boolean;
  l2BridgeReady: boolean;
  gas: BridgeGasState;
}

const RECENT_TOKENS_KEY = "bridgeRecentTokens";
const MAX_RECENT_TOKENS = 8;

// Bridge ABI selectors (verified via `forge inspect Bridge methodIdentifiers`)
const BRIDGE_ABI = {
  // bridgeEther(uint256 _rollupId, address destinationAddress) payable
  bridgeEther: "0xf402d9f3",
  // bridgeTokens(address token, uint256 amount, uint256 _rollupId, address destinationAddress)
  bridgeTokens: "0x33b15aad",
  // manager() view returns (address)
  manager: "0x481c6a75",
};

// ERC20 ABI selectors
const ERC20_ABI = {
  // balanceOf(address) view returns (uint256)
  balanceOf: "0x70a08231",
  // allowance(address owner, address spender) view returns (uint256)
  allowance: "0xdd62ed3e",
  // approve(address spender, uint256 amount) returns (bool)
  approve: "0x095ea7b3",
  // name() view returns (string)
  name: "0x06fdde03",
  // symbol() view returns (string)
  symbol: "0x95d89b41",
  // decimals() view returns (uint8)
  decimals: "0x313ce567",
};

const MAX_UINT256 = "0x" + "f".repeat(64);

function pad32(hex: string): string {
  return hex.replace("0x", "").padStart(64, "0");
}

function encodeUint256(n: bigint): string {
  return n.toString(16).padStart(64, "0");
}

function decodeUint256(hex: string): bigint {
  const clean = hex.replace("0x", "");
  if (!clean || clean === "0".repeat(64)) return 0n;
  return BigInt("0x" + clean.slice(0, 64));
}

function decodeString(hex: string): string {
  try {
    const clean = hex.replace("0x", "");
    if (clean.length < 128) return "";
    const offset = parseInt(clean.slice(0, 64), 16) * 2;
    const length = parseInt(clean.slice(offset, offset + 64), 16);
    const data = clean.slice(offset + 64, offset + 64 + length * 2);
    const bytes = new Uint8Array(data.length / 2);
    for (let i = 0; i < bytes.length; i++) {
      bytes[i] = parseInt(data.slice(i * 2, i * 2 + 2), 16);
    }
    return new TextDecoder().decode(bytes);
  } catch {
    return "";
  }
}

/** Format a raw bigint balance to a human-readable string with up to 4 decimal places */
function formatBalance(raw: bigint, decimals: number): string {
  const divisor = 10n ** BigInt(decimals);
  const whole = raw / divisor;
  const frac = raw % divisor;
  if (frac === 0n) return whole.toString();
  const fracStr = frac.toString().padStart(decimals, "0");
  // Trim trailing zeros, keep up to 4 decimal places
  const trimmed = fracStr.slice(0, 4).replace(/0+$/, "");
  return trimmed ? `${whole}.${trimmed}` : whole.toString();
}

/** Parse a decimal string to raw bigint amount */
function parseAmount(amount: string, decimals: number): bigint | null {
  try {
    const parts = amount.split(".");
    if (parts.length > 2) return null;
    const whole = parts[0] || "0";
    const frac = (parts[1] || "").padEnd(decimals, "0").slice(0, decimals);
    return BigInt(whole) * 10n ** BigInt(decimals) + BigInt(frac);
  } catch {
    return null;
  }
}

interface TxReceipt {
  status?: string;
  gasUsed?: string;
}

/** Try to get a revert reason by replaying the tx via eth_call */
async function fetchRevertReason(rpcUrl: string, txHash: string): Promise<string> {
  try {
    const tx = (await rpcCall(rpcUrl, "eth_getTransactionByHash", [txHash])) as {
      from?: string; to?: string; data?: string; input?: string;
      value?: string; blockNumber?: string;
    } | null;
    if (!tx?.to) return "";
    const result = (await rpcCall(rpcUrl, "eth_call", [
      { from: tx.from, to: tx.to, data: tx.input || tx.data, value: tx.value },
      tx.blockNumber || "latest",
    ])) as string;
    return result || "";
  } catch (e) {
    const msg = (e as Error).message || "";
    const match = msg.match(/revert(?:ed)?:?\s*(.*)/i) || msg.match(/reason:\s*(.*)/i);
    if (match?.[1]) return match[1].trim();
    if (msg.length < 200) return msg;
    return "Reverted (reason unknown)";
  }
}

function loadRecentTokens(): TokenMeta[] {
  try {
    return JSON.parse(localStorage.getItem(RECENT_TOKENS_KEY) || "[]") as TokenMeta[];
  } catch {
    return [];
  }
}

function saveRecentToken(token: TokenMeta) {
  const existing = loadRecentTokens();
  const filtered = existing.filter(
    (t) => t.address.toLowerCase() !== token.address.toLowerCase(),
  );
  const updated = [token, ...filtered].slice(0, MAX_RECENT_TOKENS);
  localStorage.setItem(RECENT_TOKENS_KEY, JSON.stringify(updated));
}

export function useBridge(
  log: Logger,
  sendTx: SendTx,
  sendL2ProxyTx: SendTx,
  sendL1Tx: SendTx,
  sendL1ProxyTx: SendTx,
  walletAddress: string | null,
) {
  const defaultGas: BridgeGasState = {
    status: "idle", estimate: null, estimateWithBuffer: null,
    gasHex: null, method: null, errorMessage: null,
  };

  const [state, setState] = useState<BridgeState>({
    phase: "idle",
    direction: "l1-to-l2",
    asset: "eth",
    amount: "",
    destinationAddress: "",
    tokenAddress: "",
    tokenMeta: null,
    txHash: null,
    error: null,
    sourceBalance: null,
    sourceBalanceRaw: null,
    allowance: null,
    l1BridgeReady: false,
    l2BridgeReady: false,
    gas: defaultGas,
  });

  const [recentTokens, setRecentTokens] = useState<TokenMeta[]>(loadRecentTokens);
  const [gasOverrideHex, setGasOverrideHex] = useState<string | null>(null);

  const stateRef = useRef(state);
  stateRef.current = state;
  const walletRef = useRef(walletAddress);
  walletRef.current = walletAddress;

  // Check bridge readiness on mount and when addresses change
  useEffect(() => {
    let cancelled = false;

    async function checkBridge(rpcUrl: string, bridgeAddr: string): Promise<boolean> {
      if (!bridgeAddr) return false;
      try {
        const code = (await rpcCall(rpcUrl, "eth_getCode", [bridgeAddr, "latest"])) as string;
        if (!code || code === "0x" || code === "0x0") return false;
        // Check if initialized by calling manager()
        const result = (await rpcCall(rpcUrl, "eth_call", [
          { to: bridgeAddr, data: BRIDGE_ABI.manager },
          "latest",
        ])) as string;
        // manager() should return a non-zero address
        return !!result && result !== "0x" + "0".repeat(64);
      } catch {
        return false;
      }
    }

    async function checkBoth() {
      const [l1Ready, l2Ready] = await Promise.all([
        checkBridge(config.l1Rpc, config.l1Bridge),
        checkBridge(config.l2Rpc, config.l2Bridge),
      ]);
      if (!cancelled) {
        setState((s) => ({ ...s, l1BridgeReady: l1Ready, l2BridgeReady: l2Ready }));
      }
      return l1Ready && l2Ready;
    }

    checkBoth();

    // Retry every 10s until both bridges are ready (bridge-deployer may still
    // be running when the UI first loads)
    const interval = setInterval(async () => {
      const bothReady = await checkBoth();
      if (bothReady) clearInterval(interval);
    }, 10000);

    return () => { cancelled = true; clearInterval(interval); };
  }, []);

  // Fetch balance and allowance
  useEffect(() => {
    if (!walletAddress) {
      setState((s) => ({ ...s, sourceBalance: null, sourceBalanceRaw: null, allowance: null }));
      return;
    }

    let cancelled = false;

    async function fetchBalances() {
      const { direction, asset, tokenAddress } = stateRef.current;
      const sourceRpc = direction === "l1-to-l2" ? config.l1Rpc : config.l2Rpc;
      const bridgeAddr = direction === "l1-to-l2" ? config.l1Bridge : config.l2Bridge;

      try {
        if (asset === "eth") {
          const bal = (await rpcCall(sourceRpc, "eth_getBalance", [
            walletAddress, "latest",
          ])) as string;
          const raw = BigInt(bal);
          if (!cancelled) {
            setState((s) => ({
              ...s,
              sourceBalance: formatBalance(raw, 18),
              sourceBalanceRaw: raw,
              allowance: null,
            }));
          }
        } else if (tokenAddress && /^0x[0-9a-fA-F]{40}$/.test(tokenAddress)) {
          const decimals = stateRef.current.tokenMeta?.decimals ?? 18;

          // Fetch balance
          const balResult = (await rpcCall(sourceRpc, "eth_call", [
            {
              to: tokenAddress,
              data: ERC20_ABI.balanceOf + pad32(walletAddress!),
            },
            "latest",
          ])) as string;
          const raw = decodeUint256(balResult);

          // Fetch allowance
          let allowance = 0n;
          if (bridgeAddr) {
            const allowResult = (await rpcCall(sourceRpc, "eth_call", [
              {
                to: tokenAddress,
                data: ERC20_ABI.allowance + pad32(walletAddress!) + pad32(bridgeAddr),
              },
              "latest",
            ])) as string;
            allowance = decodeUint256(allowResult);
          }

          if (!cancelled) {
            setState((s) => ({
              ...s,
              sourceBalance: formatBalance(raw, decimals),
              sourceBalanceRaw: raw,
              allowance,
            }));
          }
        }
      } catch {
        // Silently fail — balance display just stays null
      }
    }

    fetchBalances();
    const interval = setInterval(fetchBalances, 5000);
    return () => { cancelled = true; clearInterval(interval); };
  }, [walletAddress, state.direction, state.asset, state.tokenAddress, state.tokenMeta?.decimals]);

  // Fetch token metadata on address change (debounced)
  useEffect(() => {
    const { tokenAddress, asset, direction } = state;
    if (asset !== "erc20" || !tokenAddress || !/^0x[0-9a-fA-F]{40}$/.test(tokenAddress)) {
      setState((s) => ({ ...s, tokenMeta: null }));
      return;
    }

    let cancelled = false;
    const timer = setTimeout(async () => {
      const sourceRpc = direction === "l1-to-l2" ? config.l1Rpc : config.l2Rpc;

      let name = "Unknown Token";
      let symbol = "???";
      let decimals = 18;

      try {
        const nameResult = (await rpcCall(sourceRpc, "eth_call", [
          { to: tokenAddress, data: ERC20_ABI.name }, "latest",
        ])) as string;
        const decoded = decodeString(nameResult);
        if (decoded) name = decoded;
      } catch { /* fallback */ }

      try {
        const symbolResult = (await rpcCall(sourceRpc, "eth_call", [
          { to: tokenAddress, data: ERC20_ABI.symbol }, "latest",
        ])) as string;
        const decoded = decodeString(symbolResult);
        if (decoded) symbol = decoded;
      } catch { /* fallback */ }

      try {
        const decResult = (await rpcCall(sourceRpc, "eth_call", [
          { to: tokenAddress, data: ERC20_ABI.decimals }, "latest",
        ])) as string;
        decimals = Number(decodeUint256(decResult));
        if (decimals > 77) decimals = 18; // sanity
      } catch { /* fallback */ }

      if (!cancelled) {
        const meta: TokenMeta = { name, symbol, decimals, address: tokenAddress };
        setState((s) => ({ ...s, tokenMeta: meta }));
        saveRecentToken(meta);
        setRecentTokens(loadRecentTokens());
      }
    }, 300);

    return () => { cancelled = true; clearTimeout(timer); };
  }, [state.tokenAddress, state.asset, state.direction]);

  // Gas estimation effect — runs when bridge params change
  useEffect(() => {
    const { direction, asset, amount, tokenAddress, tokenMeta } = state;
    const bridgeAddr = direction === "l1-to-l2" ? config.l1Bridge : config.l2Bridge;

    if (!bridgeAddr || !amount) {
      setState((s) => ({ ...s, gas: defaultGas }));
      return;
    }

    const decimals = asset === "eth" ? 18 : (tokenMeta?.decimals ?? 18);
    const rawAmount = parseAmount(amount, decimals);
    if (!rawAmount || rawAmount === 0n) {
      setState((s) => ({ ...s, gas: defaultGas }));
      return;
    }

    if (asset === "erc20" && (!tokenAddress || !/^0x[0-9a-fA-F]{40}$/.test(tokenAddress))) {
      setState((s) => ({ ...s, gas: defaultGas }));
      return;
    }

    let cancelled = false;
    setState((s) => ({
      ...s,
      gas: { ...defaultGas, status: "estimating" },
    }));

    const timer = setTimeout(async () => {
      const from = walletRef.current || ESTIMATION_SENDER;
      // Destination rollupId: L1→L2 uses config.rollupId (our L2), L2→L1 uses 0 (L1)
      const rollupId = direction === "l2-to-l1"
        ? "0".padStart(64, "0")
        : parseInt(config.rollupId).toString(16).padStart(64, "0");

      let data: string;
      let value: string | undefined;

      // destinationAddress: custom if set, otherwise sender's wallet
      const customDest = stateRef.current.destinationAddress;
      const dest = customDest && /^0x[0-9a-fA-F]{40}$/.test(customDest)
        ? customDest
        : from;
      const destinationAddr = pad32(dest);

      if (asset === "eth") {
        data = BRIDGE_ABI.bridgeEther + rollupId + destinationAddr;
        value = "0x" + rawAmount.toString(16);
      } else {
        data = BRIDGE_ABI.bridgeTokens + pad32(tokenAddress) + encodeUint256(rawAmount) + rollupId + destinationAddr;
      }

      try {
        let result;
        if (direction === "l1-to-l2") {
          result = await estimateCrossChainGas({
            l1Rpc: config.l1Rpc,
            proxyAddress: bridgeAddr,
            calldata: data,
            from,
            value,
          });
        } else {
          result = await estimateGas({
            rpcUrl: config.l2ProxyRpc,
            to: bridgeAddr,
            data,
            from,
            value,
          });
        }

        if (!cancelled) {
          const methodLabel =
            result.method === "direct" ? null
            : result.method === "calldata-computed" ? "L1 calldata analysis"
            : result.method === "legacy-params" ? "legacy"
            : "simulation";
          setState((s) => ({
            ...s,
            gas: {
              status: "estimated",
              estimate: Number(result.rawEstimate),
              estimateWithBuffer: Number(result.gasLimit),
              gasHex: gasToHex(result.gasLimit),
              method: methodLabel,
              errorMessage: null,
            },
          }));
        }
      } catch (e) {
        if (!cancelled) {
          setState((s) => ({
            ...s,
            gas: {
              ...defaultGas,
              status: "error",
              errorMessage: (e as Error).message || "Gas estimation failed",
            },
          }));
        }
      }
    }, 400);

    return () => { cancelled = true; clearTimeout(timer); };
  }, [state.direction, state.asset, state.amount, state.tokenAddress, state.tokenMeta?.decimals, state.destinationAddress, walletAddress]);

  const setGasOverride = useCallback((hex: string | null) => {
    setGasOverrideHex(hex);
  }, []);

  const setDirection = useCallback((dir: BridgeDirection) => {
    setState((s) => ({
      ...s,
      direction: dir,
      sourceBalance: null,
      sourceBalanceRaw: null,
      allowance: null,
      phase: "idle",
      txHash: null,
      error: null,
    }));
  }, []);

  const setAsset = useCallback((asset: BridgeAsset) => {
    setState((s) => ({
      ...s,
      asset,
      tokenAddress: "",
      tokenMeta: null,
      amount: "",
      sourceBalance: null,
      sourceBalanceRaw: null,
      allowance: null,
      phase: "idle",
      txHash: null,
      error: null,
    }));
  }, []);

  const setAmount = useCallback((amt: string) => {
    // Only allow valid decimal format
    if (amt && !/^\d*\.?\d*$/.test(amt)) return;
    setState((s) => ({ ...s, amount: amt }));
  }, []);

  const setDestination = useCallback((addr: string) => {
    setState((s) => ({ ...s, destinationAddress: addr }));
  }, []);

  const setTokenAddress = useCallback((addr: string) => {
    setState((s) => ({
      ...s,
      tokenAddress: addr,
      tokenMeta: null,
      allowance: null,
    }));
  }, []);

  const setMax = useCallback(() => {
    const { sourceBalance } = stateRef.current;
    if (sourceBalance) {
      setState((s) => ({ ...s, amount: sourceBalance }));
    }
  }, []);

  const dismiss = useCallback(() => {
    setState((s) => ({ ...s, phase: "idle", error: null, txHash: null }));
  }, []);

  /** Approve ERC20 spending for the bridge */
  const approve = useCallback(async () => {
    const { direction, tokenAddress } = stateRef.current;
    const bridgeAddr = direction === "l1-to-l2" ? config.l1Bridge : config.l2Bridge;
    if (!bridgeAddr || !tokenAddress) return;

    setState((s) => ({ ...s, phase: "approving", error: null, txHash: null }));
    log(`Approving token spending for bridge...`, "info");

    try {
      const data = ERC20_ABI.approve + pad32(bridgeAddr) + MAX_UINT256.replace("0x", "");
      const send = direction === "l1-to-l2" ? sendL1Tx : sendTx;
      const rpcUrl = direction === "l1-to-l2" ? config.l1Rpc : config.l2Rpc;

      let gasHex: string | undefined;
      try {
        const est = await estimateGas({
          rpcUrl,
          to: tokenAddress,
          data,
          from: walletRef.current || ESTIMATION_SENDER,
        });
        gasHex = gasToHex(est.gasLimit);
      } catch {
        // Estimation failed — let the node use its default
      }

      const txHash = await send({
        to: tokenAddress,
        data,
        ...(gasHex ? { gas: gasHex } : {}),
      });

      setState((s) => ({ ...s, phase: "approve-pending", txHash }));
      log(`Approval tx: ${txHash.slice(0, 18)}...`);

      for (let i = 0; i < 30; i++) {
        await new Promise((r) => setTimeout(r, 1000));
        try {
          const receipt = (await rpcCall(rpcUrl, "eth_getTransactionReceipt", [txHash])) as TxReceipt | null;
          if (receipt) {
            if (receipt.status === "0x1") {
              setState((s) => ({ ...s, phase: "idle", txHash: null, allowance: BigInt(MAX_UINT256) }));
              log("Token approval confirmed", "ok");
            } else {
              const reason = await fetchRevertReason(rpcUrl, txHash);
              setState((s) => ({
                ...s,
                phase: "failed",
                error: reason ? `Approval reverted: ${reason}` : "Approval transaction reverted",
              }));
              log("Approval reverted", "err");
            }
            return;
          }
        } catch { /* not mined */ }
      }

      setState((s) => ({ ...s, phase: "failed", error: "Approval: no receipt after 30s" }));
    } catch (e) {
      const msg = (e as Error).message;
      setState((s) => ({ ...s, phase: "failed", error: msg }));
      log(`Approval failed: ${msg}`, "err");
    }
  }, [log, sendTx, sendL1Tx]);

  /** Main bridge action */
  const bridge = useCallback(async () => {
    const { direction, asset, amount, tokenAddress, tokenMeta, destinationAddress } = stateRef.current;
    const bridgeAddr = direction === "l1-to-l2" ? config.l1Bridge : config.l2Bridge;
    if (!bridgeAddr || !amount) return;

    const decimals = asset === "eth" ? 18 : (tokenMeta?.decimals ?? 18);
    const rawAmount = parseAmount(amount, decimals);
    if (!rawAmount || rawAmount === 0n) {
      setState((s) => ({ ...s, phase: "failed", error: "Invalid amount" }));
      return;
    }

    setState((s) => ({ ...s, phase: "sending", error: null, txHash: null }));

    // Destination rollupId: L1→L2 uses config.rollupId (our L2), L2→L1 uses 0 (L1)
    const rollupId = direction === "l2-to-l1"
      ? "0".padStart(64, "0")
      : parseInt(config.rollupId).toString(16).padStart(64, "0");

    try {
      let txHash: string;

      // Use gas override if set, otherwise use pre-computed estimate from effect
      const resolvedGas = gasOverrideHex || stateRef.current.gas.gasHex;
      const gasParam: Record<string, string> = resolvedGas ? { gas: resolvedGas } : {};

      // destinationAddress: custom if set, otherwise sender's wallet
      const from = walletRef.current || ESTIMATION_SENDER;
      const dest = destinationAddress && /^0x[0-9a-fA-F]{40}$/.test(destinationAddress)
        ? destinationAddress
        : from;
      const destinationAddr = pad32(dest);

      if (asset === "eth") {
        // bridgeEther(uint256 _rollupId, address destinationAddress) payable
        const data = BRIDGE_ABI.bridgeEther + rollupId + destinationAddr;
        const value = "0x" + rawAmount.toString(16);

        if (direction === "l1-to-l2") {
          log(`Bridging ${amount} ETH L1 → L2...`, "info");
          txHash = await sendL1ProxyTx({ to: bridgeAddr, data, value, ...gasParam });
        } else {
          log(`Bridging ${amount} ETH L2 → L1...`, "info");
          txHash = await sendL2ProxyTx({ to: bridgeAddr, data, value, ...gasParam });
        }
      } else {
        // bridgeTokens(address token, uint256 amount, uint256 _rollupId, address destinationAddress)
        const data =
          BRIDGE_ABI.bridgeTokens +
          pad32(tokenAddress) +
          encodeUint256(rawAmount) +
          rollupId +
          destinationAddr;

        if (direction === "l1-to-l2") {
          log(`Bridging ${amount} ${tokenMeta?.symbol || "tokens"} L1 → L2...`, "info");
          txHash = await sendL1ProxyTx({ to: bridgeAddr, data, ...gasParam });
        } else {
          log(`Bridging ${amount} ${tokenMeta?.symbol || "tokens"} L2 → L1...`, "info");
          txHash = await sendL2ProxyTx({ to: bridgeAddr, data, ...gasParam });
        }
      }

      setState((s) => ({ ...s, phase: "tx-pending", txHash }));
      log(`Bridge tx: ${txHash.slice(0, 18)}...`);

      // Poll for receipt
      if (direction === "l1-to-l2") {
        // L1→L2: same pattern as useCrossChain — tx goes through L1 proxy
        let txSeenOnL1 = false;
        const rpcUrl = config.l1Rpc;
        for (let i = 0; i < 60; i++) {
          await new Promise((r) => setTimeout(r, 1000));
          try {
            const receipt = (await rpcCall(rpcUrl, "eth_getTransactionReceipt", [txHash])) as TxReceipt | null;
            if (receipt) {
              if (receipt.status === "0x1") {
                setState((s) => ({ ...s, phase: "confirmed", error: null }));
                log("Bridge transaction confirmed on L1 — L2 delivery atomic", "ok");
                setTimeout(() => setState((s) => (s.phase === "confirmed" ? { ...s, phase: "idle", txHash: null } : s)), 5000);
              } else {
                const reason = await fetchRevertReason(rpcUrl, txHash);
                setState((s) => ({
                  ...s,
                  phase: "failed",
                  error: reason ? `Reverted: ${reason}` : "Bridge transaction reverted",
                }));
                log(`Bridge tx reverted${reason ? `: ${reason}` : ""}`, "err");
              }
              return;
            }
          } catch { /* not mined */ }

          if (i > 0 && i % 12 === 0 && !txSeenOnL1) {
            try {
              const tx = await rpcCall(rpcUrl, "eth_getTransactionByHash", [txHash]);
              if (tx) {
                txSeenOnL1 = true;
                log("Transaction seen on L1, waiting for confirmation...");
              } else if (i >= 36) {
                setState((s) => ({
                  ...s,
                  phase: "failed",
                  error: "Transaction not broadcast to L1 — composer may be unable to submit batches",
                }));
                log("Composer may be stuck", "err");
                return;
              }
            } catch { /* ignore */ }
          }
        }

        const finalMsg = txSeenOnL1
          ? "L1 transaction pending but not confirmed after 60s"
          : "Transaction not broadcast to L1 after 60s — check composer health";
        setState((s) => ({ ...s, phase: "failed", error: finalMsg }));
        log(finalMsg, "err");
      } else {
        // L2→L1: simpler receipt polling
        const rpcUrl = config.l2Rpc;
        for (let i = 0; i < 30; i++) {
          await new Promise((r) => setTimeout(r, 1000));
          try {
            const receipt = (await rpcCall(rpcUrl, "eth_getTransactionReceipt", [txHash])) as TxReceipt | null;
            if (receipt) {
              if (receipt.status === "0x1") {
                setState((s) => ({ ...s, phase: "confirmed", error: null }));
                log("Bridge transaction confirmed on L2", "ok");
                setTimeout(() => setState((s) => (s.phase === "confirmed" ? { ...s, phase: "idle", txHash: null } : s)), 5000);
              } else {
                const reason = await fetchRevertReason(rpcUrl, txHash);
                setState((s) => ({
                  ...s,
                  phase: "failed",
                  error: reason ? `Reverted: ${reason}` : "Bridge transaction reverted",
                }));
                log(`Bridge tx reverted${reason ? `: ${reason}` : ""}`, "err");
              }
              return;
            }
          } catch { /* not mined */ }
        }

        setState((s) => ({ ...s, phase: "failed", error: "No receipt after 30s" }));
        log("Bridge tx: no receipt after 30s", "err");
      }
    } catch (e) {
      const msg = (e as Error).message;
      setState((s) => ({ ...s, phase: "failed", error: msg }));
      log(`Bridge failed: ${msg}`, "err");
    }
  }, [log, sendTx, sendL1Tx, sendL1ProxyTx, gasOverrideHex]);

  return {
    state,
    recentTokens,
    setDirection,
    setAsset,
    setAmount,
    setDestination,
    setTokenAddress,
    setMax,
    approve,
    bridge,
    dismiss,
    setGasOverride,
  };
}
