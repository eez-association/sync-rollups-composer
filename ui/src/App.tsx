import { useCallback, useEffect, useRef, useState } from "react";
import { config } from "./config";
import { useConfigLoader } from "./hooks/useConfig";
import { useLog } from "./hooks/useLog";
import { useWallet } from "./hooks/useWallet";
import { useDashboard } from "./hooks/useDashboard";
import { useHealth } from "./hooks/useHealth";
import { useCounter } from "./hooks/useCounter";
import { useCrossChain } from "./hooks/useCrossChain";
import { useBridge } from "./hooks/useBridge";
import { useExecutionVisualizer } from "./hooks/useExecutionVisualizer";
import { useTxHistory } from "./hooks/useTxHistory";
import { useTheme } from "./hooks/useTheme";
import { useBlockscoutAbi } from "./hooks/useBlockscoutAbi";
import { useRecentAddresses } from "./hooks/useRecentAddresses";
import { useFlashLoan } from "./hooks/useFlashLoan";
import { useFlashLoanReverse } from "./hooks/useFlashLoanReverse";
import { useFlashLoanDeploy } from "./hooks/useFlashLoanDeploy";
import { useFaucet } from "./hooks/useFaucet";
import { Header } from "./components/Header";
// NodeHealth merged into Header
import { CounterPanel } from "./components/CounterPanel";
import { CrossChainPanel } from "./components/CrossChainPanel";
import { ProxyDeploySection } from "./components/ProxyDeploySection";
import { CrossChainCallBuilder } from "./components/CrossChainCallBuilder";
import { BridgePanel } from "./components/BridgePanel";
import { FlashLoanPanel } from "./components/FlashLoanPanel";
import { AggregatorPanel } from "./components/AggregatorPanel";
import { useAggregator } from "./hooks/useAggregator";
import { FaucetPanel } from "./components/FaucetPanel";
import { VisualizerView } from "./components/VisualizerView";
import { TxHistoryPanel } from "./components/TxHistoryPanel";


type DashboardTab = "dashboard" | "counter-demo" | "bridge" | "flash-loan" | "aggregator";

/** Dashboard sub-tabs that can be deep-linked via hash */
const HASH_TO_TAB: Record<string, DashboardTab> = {
  "flash-loan": "flash-loan",
  "counter-demo": "counter-demo",
  "bridge": "bridge",
  "aggregator": "aggregator",
};

function getInitialView(): string {
  const raw = window.location.hash.replace("#/", "").replace("#", "");
  const hash = raw.split("?")[0];
  if (hash === "visualizer") return "visualizer";
  if (hash === "monitor") return "visualizer"; // redirect to unified visualizer
  return "dashboard";
}

function getInitialTab(): DashboardTab {
  const raw = window.location.hash.replace("#/", "").replace("#", "");
  const hash = raw.split("?")[0] || "";
  return HASH_TO_TAB[hash] ?? "dashboard";
}

/** Parse ?key=value from the hash fragment (e.g. #/visualizer?block=123) */
function getHashParam(key: string): string | null {
  const hash = window.location.hash;
  const qIdx = hash.indexOf("?");
  if (qIdx === -1) return null;
  const params = new URLSearchParams(hash.slice(qIdx));
  return params.get(key);
}

/* ---- Sub-tab bar inline styles (matches VisualizerView.module.css .modeTabs pattern) ---- */
const subTabBarStyle: React.CSSProperties = {
  display: "flex",
  gap: 4,
  marginBottom: 20,
  padding: 4,
  background: "var(--bg)",
  border: "1px solid var(--border)",
  borderRadius: "var(--radius)",
};

const subTabStyle: React.CSSProperties = {
  flex: 1,
  fontFamily: "var(--sans)",
  fontSize: 11,
  fontWeight: 600,
  color: "var(--text-dim)",
  background: "none",
  border: "none",
  padding: "8px 16px",
  borderRadius: "var(--radius-sm, 6px)",
  cursor: "pointer",
  transition: "all 0.15s ease",
  display: "flex",
  alignItems: "center",
  justifyContent: "center",
  gap: 8,
};

const subTabActiveExtra: React.CSSProperties = {
  color: "var(--text-bright)",
  background: "var(--bg-card)",
  border: "1px solid var(--border)",
  boxShadow: "0 1px 6px rgba(0,0,0,0.15)",
};

