import { useEffect, useState } from "react";
import { config, setConfig, L1_CHAIN, L2_CHAIN } from "../config";
import { rpcCall } from "../rpc";
import { registerContractsFromEnv } from "../lib/addressBook";

function parseEnv(text: string): Record<string, string> {
  const result: Record<string, string> = {};
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const eq = trimmed.indexOf("=");
    if (eq < 0) continue;
    result[trimmed.slice(0, eq).trim()] = trimmed.slice(eq + 1).trim();
  }
  return result;
}

/** Loads config from /shared/ env files and auto-detects chain IDs from RPCs */
export function useConfigLoader() {
  const [loaded, setLoaded] = useState(false);

  useEffect(() => {
    (async () => {
      // Load env file (unified rollup.env written by deploy.sh)
      try {
        const resp = await fetch("/shared/rollup.env");
        if (resp.ok) {
          const env = parseEnv(await resp.text());
          if (!config.rollupsAddress && env["ROLLUPS_ADDRESS"]) {
            setConfig({ rollupsAddress: env["ROLLUPS_ADDRESS"] });
          }
          if (env["ROLLUP_ID"]) {
            setConfig({ rollupId: env["ROLLUP_ID"] });
          }
          if (!config.l1Bridge && env["BRIDGE_L1_ADDRESS"])
            setConfig({ l1Bridge: env["BRIDGE_L1_ADDRESS"] });
          if (!config.l2Bridge && env["BRIDGE_L2_ADDRESS"])
            setConfig({ l2Bridge: env["BRIDGE_L2_ADDRESS"] });
          if (!config.flashExecutorL1 && env["FLASH_EXECUTOR_L1_ADDRESS"])
            setConfig({ flashExecutorL1: env["FLASH_EXECUTOR_L1_ADDRESS"] });
          if (!config.flashTokenAddress && env["FLASH_TOKEN_ADDRESS"])
            setConfig({ flashTokenAddress: env["FLASH_TOKEN_ADDRESS"] });
          if (!config.flashPoolAddress && env["FLASH_POOL_ADDRESS"])
            setConfig({ flashPoolAddress: env["FLASH_POOL_ADDRESS"] });
          if (!config.flashNftAddress && env["FLASH_NFT_ADDRESS"])
            setConfig({ flashNftAddress: env["FLASH_NFT_ADDRESS"] });
          if (!config.flashExecutorL2 && env["FLASH_EXECUTOR_L2_ADDRESS"])
            setConfig({ flashExecutorL2: env["FLASH_EXECUTOR_L2_ADDRESS"] });
          if (!config.flashWrappedTokenL2 && env["WRAPPED_TOKEN_L2"])
            setConfig({ flashWrappedTokenL2: env["WRAPPED_TOKEN_L2"] });
          if (!config.reverseExecutorL2 && env["REVERSE_EXECUTOR_L2"])
            setConfig({ reverseExecutorL2: env["REVERSE_EXECUTOR_L2"] });
          if (!config.reverseNftL1 && env["REVERSE_NFT_L1"])
            setConfig({ reverseNftL1: env["REVERSE_NFT_L1"] });
          if (!config.reverseExecutorL1 && env["REVERSE_EXECUTOR_L1"])
            setConfig({ reverseExecutorL1: env["REVERSE_EXECUTOR_L1"] });
          if (!config.faucetAddress && env["FAUCET_ADDRESS"])
            setConfig({ faucetAddress: env["FAUCET_ADDRESS"] });
          // Aggregator addresses
          if (!config.aggWeth && env["AGG_WETH_ADDRESS"])
            setConfig({ aggWeth: env["AGG_WETH_ADDRESS"] });
          if (!config.aggUsdc && env["AGG_USDC_ADDRESS"])
            setConfig({ aggUsdc: env["AGG_USDC_ADDRESS"] });
          if (!config.aggL1Amm && env["AGG_L1_AMM_ADDRESS"])
            setConfig({ aggL1Amm: env["AGG_L1_AMM_ADDRESS"] });
          if (!config.aggAggregator && env["AGG_AGGREGATOR_ADDRESS"])
            setConfig({ aggAggregator: env["AGG_AGGREGATOR_ADDRESS"] });
          if (!config.aggL2Executor && env["AGG_L2_EXECUTOR_ADDRESS"])
            setConfig({ aggL2Executor: env["AGG_L2_EXECUTOR_ADDRESS"] });
          if (!config.aggL2Amm && env["AGG_L2_AMM_ADDRESS"])
            setConfig({ aggL2Amm: env["AGG_L2_AMM_ADDRESS"] });
          if (!config.aggL2ExecutorProxy && env["AGG_L2_EXECUTOR_PROXY_ADDRESS"])
            setConfig({ aggL2ExecutorProxy: env["AGG_L2_EXECUTOR_PROXY_ADDRESS"] });
          if (!config.aggWrappedWethL2 && env["AGG_WRAPPED_WETH_L2"])
            setConfig({ aggWrappedWethL2: env["AGG_WRAPPED_WETH_L2"] });
          if (!config.aggWrappedUsdcL2 && env["AGG_WRAPPED_USDC_L2"])
            setConfig({ aggWrappedUsdcL2: env["AGG_WRAPPED_USDC_L2"] });
          registerContractsFromEnv(env);
        }
      } catch {
        /* shared not mounted */
      }

      // Auto-detect chain IDs from RPCs
      try {
        const l1ChainId = (await rpcCall(
          config.l1Rpc,
          "eth_chainId",
        )) as string;
        L1_CHAIN.chainId = l1ChainId;
        const dec = parseInt(l1ChainId, 16);
        L1_CHAIN.chainName = `Based Rollup L1 (${dec})`;
      } catch {
        /* keep defaults */
      }

      try {
        const l2ChainId = (await rpcCall(
          config.l2Rpc,
          "eth_chainId",
        )) as string;
        L2_CHAIN.chainId = l2ChainId;
        const dec = parseInt(l2ChainId, 16);
        L2_CHAIN.chainName = `Based Rollup L2 (${dec})`;
      } catch {
        /* keep defaults */
      }

      setLoaded(true);
    })();
  }, []);

  return loaded;
}
