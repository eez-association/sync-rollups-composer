import { useState, useEffect, useCallback } from "react";
import { parseEther } from "viem";
import { config, ESTIMATION_SENDER } from "../config";
import { rpcCall } from "../rpc";
import { estimateCrossChainGas, GasEstimateError, gasToHex } from "../lib/gasEstimation";
import type { CrossChainState } from "../hooks/useCrossChain";
import type { AbiFunction } from "../hooks/useBlockscoutAbi";
import { AbiMethodSelector } from "./AbiMethodSelector";
import { GasLimitEditor } from "./GasLimitEditor";
import { TxLink } from "./TxLink";
import { ExplorerLink } from "./ExplorerLink";
import styles from "./CrossChainCallBuilder.module.css";

interface Props {
  targetAddress: string;
  proxyAddress: string | null;
  abi: AbiFunction[] | null;
  abiLoading: boolean;
  abiError: string | null;
  contractName: string | null;
  crossChainState: CrossChainState;
  onSendCall: (proxy: string, calldata: string, target?: string, value?: string, gas?: string) => void;
  onReset: () => void;
  l2Rpc: string;
  /** Sender address for gas estimation (wallet or demo) */
  senderAddress: string | null;
}

interface TxReceipt {
  status?: string;
  gasUsed?: string;
  blockNumber?: string;
}

type GasState =
  | { status: "idle" }
  | { status: "estimating" }
  | { status: "estimated"; estimate: number; gasHex: string; method: string }
  | { status: "revert"; reason: string }
  | { status: "rpc-error"; message: string };

/** Try to parse a JSON string as an ABI array of functions */
function parseManualAbi(json: string): AbiFunction[] | null {
  try {
    const parsed = JSON.parse(json);
    const arr = Array.isArray(parsed) ? parsed : [];
    const fns: AbiFunction[] = arr
      .filter((item: { type?: string }) => item.type === "function")
      .map((item: AbiFunction) => ({
        name: item.name,
        inputs: item.inputs || [],
        outputs: item.outputs || [],
        stateMutability: item.stateMutability || "nonpayable",
      }));
    return fns.length > 0 ? fns : null;
  } catch {
    return null;
  }
}