export function App() {
  const configLoaded = useConfigLoader();
  const { theme, toggle: toggleTheme } = useTheme();
  const { entries: _entries, log } = useLog();
  const wallet = useWallet(log);
  const { l1, l2 } = useDashboard();
  const health = useHealth();
  const counter = useCounter(log, wallet.sendTx);
  const crossChain = useCrossChain(log, wallet.sendL1Tx, wallet.sendL1ProxyTx);
  const crossChainGeneric = useCrossChain(log, wallet.sendL1Tx, wallet.sendL1ProxyTx);
  const bridgeHook = useBridge(log, wallet.sendTx, wallet.sendL2ProxyTx, wallet.sendL1Tx, wallet.sendL1ProxyTx, wallet.address);
  const flashDeploy = useFlashLoanDeploy(log, wallet.sendTx, wallet.sendL1Tx, wallet.address);
  const flashLoan = useFlashLoan(log, wallet.sendL1ProxyTx, wallet.address ?? undefined, {
    executorL1: flashDeploy.state.executorL1 || undefined,
    executorL2: flashDeploy.state.executorL2 || undefined,
  });
  const flashLoanReverse = useFlashLoanReverse(log, wallet.sendL2ProxyTx);
  const faucet = useFaucet(log, wallet.address);
  const aggregator = useAggregator(log, wallet.sendL1Tx, wallet.sendL1ProxyTx, wallet.address);

  const execVis = useExecutionVisualizer();
  const txHistory = useTxHistory();

  // Dashboard tab hooks
  const [genericTargetAddr, setGenericTargetAddr] = useState<string>(() => {
    return getHashParam("target") || "";
  });
  const blockscoutAbi = useBlockscoutAbi(genericTargetAddr);
  const recentAddrs = useRecentAddresses();

  const [view, setView] = useState(getInitialView);
  const [dashboardTab, setDashboardTab] = useState<DashboardTab>(getInitialTab);

  /** Switch dashboard sub-tab and update hash for deep linking */
  const switchTab = useCallback((tab: DashboardTab) => {
    setDashboardTab(tab);
    window.location.hash = tab === "dashboard" ? "" : `#/${tab}`;
  }, []);
  const [pendingDebugHash, setPendingDebugHash] = useState<string | null>(null);
  const [initialVisualizerMode, setInitialVisualizerMode] = useState<"explorer" | "live" | "debug" | undefined>(() => {
    const raw = window.location.hash.replace("#/", "").replace("#", "").split("?")[0];
    return raw === "monitor" ? "live" : undefined;
  });
  const [initialBlock, setInitialBlock] = useState<number | null>(() => {
    const b = getHashParam("block");
    return b ? parseInt(b, 10) : null;
  });

  const navigate = useCallback((v: string) => {
    if (v === "monitor") {
      setView("visualizer");
      setInitialVisualizerMode("live");
      window.location.hash = "#/visualizer";
    } else {
      setView(v);
      setInitialVisualizerMode(undefined);
      window.location.hash = v === "dashboard" ? "" : `#/${v}`;
      // Reset to default dashboard tab when navigating to dashboard view
      if (v === "dashboard") setDashboardTab("dashboard");
    }
  }, []);

  const handleDebugTx = useCallback((txHash: string) => {
    setPendingDebugHash(txHash);
    navigate("visualizer");
  }, [navigate]);

  const handleViewBlock = useCallback((blockNumber: number) => {
    setInitialBlock(blockNumber);
    setInitialVisualizerMode("explorer");
    setView("visualizer");
    // Include block= in hash so the hashchange listener preserves it
    window.location.hash = `#/visualizer?block=${blockNumber}`;
  }, []);

  // Listen for browser back/forward
  useEffect(() => {
    const onHashChange = () => {
      const raw = window.location.hash.replace("#/", "").replace("#", "").split("?")[0];
      if (raw === "monitor") {
        setInitialVisualizerMode("live");
      }
      const b = getHashParam("block");
      setInitialBlock(b ? parseInt(b, 10) : null);
      // Check for target deep link
      const target = getHashParam("target");
      if (target) setGenericTargetAddr(target);
      // Deep link to dashboard sub-tabs (e.g. #/flash-loan, #/bridge)
      const tab = raw ? HASH_TO_TAB[raw] : undefined;
      if (tab) setDashboardTab(tab);
      setView(getInitialView());
    };
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  // Track counter demo cross-chain transactions in history
  const ccTxRef = useRef<string | null>(null);
  const prevCCPhase = useRef(crossChain.state.phase);

  useEffect(() => {
    const { phase, txHash, targetAddress, proxyAddress } = crossChain.state;

    if (
      (phase === "creating-proxy" || phase === "sending") &&
      prevCCPhase.current !== phase
    ) {
      const type = phase === "creating-proxy" ? "cross-chain-proxy" : "cross-chain-call";
      const label =
        phase === "creating-proxy"
          ? `Proxy for ${targetAddress.slice(0, 10)}...`
          : `Call → ${proxyAddress.slice(0, 10)}...`;
      ccTxRef.current = txHistory.addTx(type, label);
    }

    if (txHash && ccTxRef.current && (phase === "proxy-pending" || phase === "l1-pending")) {
      txHistory.updateTx(ccTxRef.current, { hash: txHash });
    }

    if ((phase === "confirmed" || phase === "failed") && ccTxRef.current) {
      txHistory.updateTx(ccTxRef.current, {
        status: phase === "confirmed" ? "confirmed" : "failed",
        hash: txHash ?? undefined,
      });
      ccTxRef.current = null;
    }

    prevCCPhase.current = phase;
  }, [crossChain.state.phase, crossChain.state.txHash]);

  // Track generic cross-chain transactions in history
  const ccGenTxRef = useRef<string | null>(null);
  const prevCCGenPhase = useRef(crossChainGeneric.state.phase);

  useEffect(() => {
    const { phase, txHash, targetAddress, proxyAddress } = crossChainGeneric.state;

    if (
      (phase === "creating-proxy" || phase === "sending") &&
      prevCCGenPhase.current !== phase
    ) {
      const type = phase === "creating-proxy" ? "cross-chain-proxy" : "cross-chain-call";
      const label =
        phase === "creating-proxy"
          ? `Proxy for ${targetAddress.slice(0, 10)}...`
          : `Call → ${proxyAddress.slice(0, 10)}...`;
      ccGenTxRef.current = txHistory.addTx(type, label);
    }

    if (txHash && ccGenTxRef.current && (phase === "proxy-pending" || phase === "l1-pending")) {
      txHistory.updateTx(ccGenTxRef.current, { hash: txHash });
    }

    if ((phase === "confirmed" || phase === "failed") && ccGenTxRef.current) {
      txHistory.updateTx(ccGenTxRef.current, {
        status: phase === "confirmed" ? "confirmed" : "failed",
        hash: txHash ?? undefined,
      });
      ccGenTxRef.current = null;
    }

    prevCCGenPhase.current = phase;
  }, [crossChainGeneric.state.phase, crossChainGeneric.state.txHash]);

  // Track bridge transactions in history
  const bridgeTxRef = useRef<string | null>(null);
  const prevBridgePhase = useRef(bridgeHook.state.phase);

  useEffect(() => {
    const { phase, txHash, direction, asset, amount, tokenMeta } = bridgeHook.state;

    if (phase === "sending" && prevBridgePhase.current !== "sending") {
      const symbol = asset === "eth" ? "ETH" : (tokenMeta?.symbol || "tokens");
      const dirLabel = direction === "l1-to-l2" ? "L1\u2192L2" : "L2\u2192L1";
      bridgeTxRef.current = txHistory.addTx(
        "cross-chain-call",
        `Bridge ${amount} ${symbol} ${dirLabel}`,
      );
    }

    if (txHash && bridgeTxRef.current && phase === "tx-pending") {
      txHistory.updateTx(bridgeTxRef.current, { hash: txHash });
    }

    if ((phase === "confirmed" || phase === "failed") && bridgeTxRef.current) {
      txHistory.updateTx(bridgeTxRef.current, {
        status: phase === "confirmed" ? "confirmed" : "failed",
        hash: txHash ?? undefined,
      });
      bridgeTxRef.current = null;
    }

    prevBridgePhase.current = phase;
  }, [bridgeHook.state.phase, bridgeHook.state.txHash]);

  // Track faucet transactions in history
  const faucetTxRef = useRef<string | null>(null);
  const prevFaucetPhase = useRef(faucet.state.phase);

  useEffect(() => {
    const { phase, txHash, chain } = faucet.state;

    if (phase === "sending" && prevFaucetPhase.current !== "sending") {
      const addr = wallet.address ? `${wallet.address.slice(0, 10)}...` : "?";
      faucetTxRef.current = txHistory.addTx(
        "faucet",
        `Faucet 0.5 ETH to ${addr} (${chain.toUpperCase()})`,
      );
    }

    if (txHash && faucetTxRef.current && phase === "tx-pending") {
      txHistory.updateTx(faucetTxRef.current, { hash: txHash });
    }

    if ((phase === "confirmed" || phase === "failed") && faucetTxRef.current) {
      txHistory.updateTx(faucetTxRef.current, {
        status: phase === "confirmed" ? "confirmed" : "failed",
        hash: txHash ?? undefined,
      });
      faucetTxRef.current = null;
    }

    prevFaucetPhase.current = phase;
  }, [faucet.state.phase, faucet.state.txHash]);

  // Sync execution visualizer with counter demo cross-chain phase
  useEffect(() => {
    const { phase, txHash, targetAddress, calldata, proxyAddress } = crossChain.state;
    execVis.syncWithPhase(
      phase,
      txHash,
      targetAddress,
      calldata,
      proxyAddress,
    );
  }, [crossChain.state.phase, crossChain.state.txHash]);

  // Track auto-detected proxy from ProxyDeploySection (on-chain but not in localStorage)
  const [autoDetectedProxy, setAutoDetectedProxy] = useState<string | null>(null);

  // Effective proxy: saved (localStorage) takes priority, then auto-detected (on-chain)
  const savedProxy = genericTargetAddr
    ? crossChainGeneric.getProxy(genericTargetAddr)
    : null;
  const genericProxy = savedProxy || autoDetectedProxy;

  // Wrapper for generic sendCrossChainCall that also saves to recent addresses
  const handleGenericSendCall = useCallback(
    (proxy: string, calldata: string, target?: string, _value?: string, gas?: string) => {
      if (target) recentAddrs.addAddress(target);
      crossChainGeneric.sendCrossChainCall(proxy, calldata, target, _value, gas);
    },
    [crossChainGeneric, recentAddrs],
  );

  if (!configLoaded) return null;

  return (
    <>
      <Header
        wallet={wallet}
        onConnect={wallet.connect}
        onDisconnect={wallet.disconnect}
        onNavigate={navigate}
        currentView={view}
        theme={theme}
        onToggleTheme={toggleTheme}
        currentChainId={wallet.chainId}
        onSwitchL1={wallet.switchToL1}
        onSwitchL2={wallet.switchToL2}
        health={health}
        l1={{
          blockNumber: l1.blockNumber,
          txCount: l1.txCount,
          gasUsed: l1.gasUsed,
          gasLimit: l1.gasLimit,
          timestamp: l1.timestamp,
        }}
        l2={{
          blockNumber: l2.blockNumber,
          txCount: l2.txCount,
          gasUsed: l2.gasUsed,
          gasLimit: l2.gasLimit,
          timestamp: l2.timestamp,
          synced: l2.synced,
        }}
      />

      {view === "visualizer" ? (
        <VisualizerView
          liveState={execVis.state}
          liveTargetAddress={crossChain.state.targetAddress}
          liveCalldata={crossChain.state.calldata}
          onBack={() => { setPendingDebugHash(null); setInitialVisualizerMode(undefined); setInitialBlock(null); navigate("dashboard"); }}
          initialDebugHash={pendingDebugHash}
          initialMode={initialVisualizerMode}
          initialBlock={initialBlock}
        />
      ) : (
        <main style={{ maxWidth: 1100, margin: "0 auto", padding: "24px 24px 48px" }}>
          {/* Sub-tab bar */}
          <div style={subTabBarStyle}>
            <button
              style={{
                ...subTabStyle,
                ...(dashboardTab === "dashboard" ? subTabActiveExtra : {}),
              }}
              onClick={() => switchTab("dashboard")}
            >
              Dashboard
            </button>
            <button
              style={{
                ...subTabStyle,
                ...(dashboardTab === "counter-demo" ? subTabActiveExtra : {}),
              }}
              onClick={() => switchTab("counter-demo")}
            >
              Counter Demo
            </button>
            <button
              style={{
                ...subTabStyle,
                ...(dashboardTab === "bridge" ? subTabActiveExtra : {}),
              }}
              onClick={() => switchTab("bridge")}
            >
              Bridge
            </button>
            <button
              style={{
                ...subTabStyle,
                ...(dashboardTab === "flash-loan" ? subTabActiveExtra : {}),
              }}
              onClick={() => switchTab("flash-loan")}
            >
              Flash Loan
            </button>
            <button
              style={{
                ...subTabStyle,
                ...(dashboardTab === "aggregator" ? subTabActiveExtra : {}),
              }}
              onClick={() => switchTab("aggregator")}
            >
              Aggregator
            </button>
          </div>

          {/* Tab content */}
          <div style={{ display: "flex", flexDirection: "column", gap: 16 }}>
            {dashboardTab === "dashboard" && (
              <>
                <FaucetPanel
                  state={faucet.state}
                  ready={faucet.ready}
                  cooldownRemaining={faucet.cooldownRemaining}
                  faucetBalance={faucet.faucetBalance}
                  walletAddress={wallet.address}
                  onSetChain={faucet.setChain}
                  onRequestFunds={faucet.requestFunds}
                  onDismiss={faucet.dismiss}
                />

                <ProxyDeploySection
                  state={crossChainGeneric.state}
                  targetAddress={genericTargetAddr}
                  onTargetChange={setGenericTargetAddr}
                  contractName={blockscoutAbi.contractName}
                  recentAddresses={recentAddrs.addresses}
                  savedProxies={crossChainGeneric.savedProxies}
                  onCreateProxy={crossChainGeneric.createProxy}
                  getProxy={crossChainGeneric.getProxy}
                  onReset={crossChainGeneric.reset}
                  computeProxyAddress={crossChainGeneric.computeProxyAddress}
                  onProxyDetected={setAutoDetectedProxy}
                />

                <CrossChainCallBuilder
                  targetAddress={genericTargetAddr}
                  proxyAddress={genericProxy}
                  abi={blockscoutAbi.abi}
                  abiLoading={blockscoutAbi.loading}
                  abiError={blockscoutAbi.error}
                  contractName={blockscoutAbi.contractName}
                  crossChainState={crossChainGeneric.state}
                  onSendCall={handleGenericSendCall}
                  onReset={crossChainGeneric.reset}
                  l2Rpc={config.l2Rpc}
                  senderAddress={wallet.address}
                />
              </>
            )}

            {dashboardTab === "counter-demo" && (
              <>
                <CounterPanel
                  address={counter.address}
                  onAddressChange={counter.setAddress}
                  count={counter.count}
                  prevCount={counter.prevCount}
                  deploying={counter.deploying}
                  incrementing={counter.incrementing}
                  txStatus={counter.txStatus}
                  totalIncrements={counter.totalIncrements}
                  onDeploy={counter.deploy}
                  onIncrement={counter.increment}
                  onRefresh={counter.refresh}
                  connected={l2.blockNumber !== null}
                />

                <CrossChainPanel
                  state={crossChain.state}
                  counterAddress={counter.address}
                  count={counter.count}
                  prevCount={counter.prevCount}
                  savedProxies={crossChain.savedProxies}
                  onCreateProxy={crossChain.createProxy}
                  onSendCall={crossChain.sendCrossChainCall}
                  getProxy={crossChain.getProxy}
                  onReset={crossChain.reset}
                />
              </>
            )}

            {dashboardTab === "flash-loan" && (
              <FlashLoanPanel
                state={flashLoan.state}
                reverseState={flashLoanReverse.state}
                deployState={flashDeploy.state}
                onDeploy={flashDeploy.deploy}
                onExecute={flashLoan.execute}
                onExecuteReverse={flashLoanReverse.execute}
                onReset={flashLoan.reset}
                onResetReverse={flashLoanReverse.reset}
                walletConnected={wallet.isConnected}
                walletAddress={wallet.address}
              />
            )}

            {dashboardTab === "aggregator" && (
              <AggregatorPanel
                state={aggregator.state}
                onExecute={() => aggregator.execute(aggregator.state.totalAmount, aggregator.state.splitPercent)}
                onWrapEth={aggregator.wrapEth}
                onUnwrapWeth={aggregator.unwrapWeth}
                onReset={aggregator.reset}
                onSetSplit={aggregator.setSplit}
                onSetAmount={aggregator.setAmount}
                walletConnected={wallet.isConnected}
                walletAddress={wallet.address}
              />
            )}

            {dashboardTab === "bridge" && (
              <BridgePanel
                state={bridgeHook.state}
                recentTokens={bridgeHook.recentTokens}
                walletAddress={wallet.address}
                onSetDirection={bridgeHook.setDirection}
                onSetAsset={bridgeHook.setAsset}
                onSetAmount={bridgeHook.setAmount}
                onSetDestination={bridgeHook.setDestination}
                onSetTokenAddress={bridgeHook.setTokenAddress}
                onSetMax={bridgeHook.setMax}
                onApprove={bridgeHook.approve}
                onBridge={bridgeHook.bridge}
                onDismiss={bridgeHook.dismiss}
                onGasOverride={bridgeHook.setGasOverride}
              />
            )}

            <TxHistoryPanel
              records={txHistory.records}
              onClear={txHistory.clearHistory}
              onDebug={handleDebugTx}
              onViewBlock={handleViewBlock}
            />


          </div>
        </main>
      )}
    </>
  );
}
