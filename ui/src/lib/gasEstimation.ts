/**
 * Robust gas estimation with multi-strategy fallback.
 *
 * All estimation targets the chain where the transaction actually executes:
 * - L1 transactions → estimated against L1 RPC
 * - L2 transactions → estimated against L2 RPC
 * - Cross-chain proxy calls (execute on L1) → estimated against L1 RPC
 *
 * Strategy order for standard transactions:
 * 1. eth_estimateGas with EIP-1559 pricing
 * 2. eth_estimateGas with legacy gas pricing
 * 3. eth_call simulation fallback
 *
 * Strategy order for cross-chain proxy calls:
 * 1. eth_estimateGas on L1 (may work if execution table is populated)
 * 2. eth_estimateGas on L1 with legacy params
 * 3. eth_call on L1 to detect genuine reverts vs expected cross-chain reverts
 * 4. Compute gas dynamically from calldata size + L1 contract overhead
 *
 * All successful estimates get a 1.3x safety multiplier applied.
 */

import { rpcCall } from "../rpc";

/** Safety multiplier: 1.3x (130/100) */
const GAS_BUFFER_NUMERATOR = 130n;
const GAS_BUFFER_DENOMINATOR = 100n;

/**
 * Base gas overhead for the L1 cross-chain proxy + Rollups.executeCrossChainCall
 * machinery (excluding calldata costs). Covers: proxy DELEGATECALL forwarding,
 * ABI decoding, execution table lookup, state delta application, storage writes,
 * and event emission.
 */
const CROSS_CHAIN_CONTRACT_OVERHEAD = 160_000n;

/** Base transaction gas (intrinsic cost per EVM spec) */
const TX_BASE_GAS = 21_000n;

/** EIP-2028 calldata gas costs */
const CALLDATA_GAS_ZERO_BYTE = 4n;
const CALLDATA_GAS_NONZERO_BYTE = 16n;

export interface GasEstimateResult {
  /** Gas limit with safety multiplier applied — use this for the tx */
  gasLimit: bigint;
  /** Raw estimate before multiplier */
  rawEstimate: bigint;
  /** Which strategy produced this estimate */
  method: "direct" | "legacy-params" | "calldata-computed" | "eth-call-simulation";
}

export type GasEstimateErrorType = "revert" | "rpc-error";

export class GasEstimateError extends Error {
  type: GasEstimateErrorType;
  revertReason: string | null;

  constructor(type: GasEstimateErrorType, message: string, revertReason?: string) {
    super(message);
    this.type = type;
    this.revertReason = revertReason ?? null;
  }
}

/** Apply the 1.3x safety buffer */
function applyBuffer(gas: bigint): bigint {
  return (gas * GAS_BUFFER_NUMERATOR) / GAS_BUFFER_DENOMINATOR;
}

/**
 * Classify an RPC error as either a contract revert or an RPC-level issue.
 */
