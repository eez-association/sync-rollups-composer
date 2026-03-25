/**
 * Address book — maps known addresses to human-readable labels.
 * Populated at startup from rollup.env + hardcoded dev accounts.
 */

const book = new Map<string, { label: string; chain?: "l1" | "l2" }>();

/** Register a known address. */
export function registerAddress(address: string, label: string, chain?: "l1" | "l2") {
  if (!address) return;
  book.set(address.toLowerCase(), { label, chain });
}

/** Look up a label for an address. Returns undefined if unknown. */
export function lookupAddress(address: string): string | undefined {
  return book.get(address.toLowerCase())?.label;
}

/** Look up with chain context. */
export function lookupAddressForChain(address: string, _chain?: "l1" | "l2"): string | undefined {
  return book.get(address.toLowerCase())?.label;
}

// ─── Hardcoded well-known dev accounts ───

registerAddress("0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266", "Composer (dev#0)");
registerAddress("0x70997970C51812dc3A010C7d01b50e0d17dc79C8", "TxSender (dev#1)");
registerAddress("0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC", "Recipient (dev#2)");
registerAddress("0x90F79bf6EB2c4f870365E785982E1f101E93b906", "Recipient (dev#3)");
registerAddress("0x15d34AAf54267DB7D7c367839AAf71A00a2C6A65", "DemoUser (dev#4)");
registerAddress("0x9965507D1a55bcC2695C58ba16FB37d819B0A4dc", "ComplexTx (dev#5)");

/**
 * Called by useConfig after loading rollup.env to register contract addresses.
 */
export function registerContractsFromEnv(env: Record<string, string>) {
  if (env["ROLLUPS_ADDRESS"]) registerAddress(env["ROLLUPS_ADDRESS"], "Rollups", "l1");
  if (env["VERIFIER_ADDRESS"]) registerAddress(env["VERIFIER_ADDRESS"], "MockZKVerifier", "l1");
  if (env["BUILDER_ADDRESS"]) registerAddress(env["BUILDER_ADDRESS"], "Composer (dev#0)");
  if (env["L2_CONTEXT_ADDRESS"]) registerAddress(env["L2_CONTEXT_ADDRESS"], "L2Context", "l2");
  if (env["CROSS_CHAIN_MANAGER_ADDRESS"]) registerAddress(env["CROSS_CHAIN_MANAGER_ADDRESS"], "CCM (L2)", "l2");
  if (env["BRIDGE_L1_ADDRESS"]) registerAddress(env["BRIDGE_L1_ADDRESS"], "Bridge (L1)", "l1");
  if (env["BRIDGE_L2_ADDRESS"]) registerAddress(env["BRIDGE_L2_ADDRESS"], "Bridge (L2)", "l2");
}