export function CrossChainCallBuilder({
  targetAddress,
  proxyAddress,
  abi,
  abiLoading,
  abiError,
  contractName,
  crossChainState,
  onSendCall,
  onReset,
  l2Rpc,
  senderAddress,
}: Props) {
  const [rawCalldata, setRawCalldata] = useState("");
  const [abiCalldata, setAbiCalldata] = useState<string | null>(null);
  const [ethValue, setEthValue] = useState("");
  const [gasState, setGasState] = useState<GasState>({ status: "idle" });
  const [gasOverrideHex, setGasOverrideHex] = useState<string | null>(null);
  const [receipt, setReceipt] = useState<TxReceipt | null>(null);
  const [showReceipt, setShowReceipt] = useState(false);

  // Manual ABI paste state
  const [manualAbiJson, setManualAbiJson] = useState("");
  const [manualAbiParsed, setManualAbIParsed] = useState<AbiFunction[] | null>(null);
  const [manualAbiError, setManualAbiError] = useState<string | null>(null);

  // Effective ABI: blockscout > manual paste > raw calldata
  const effectiveAbi = abi || manualAbiParsed;
  const calldata = effectiveAbi ? abiCalldata : rawCalldata || null;
  const busy = !["idle", "confirmed", "failed"].includes(crossChainState.phase);

  // Parse manual ABI when text changes
  useEffect(() => {
    if (!manualAbiJson.trim()) {
      setManualAbIParsed(null);
      setManualAbiError(null);
      return;
    }
    const parsed = parseManualAbi(manualAbiJson);
    if (parsed) {
      setManualAbIParsed(parsed);
      setManualAbiError(null);
    } else {
      setManualAbIParsed(null);
      setManualAbiError("Invalid ABI JSON — expected an array with function entries");
    }
  }, [manualAbiJson]);

  // Gas estimation using the robust multi-strategy estimator
  useEffect(() => {
    if (!proxyAddress || !calldata) {
      setGasState({ status: "idle" });
      return;
    }

    let cancelled = false;
    setGasState({ status: "estimating" });

    const timer = setTimeout(async () => {
      try {
        const from = senderAddress || ESTIMATION_SENDER;
        const value = ethValue ? "0x" + parseEther(ethValue).toString(16) : undefined;

        const result = await estimateCrossChainGas({
          l1Rpc: config.l1Rpc,
          proxyAddress,
          calldata,
          from,
          value,
        });

        if (!cancelled) {
          setGasState({
            status: "estimated",
            estimate: Number(result.rawEstimate),
            gasHex: gasToHex(result.gasLimit),
            method: result.method,
          });
        }
      } catch (e) {
        if (cancelled) return;
        if (e instanceof GasEstimateError) {
          if (e.type === "revert") {
            setGasState({
              status: "revert",
              reason: e.revertReason || e.message,
            });
          } else {
            setGasState({
              status: "rpc-error",
              message: e.message,
            });
          }
        } else {
          setGasState({
            status: "rpc-error",
            message: (e as Error).message || "Unknown error",
          });
        }
      }
    }, 300);

    return () => { cancelled = true; clearTimeout(timer); };
  }, [proxyAddress, calldata, ethValue, senderAddress]);

  // Fetch receipt on confirm
  useEffect(() => {
    if (crossChainState.phase !== "confirmed" || !crossChainState.txHash) {
      if (crossChainState.phase === "idle") setReceipt(null);
      return;
    }

    let cancelled = false;
    (async () => {
      try {
        const r = (await rpcCall(config.l1Rpc, "eth_getTransactionReceipt", [
          crossChainState.txHash,
        ])) as TxReceipt | null;
        if (!cancelled && r) setReceipt(r);
      } catch { /* ignore */ }
    })();
    return () => { cancelled = true; };
  }, [crossChainState.phase, crossChainState.txHash]);

  const handleGasOverride = useCallback((hex: string | null) => {
    setGasOverrideHex(hex);
  }, []);

  const handleSend = useCallback(() => {
    if (!proxyAddress || !calldata) return;
    // Use custom gas override if set, otherwise use the estimate
    const gas = gasOverrideHex
      ?? (gasState.status === "estimated" ? gasState.gasHex : undefined);
    onSendCall(
      proxyAddress,
      calldata,
      targetAddress,
      ethValue ? "0x" + parseEther(ethValue).toString(16) : undefined,
      gas,
    );
  }, [proxyAddress, calldata, targetAddress, ethValue, onSendCall, gasState, gasOverrideHex]);

  // --- No proxy: show waiting message ---
  if (!proxyAddress) {
    if (!targetAddress || !/^0x[0-9a-fA-F]{40}$/.test(targetAddress)) {
      return null; // Don't show anything until there's a valid address
    }
    return (
      <div className={styles.card}>
        <div className={styles.cardHeader}>
          <span className={styles.cardTitle}>Execute Cross-Chain Call</span>
        </div>
        <div className={styles.noProxy}>
          No L1 proxy found for this contract. Create one above to send cross-chain calls.
        </div>
      </div>
    );
  }

  // --- Proxy exists: full call builder UI ---
  const writeFnCount = effectiveAbi ? effectiveAbi.filter((f) => f.stateMutability !== "view" && f.stateMutability !== "pure").length : 0;
  const readFnCount = effectiveAbi ? effectiveAbi.filter((f) => f.stateMutability === "view" || f.stateMutability === "pure").length : 0;

  // Can only send if gas estimation didn't detect a revert
  const gasBlocked = gasState.status === "revert";

  return (
    <div className={styles.card}>
      <div className={styles.cardHeader}>
        <span className={styles.cardTitle}>Execute Cross-Chain Call</span>
        <span className={styles.subtitle}>L1 Proxy → L2</span>
      </div>

      {/* Step 1: Show the detected proxy prominently */}
      <div className={styles.proxyBanner}>
        <div className={styles.proxyBannerLeft}>
          <span className={styles.proxyDot} />
          <span className={styles.proxyBannerLabel}>L1 Proxy</span>
        </div>
        <ExplorerLink value={proxyAddress} chain="l1" className={styles.proxyBannerAddr} />
      </div>

      {/* Phase indicator */}
      {["sending", "l1-pending"].includes(crossChainState.phase) && (
        <div className={styles.phaseBar}>
          <span className={styles.spinner} />
          <span>
            {crossChainState.phase === "sending"
              ? "Sending cross-chain call via L1 proxy..."
              : "Waiting for L1 confirmation — L2 state updating atomically..."}
          </span>
        </div>
      )}

      {crossChainState.phase === "confirmed" && crossChainState.txHash && (
        <div className={`${styles.phaseBar} ${styles.phaseOk}`}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
          <span>Cross-chain call confirmed — L2 state updated</span>
        </div>
      )}

      {crossChainState.phase === "failed" && crossChainState.error && (
        <div className={styles.errorBar}>
          {crossChainState.error}
          <button className="btn btn-sm btn-ghost" onClick={onReset}>Dismiss</button>
        </div>
      )}

      {crossChainState.txHash && (
        <div className={styles.txHashRow}>
          <span className={styles.txLabel}>TX</span>
          <TxLink hash={crossChainState.txHash} chain="l1" className={styles.txValue} />
        </div>
      )}

      {/* Receipt details */}
      {receipt && crossChainState.phase === "confirmed" && (
        <div className={styles.receiptCard}>
          <button
            className={styles.receiptToggle}
            onClick={() => setShowReceipt(!showReceipt)}
          >
            <span>Transaction Receipt</span>
            <span>{showReceipt ? "\u25B2" : "\u25BC"}</span>
          </button>
          {showReceipt && (
            <div className={styles.receiptDetails}>
              {receipt.gasUsed && (
                <div className={styles.receiptRow}>
                  <span className={styles.receiptKey}>Gas Used</span>
                  <span>{parseInt(receipt.gasUsed, 16).toLocaleString()}</span>
                </div>
              )}
              {receipt.blockNumber && (
                <div className={styles.receiptRow}>
                  <span className={styles.receiptKey}>Block</span>
                  <span>{parseInt(receipt.blockNumber, 16).toLocaleString()}</span>
                </div>
              )}
              {crossChainState.txHash && (
                <div className={styles.receiptRow}>
                  <span className={styles.receiptKey}>Explorer</span>
                  <ExplorerLink value={crossChainState.txHash} type="tx" chain="l1" />
                </div>
              )}
            </div>
          )}
        </div>
      )}

      {/* Step 2: ABI status + function selection */}
      <div className={styles.section}>
        <div className={styles.sectionTitle}>
          <span className={styles.stepNumber}>1</span>
          Select Function
        </div>

        {/* ABI status banner */}
        {abiLoading ? (
          <div className={`${styles.abiStatus} ${styles.abiLoading}`}>
            <span className={styles.abiSpinner} />
            Fetching ABI from Blockscout...
          </div>
        ) : abi ? (
          <div className={`${styles.abiStatus} ${styles.abiFound}`}>
            {contractName ? <strong>{contractName}</strong> : "Verified contract"} — {writeFnCount} write, {readFnCount} read functions
          </div>
        ) : abiError ? (
          <div className={`${styles.abiStatus} ${styles.abiError}`}>
            Blockscout unavailable: {abiError}
          </div>
        ) : null}

        {/* ABI source: from Blockscout, or manual paste, or raw calldata */}
        {effectiveAbi ? (
          <AbiMethodSelector
            abi={effectiveAbi}
            targetAddress={targetAddress}
            onCalldataChange={setAbiCalldata}
            onValueChange={setEthValue}
            l2Rpc={l2Rpc}
          />
        ) : (
          <>
            {/* Manual ABI paste fallback */}
            <div className={styles.abiPasteSection}>
              <div className={styles.abiPasteHeader}>
                <span className={styles.abiPasteLabel}>
                  {!abi && !abiError ? "No verified ABI found — " : ""}Paste ABI JSON or enter raw calldata
                </span>
              </div>
              <textarea
                className={styles.abiPasteArea}
                value={manualAbiJson}
                onChange={(e) => setManualAbiJson(e.target.value)}
                placeholder={'[\n  {"type":"function","name":"increment","inputs":[],"stateMutability":"nonpayable"},\n  ...\n]'}
                rows={4}
              />
              {manualAbiError && (
                <div className={styles.abiPasteError}>{manualAbiError}</div>
              )}
            </div>

            {/* Raw calldata fallback (only if no manual ABI either) */}
            {!manualAbiParsed && (
              <>
                <div className={styles.orDivider}>
                  <span>or enter raw calldata</span>
                </div>
                <input
                  type="text"
                  className={styles.rawInput}
                  value={rawCalldata}
                  onChange={(e) => setRawCalldata(e.target.value)}
                  placeholder="0x... (function selector + encoded params)"
                />
              </>
            )}
          </>
        )}
      </div>

      {/* Step 3: Send */}
      <div className={styles.section}>
        <div className={styles.sectionTitle}>
          <span className={styles.stepNumber}>2</span>
          Send Transaction
        </div>

        {/* Gas estimation status */}
        {calldata && gasState.status === "revert" && (
          <div className={styles.gasRevert}>
            Transaction will revert: {gasState.reason}
          </div>
        )}
        {calldata && gasState.status === "rpc-error" && (
          <div className={styles.gasRpcError}>
            {gasState.message}
          </div>
        )}

        {calldata && (
          <GasLimitEditor
            estimatedGas={gasState.status === "estimated" ? gasState.estimate : null}
            estimatedGasWithBuffer={gasState.status === "estimated" ? parseInt(gasState.gasHex, 16) : null}
            estimating={gasState.status === "estimating"}
            estimationMethod={
              gasState.status === "estimated" && gasState.method !== "direct"
                ? gasState.method === "calldata-computed"
                  ? "L1 calldata analysis"
                  : gasState.method === "legacy-params"
                    ? "legacy"
                    : "simulation"
                : null
            }
            onGasOverride={handleGasOverride}
            disabled={busy}
          />
        )}

        <button
          className="btn btn-solid btn-green btn-block"
          onClick={handleSend}
          disabled={busy || !calldata || gasBlocked}
        >
          {busy ? (
            <><span className="btn-spinner" /> Sending...</>
          ) : gasBlocked ? (
            "Transaction Will Revert"
          ) : (
            "Send Cross-Chain Transaction"
          )}
        </button>

        {!calldata && !busy && (
          <div className={styles.sendHint}>
            {effectiveAbi ? "Select a write function above" : "Enter calldata above to enable sending"}
          </div>
        )}
      </div>
    </div>
  );
}
