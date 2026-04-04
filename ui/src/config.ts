/** Centralized configuration — overridable via URL params */

const params = new URLSearchParams(window.location.search);
const HOST = window.location.hostname;
const PROTO = window.location.protocol; // "http:" or "https:"

// When served behind an nginx reverse proxy (HTTPS on standard port),
// use path-based endpoints instead of port-based. Detect by checking
// if we're on HTTPS or a non-localhost hostname without explicit port.
const IS_PROXIED =
  PROTO === "https:" ||
  (HOST !== "localhost" && HOST !== "127.0.0.1" && !window.location.port);
const ORIGIN = IS_PROXIED
  ? `${PROTO}//${window.location.host}`
  : `http://${HOST}`;

export const config = {
  /** L1 RPC endpoint */
  l1Rpc: params.get("l1") || (IS_PROXIED ? `${ORIGIN}/rpc/l1` : `${ORIGIN}:9555`),
  /** L2 RPC endpoint (builder) */
  l2Rpc: params.get("l2") || (IS_PROXIED ? `${ORIGIN}/rpc/l2` : `${ORIGIN}:9545`),
  /** L1 proxy RPC — used as MetaMask L1 endpoint (routes cross-chain) */
  l1ProxyRpc: params.get("l1proxy") || (IS_PROXIED ? `${ORIGIN}/composer/l1` : `${ORIGIN}:9556`),
  /** L2 proxy RPC — intercepts eth_sendRawTransaction for L2→L1 cross-chain call detection */
  l2ProxyRpc: params.get("l2proxy") || (IS_PROXIED ? `${ORIGIN}/composer/l2` : `${ORIGIN}:9548`),

  /** BasedRollup contract address — loaded from /shared/rollup.env or URL param */
  rollupsAddress: params.get("rollups") || "",
  /** Rollup ID for state root queries */
  rollupId: params.get("rollupId") || "1",
  /** Block explorer base URLs (Blockscout frontends) */
  l1Explorer: params.get("l1explorer") || (IS_PROXIED ? `${PROTO}//l1.${HOST}` : `http://${HOST}:4000`),
  l2Explorer: params.get("l2explorer") || (IS_PROXIED ? `${PROTO}//l2.${HOST}` : `http://${HOST}:4001`),
  /** Blockscout backend API (for ABI fetching etc.) */
  l2ExplorerApi: params.get("l2explorerapi") || (IS_PROXIED ? `${PROTO}//l2.${HOST}` : `http://${HOST}:4003`),
  /** Bridge contract addresses */
  l1Bridge: params.get("l1bridge") || "",
  l2Bridge: params.get("l2bridge") || "",
  /** Flash loan contract addresses (loaded from rollup.env) */
  flashExecutorL1: params.get("flashExecutorL1") || "",
  flashTokenAddress: params.get("flashTokenAddress") || "",
  flashPoolAddress: params.get("flashPoolAddress") || "",
  flashNftAddress: params.get("flashNftAddress") || "",
  flashExecutorL2: params.get("flashExecutorL2") || "",
  flashWrappedTokenL2: params.get("flashWrappedTokenL2") || "",
  /** Reverse flash loan contract addresses (L2→L1 direction, loaded from rollup.env) */
  reverseExecutorL2: params.get("reverseExecutorL2") || "",
  reverseNftL1: params.get("reverseNftL1") || "",
  reverseExecutorL1: params.get("reverseExecutorL1") || "",
  /** Faucet address — loaded from /shared/rollup.env or URL param */
  faucetAddress: params.get("faucetAddress") || "",
  /** Aggregator contract addresses (loaded from rollup.env) */
  aggWeth: params.get("aggWeth") || "",
  aggUsdc: params.get("aggUsdc") || "",
  aggL1Amm: params.get("aggL1Amm") || "",
  aggAggregator: params.get("aggAggregator") || "",
  aggL2Executor: params.get("aggL2Executor") || "",
  aggL2Amm: params.get("aggL2Amm") || "",
  aggL2ExecutorProxy: params.get("aggL2ExecutorProxy") || "",
  aggWrappedWethL2: params.get("aggWrappedWethL2") || "",
  aggWrappedUsdcL2: params.get("aggWrappedUsdcL2") || "",
};

/** Mutable — set after loading env files */
export function setConfig(updates: Partial<typeof config>) {
  Object.assign(config, updates);
}

/** L1 chain definition for wallet_addEthereumChain — populated at runtime */
export const L1_CHAIN = {
  chainId: "0x539", // default 1337, auto-detected on init
  chainName: "Based Rollup L1",
  rpcUrls: [config.l1ProxyRpc],
  nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
};

/** L2 chain definition for wallet_addEthereumChain — populated at runtime.
 * Uses the L2 composer RPC (port 9548) so that L2→L1 cross-chain calls go
 * through the hold-then-forward composer which detects executeCrossChainCall
 * and queues entries before forwarding to the builder. Regular L2 txs
 * work identically through the composer (it only intercepts cross-chain calls). */
export const L2_CHAIN = {
  chainId: "0xa455", // default 42069, auto-detected on init
  chainName: "Based Rollup L2",
  rpcUrls: [config.l2ProxyRpc],
  nativeCurrency: { name: "Ether", symbol: "ETH", decimals: 18 },
};

/** Counter contract bytecode (SimpleCounter: count, increment, getCount) */
export const COUNTER_BYTECODE =
  "0x6080604052348015600e575f5ffd5b506101778061001c5f395ff3fe608060405234801561000f575f5ffd5b506004361061003f575f3560e01c806306661abd14610043578063a87d942c14610061578063d09de08a1461007f575b5f5ffd5b61004b610089565b60405161005891906100c8565b60405180910390f35b61006961008e565b60405161007691906100c8565b60405180910390f35b610087610096565b005b5f5481565b5f5f54905090565b60015f5f8282546100a7919061010e565b92505081905550565b5f819050919050565b6100c2816100b0565b82525050565b5f6020820190506100db5f8301846100b9565b92915050565b7f4e487b71000000000000000000000000000000000000000000000000000000005f52601160045260245ffd5b5f610118826100b0565b9150610123836100b0565b925082820190508082111561013b5761013a6100e1565b5b9291505056fea2646970667358221220928ed30d80bb25597bae15bbba9d2ddff597e73d3b457921d19fb425f63c421464736f6c63430008210033";

/** Counter ABI selectors */
export const COUNTER_ABI = {
  increment: "0xd09de08a",
  getCount: "0xa87d942c",
} as const;

/** Well-known anvil/reth dev account #4 — used as `from` in read-only eth_call estimation (gas, balances).
 *  Never signs transactions; only provides a funded address for simulation. */
export const ESTIMATION_SENDER = "0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65";