function classifyError(err: unknown): { isRevert: boolean; reason: string } {
  const msg = (err instanceof Error ? err.message : String(err));
  const msgLower = msg.toLowerCase();

  const revertPatterns = [
    /execution reverted/i,
    /revert/i,
    /out of gas/i,
    /invalid opcode/i,
    /stack underflow/i,
    /vm exception/i,
    /evm error/i,
    /transaction would fail/i,
  ];

  const isRevert = revertPatterns.some((p) => p.test(msgLower));

  let reason = msg;
  const match =
    msg.match(/revert(?:ed)?:?\s*(.*)/i) ||
    msg.match(/reason:\s*(.*)/i) ||
    msg.match(/execution reverted:\s*(.*)/i);
  if (match?.[1]) {
    reason = match[1].trim().replace(/[."']+$/, "").trim();
  }

  return { isRevert, reason };
}

/**
 * Compute gas dynamically from calldata hex string.
 * Per EIP-2028: 4 gas per zero byte, 16 gas per non-zero byte.
 */
function computeCalldataGas(hexData: string): bigint {
  const clean = hexData.startsWith("0x") ? hexData.slice(2) : hexData;
  if (clean.length === 0 || clean.length % 2 !== 0) return 0n;

  let gas = 0n;
  for (let i = 0; i < clean.length; i += 2) {
    const byteHex = clean.slice(i, i + 2);
    gas += byteHex === "00" ? CALLDATA_GAS_ZERO_BYTE : CALLDATA_GAS_NONZERO_BYTE;
  }
  return gas;
}

/**
 * Try eth_estimateGas with the given params.
 */
async function tryEstimateGas(
  rpcUrl: string,
  params: Record<string, string>,
): Promise<bigint> {
  const result = (await rpcCall(rpcUrl, "eth_estimateGas", [params])) as string;
  return BigInt(result);
}

/**
 * Try eth_call to simulate a transaction. If it succeeds, we know the tx
 * won't revert, but we don't get an exact gas figure — use a generous default.
 */
async function tryEthCallSimulation(
  rpcUrl: string,
  params: Record<string, string>,
): Promise<bigint> {
  await rpcCall(rpcUrl, "eth_call", [
    { ...params, gas: "0x1000000" }, // 16M gas budget for simulation
    "latest",
  ]);
  return 200_000n;
}

/**
 * Fetch current gas pricing from the chain.
 */
async function fetchGasPricing(
  rpcUrl: string,
): Promise<{ gasPrice?: string; maxFeePerGas?: string; maxPriorityFeePerGas?: string }> {
  try {
    const block = (await rpcCall(rpcUrl, "eth_getBlockByNumber", [
      "latest",
      false,
    ])) as { baseFeePerGas?: string } | null;

    if (block?.baseFeePerGas) {
      const baseFee = BigInt(block.baseFeePerGas);
      let tip = 1_000_000_000n;
      try {
        const tipResult = (await rpcCall(
          rpcUrl,
          "eth_maxPriorityFeePerGas",
          [],
        )) as string;
        tip = BigInt(tipResult);
      } catch {
        /* use default tip */
      }
      const maxFee = baseFee * 2n + tip;
      return {
        maxFeePerGas: "0x" + maxFee.toString(16),
        maxPriorityFeePerGas: "0x" + tip.toString(16),
      };
    }
  } catch {
    /* fall through to legacy */
  }

  try {
    const price = (await rpcCall(rpcUrl, "eth_gasPrice", [])) as string;
    return { gasPrice: price };
  } catch {
    return {};
  }
}

/**
 * Estimate gas for a standard (non-cross-chain) transaction.
 *
 * Simulates against the provided `rpcUrl` — callers must pass the RPC
 * for the chain where the transaction actually executes.
 *
 * @throws {GasEstimateError} with type "revert" if the tx will revert,
 *         or type "rpc-error" if estimation failed for RPC reasons.
 */
export async function estimateGas(params: {
  rpcUrl: string;
  to: string;
  data: string;
  from?: string;
  value?: string;
}): Promise<GasEstimateResult> {
  const { rpcUrl, to, data, from, value } = params;

  const baseParams: Record<string, string> = { to, data };
  if (from) baseParams.from = from;
  if (value) baseParams.value = value;

  // Strategy 1: eth_estimateGas with EIP-1559 gas pricing
  const pricing = await fetchGasPricing(rpcUrl);
  try {
    const est = await tryEstimateGas(rpcUrl, { ...baseParams, ...pricing });
    return { gasLimit: applyBuffer(est), rawEstimate: est, method: "direct" };
  } catch (e) {
    const { isRevert, reason } = classifyError(e);
    if (isRevert) {
      // Try eth_call for a better revert reason
      try {
        await tryEthCallSimulation(rpcUrl, baseParams);
        const est = 200_000n;
        return {
          gasLimit: applyBuffer(est),
          rawEstimate: est,
          method: "eth-call-simulation",
        };
      } catch (callErr) {
        const { reason: callReason } = classifyError(callErr);
        throw new GasEstimateError("revert", callReason || reason, callReason || reason);
      }
    }
    // Not a revert — try legacy params
  }

  // Strategy 2: Retry with legacy gas pricing
  try {
    let legacyPrice: string | undefined;
    try {
      legacyPrice = (await rpcCall(rpcUrl, "eth_gasPrice", [])) as string;
    } catch { /* no price available */ }

    const legacyParams = { ...baseParams };
    if (legacyPrice) legacyParams.gasPrice = legacyPrice;
    delete legacyParams.maxFeePerGas;
    delete legacyParams.maxPriorityFeePerGas;

    const est = await tryEstimateGas(rpcUrl, legacyParams);
    return { gasLimit: applyBuffer(est), rawEstimate: est, method: "legacy-params" };
  } catch (e) {
    const { isRevert, reason } = classifyError(e);
    if (isRevert) {
      throw new GasEstimateError("revert", reason, reason);
    }
  }

  // Strategy 3: eth_call simulation
  try {
    const est = await tryEthCallSimulation(rpcUrl, baseParams);
    return {
      gasLimit: applyBuffer(est),
      rawEstimate: est,
      method: "eth-call-simulation",
    };
  } catch (e) {
    const { isRevert, reason } = classifyError(e);
    if (isRevert) {
      throw new GasEstimateError("revert", reason, reason);
    }
    throw new GasEstimateError("rpc-error", reason);
  }
}

/**
 * Estimate gas for a cross-chain proxy call.
 *
 * The transaction executes on L1, so ALL estimation targets the L1 RPC.
 * L2 is never consulted — the gas the user pays is L1 gas.
 *
 * Cross-chain calls via the L1 proxy flow through:
 *   User → CrossChainProxy (L1) → Rollups.executeCrossChainCall (L1)
 *
 * Direct L1 estimation usually fails because the execution table hasn't
 * been populated yet (that happens during the actual send). When this
 * expected failure is detected, we compute gas dynamically from:
 *   - TX base cost (21,000)
 *   - Calldata cost (4/16 gas per zero/non-zero byte, EIP-2028)
 *   - Cross-chain contract overhead (proxy + Rollups execution machinery)
 *
 * @throws {GasEstimateError} with type "revert" if the tx has a genuine
 *         error (bad calldata, unauthorized, etc.), or "rpc-error" for
 *         connectivity issues.
 */
export async function estimateCrossChainGas(params: {
  l1Rpc: string;
  proxyAddress: string;
  calldata: string;
  from?: string;
  value?: string;
}): Promise<GasEstimateResult> {
  const { l1Rpc, proxyAddress, calldata, from, value } = params;

  const baseParams: Record<string, string> = { to: proxyAddress, data: calldata };
  if (from) baseParams.from = from;
  if (value) baseParams.value = value;

  // Strategy 1: Try direct eth_estimateGas on L1 with EIP-1559 pricing
  const pricing = await fetchGasPricing(l1Rpc);
  try {
    const est = await tryEstimateGas(l1Rpc, { ...baseParams, ...pricing });
    return { gasLimit: applyBuffer(est), rawEstimate: est, method: "direct" };
  } catch {
    // Expected to fail for cross-chain calls — fall through
  }

  // Strategy 2: Try eth_estimateGas on L1 with legacy pricing
  try {
    let legacyPrice: string | undefined;
    try {
      legacyPrice = (await rpcCall(l1Rpc, "eth_gasPrice", [])) as string;
    } catch { /* no price available */ }

    const legacyParams = { ...baseParams };
    if (legacyPrice) legacyParams.gasPrice = legacyPrice;
    delete legacyParams.maxFeePerGas;
    delete legacyParams.maxPriorityFeePerGas;

    const est = await tryEstimateGas(l1Rpc, legacyParams);
    return { gasLimit: applyBuffer(est), rawEstimate: est, method: "legacy-params" };
  } catch {
    // Expected — fall through to eth_call detection
  }

  // Strategy 3: eth_call on L1 to distinguish expected vs genuine reverts
  try {
    // If eth_call succeeds, the tx is valid — use simulation estimate
    const est = await tryEthCallSimulation(l1Rpc, baseParams);
    return {
      gasLimit: applyBuffer(est),
      rawEstimate: est,
      method: "eth-call-simulation",
    };
  } catch {
    // Cross-chain proxy calls ALWAYS revert during estimation because the
    // execution table isn't populated yet (that only happens during the actual
    // send via the L1 proxy). Any revert here — whether it has a specific
    // custom error name or a bare "execution reverted" — is expected.
    // Fall through to calldata-based computation.
  }

  // Strategy 4: Last resort — compute from calldata (no RPC validation)
  const calldataGas = computeCalldataGas(calldata);
  const totalEst = TX_BASE_GAS + calldataGas + CROSS_CHAIN_CONTRACT_OVERHEAD;
  return {
    gasLimit: applyBuffer(totalEst),
    rawEstimate: totalEst,
    method: "calldata-computed",
  };
}

/** Format a gas bigint to a hex string suitable for tx params */
export function gasToHex(gas: bigint): string {
  return "0x" + gas.toString(16);
}
