/**
 * ABI definitions and encoding helpers for the cross-chain aggregator contracts.
 * Uses viem for typed ABI encoding — NO hardcoded selectors.
 */
import {
  encodeFunctionData,
  type Abi,
  type Address,
} from "viem";

// ══════════════════════════════════════════════════════════════════════════
// ABI Definitions — source of truth is the Solidity contracts
// ══════════════════════════════════════════════════════════════════════════

export const wethAbi = [
  {
    type: "function",
    name: "deposit",
    inputs: [],
    outputs: [],
    stateMutability: "payable",
  },
  {
    type: "function",
    name: "withdraw",
    inputs: [{ name: "amount", type: "uint256" }],
    outputs: [],
    stateMutability: "nonpayable",
  },
] as const satisfies Abi;

export const aggregatorAbi = [
  {
    type: "function",
    name: "aggregatedSwap",
    inputs: [
      { name: "tokenIn", type: "address" },
      { name: "totalAmount", type: "uint256" },
      { name: "localAmount", type: "uint256" },
    ],
    outputs: [{ name: "totalOut", type: "uint256" }],
    stateMutability: "nonpayable",
  },
  {
    type: "function",
    name: "setL2Executor",
    inputs: [
      { name: "_executor", type: "address" },
      { name: "_proxy", type: "address" },
    ],
    outputs: [],
    stateMutability: "nonpayable",
  },
  {
    type: "function",
    name: "l2Executor",
    inputs: [],
    outputs: [{ name: "", type: "address" }],
    stateMutability: "view",
  },
  {
    type: "function",
    name: "l2ExecutorProxy",
    inputs: [],
    outputs: [{ name: "", type: "address" }],
    stateMutability: "view",
  },
] as const satisfies Abi;

export const simpleAmmAbi = [
  {
    type: "function",
    name: "reserveA",
    inputs: [],
    outputs: [{ name: "", type: "uint256" }],
    stateMutability: "view",
  },
  {
    type: "function",
    name: "reserveB",
    inputs: [],
    outputs: [{ name: "", type: "uint256" }],
    stateMutability: "view",
  },
  {
    type: "function",
    name: "getAmountOut",
    inputs: [
      { name: "tokenIn", type: "address" },
      { name: "amountIn", type: "uint256" },
    ],
    outputs: [{ name: "", type: "uint256" }],
    stateMutability: "view",
  },
  {
    type: "function",
    name: "addLiquidity",
    inputs: [
      { name: "amountA", type: "uint256" },
      { name: "amountB", type: "uint256" },
    ],
    outputs: [],
    stateMutability: "nonpayable",
  },
  {
    type: "function",
    name: "tokenA",
    inputs: [],
    outputs: [{ name: "", type: "address" }],
    stateMutability: "view",
  },
  {
    type: "function",
    name: "tokenB",
    inputs: [],
    outputs: [{ name: "", type: "address" }],
    stateMutability: "view",
  },
] as const satisfies Abi;

export const erc20Abi = [
  {
    type: "function",
    name: "balanceOf",
    inputs: [{ name: "account", type: "address" }],
    outputs: [{ name: "", type: "uint256" }],
    stateMutability: "view",
  },
  {
    type: "function",
    name: "approve",
    inputs: [
      { name: "spender", type: "address" },
      { name: "amount", type: "uint256" },
    ],
    outputs: [{ name: "", type: "bool" }],
    stateMutability: "nonpayable",
  },
  {
    type: "function",
    name: "allowance",
    inputs: [
      { name: "owner", type: "address" },
      { name: "spender", type: "address" },
    ],
    outputs: [{ name: "", type: "uint256" }],
    stateMutability: "view",
  },
  {
    type: "function",
    name: "symbol",
    inputs: [],
    outputs: [{ name: "", type: "string" }],
    stateMutability: "view",
  },
] as const satisfies Abi;

// ══════════════════════════════════════════════════════════════════════════
// Calldata builders — use viem encodeFunctionData, no hardcoded selectors
// ══════════════════════════════════════════════════════════════════════════

export function encodeDeposit(): string {
  return encodeFunctionData({ abi: wethAbi, functionName: "deposit" });
}

export function encodeWithdraw(amount: bigint): string {
  return encodeFunctionData({
    abi: wethAbi,
    functionName: "withdraw",
    args: [amount],
  });
}

export function encodeAggregatedSwap(
  tokenIn: Address,
  totalAmount: bigint,
  localAmount: bigint,
): string {
  return encodeFunctionData({
    abi: aggregatorAbi,
    functionName: "aggregatedSwap",
    args: [tokenIn, totalAmount, localAmount],
  });
}

export function encodeApprove(spender: Address, amount: bigint): string {
  return encodeFunctionData({
    abi: erc20Abi,
    functionName: "approve",
    args: [spender, amount],
  });
}

export function encodeBalanceOf(account: Address): string {
  return encodeFunctionData({
    abi: erc20Abi,
    functionName: "balanceOf",
    args: [account],
  });
}

export function encodeAllowance(owner: Address, spender: Address): string {
  return encodeFunctionData({
    abi: erc20Abi,
    functionName: "allowance",
    args: [owner, spender],
  });
}

export function encodeGetAmountOut(
  tokenIn: Address,
  amountIn: bigint,
): string {
  return encodeFunctionData({
    abi: simpleAmmAbi,
    functionName: "getAmountOut",
    args: [tokenIn, amountIn],
  });
}

export function encodeReserveA(): string {
  return encodeFunctionData({ abi: simpleAmmAbi, functionName: "reserveA" });
}

export function encodeReserveB(): string {
  return encodeFunctionData({ abi: simpleAmmAbi, functionName: "reserveB" });
}

// ══════════════════════════════════════════════════════════════════════════
// Decode helpers
// ══════════════════════════════════════════════════════════════════════════

export function decodeUint256Result(hex: string): bigint {
  const clean = hex.replace("0x", "");
  if (!clean || clean.length === 0) return 0n;
  return BigInt("0x" + clean.slice(0, 64));
}

export function formatTokenAmount(raw: bigint, decimals: number): string {
  const divisor = 10n ** BigInt(decimals);
  const whole = raw / divisor;
  const frac = raw % divisor;
  if (frac === 0n) return whole.toString();
  const fracStr = frac.toString().padStart(decimals, "0").replace(/0+$/, "");
  return `${whole}.${fracStr}`;
}

export function parseTokenAmount(amount: string, decimals: number): bigint {
  const parts = amount.split(".");
  const whole = BigInt(parts[0] || "0");
  let frac = 0n;
  if (parts[1]) {
    const fracStr = parts[1].padEnd(decimals, "0").slice(0, decimals);
    frac = BigInt(fracStr);
  }
  return whole * 10n ** BigInt(decimals) + frac;
}

/** Max uint256 for approvals */
export const MAX_UINT256 =
  115792089237316195423570985008687907853269984665640564039457584007913129639935n;

/** Gas limits (hex) */
export const WRAP_GAS = "0x30D40"; // 200,000
export const APPROVE_GAS = "0x186A0"; // 100,000
export const SWAP_GAS = "0x3D0900"; // 4,000,000
