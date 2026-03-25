import { useState, useCallback } from "react";
import type { CrossChainState } from "../hooks/useCrossChain";
import { COUNTER_ABI } from "../config";
import { TxLink } from "./TxLink";
import { ExplorerLink } from "./ExplorerLink";
import styles from "./CrossChainPanel.module.css";

interface Props {
  state: CrossChainState;
  counterAddress: string;
  count: number | null;
  prevCount: number | null;
  savedProxies: Record<string, string>;
  onCreateProxy: (target: string) => void;
  onSendCall: (proxy: string, calldata: string, target?: string) => void;
  getProxy: (target: string) => string | null;
  onReset: () => void;
}

/** Preset function calls for the counter contract */
const PRESETS = [
  { label: "increment()", selector: COUNTER_ABI.increment, description: "Increase counter by 1 (cross-chain)" },
];

function PhaseIndicator({ phase }: { phase: CrossChainState["phase"] }) {
  if (phase === "idle") return null;

  const labels: Record<string, string> = {
    "creating-proxy": "Creating proxy on L1...",
    "proxy-pending": "Waiting for L1 confirmation...",
    "sending": "Sending cross-chain call...",
    "l1-pending": "Waiting for L1 confirmation — L2 state updating atomically...",
    "confirmed": "Confirmed",
    "failed": "Failed",
  };

  const isLoading = ["creating-proxy", "proxy-pending", "sending", "l1-pending"].includes(phase);
  const isSuccess = phase === "confirmed";
  const isError = phase === "failed";

  return (
    <div className={`${styles.phaseBar} ${isSuccess ? styles.phaseOk : ""} ${isError ? styles.phaseErr : ""}`}>
      {isLoading && <span className={styles.spinner} />}
      {isSuccess && (
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
      )}
      {isError && (
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" /></svg>
      )}
      <span>{labels[phase]}</span>
    </div>
  );
}

export function CrossChainPanel({
  state,
  counterAddress,
  count,
  prevCount,
  savedProxies,
  onCreateProxy,
  onSendCall,
  getProxy,
  onReset,
}: Props) {
  const [targetAddr, setTargetAddr] = useState("");
  const [customCalldata, setCustomCalldata] = useState("");
  const [selectedPreset, setSelectedPreset] = useState(0);

  // Auto-fill target with counter address if available
  const effectiveTarget = targetAddr || counterAddress;
  const existingProxy = effectiveTarget ? getProxy(effectiveTarget) : null;

  const handleCreateProxy = useCallback(() => {
    if (!effectiveTarget) return;
    onCreateProxy(effectiveTarget);
  }, [effectiveTarget, onCreateProxy]);

  const handleSendCall = useCallback(() => {
    if (!existingProxy) return;
    const calldata = customCalldata || PRESETS[selectedPreset]?.selector || "";
    if (!calldata) return;
    onSendCall(existingProxy, calldata, effectiveTarget);
  }, [existingProxy, customCalldata, selectedPreset, onSendCall, effectiveTarget]);

  const proxyEntries = Object.entries(savedProxies);
  const busy = !["idle", "confirmed", "failed"].includes(state.phase);

  return (
    <div className={styles.card}>
      <div className={styles.cardHeader}>
        <span className={styles.cardTitle}>Cross-Chain Calls</span>
        <span className={styles.subtitle}>L1 → L2 via Proxy</span>
      </div>

      <PhaseIndicator phase={state.phase} />

      {state.error && (
        <div className={styles.errorBar}>
          {state.error}
          <button className="btn btn-sm btn-ghost" onClick={onReset}>Dismiss</button>
        </div>
      )}

      {state.txHash && (
        <div className={styles.txHashRow}>
          <span className={styles.txLabel}>TX</span>
          <TxLink hash={state.txHash} chain="l1" className={styles.txValue} />
        </div>
      )}

      {/* Flow diagram */}
      <div className={styles.flowDiagram}>
        <div className={styles.flowStep}>
          <div className={styles.flowIcon}>L1</div>
          <div className={styles.flowText}>
            <strong>1. Call Proxy</strong>
            <span>Send tx to CrossChainProxy on L1</span>
          </div>
        </div>
        <div className={styles.flowArrow}>→</div>
        <div className={styles.flowStep}>
          <div className={styles.flowIcon}>P</div>
          <div className={styles.flowText}>
            <strong>2. L1 Proxy Detects</strong>
            <span>Traces tx, builds execution table</span>
          </div>
        </div>
        <div className={styles.flowArrow}>→</div>
        <div className={styles.flowStep}>
          <div className={styles.flowIcon}>L2</div>
          <div className={styles.flowText}>
            <strong>3. Atomic Execution</strong>
            <span>L2 state updated in same L1 block</span>
          </div>
        </div>
      </div>

      {/* L2 Counter value */}
      {counterAddress && (
        <div className={styles.counterDisplay}>
          <div className={styles.counterValue}>
            <span className={styles.counterNumber}>
              {count !== null ? count : "\u2014"}
            </span>
            {prevCount !== null && count !== null && count !== prevCount && (
              <span className={styles.counterDelta}>+{count - prevCount}</span>
            )}
          </div>
          <div className={styles.counterMeta}>
            <span className={styles.counterLabel}>L2 Counter Value</span>
            <ExplorerLink value={counterAddress} chain="l2" className={styles.counterAddr} />
          </div>
        </div>
      )}

      <div className={styles.sections}>
        {/* Section 1: Target + Proxy */}
        <div className={styles.section}>
          <div className={styles.sectionTitle}>Target Contract (L2)</div>
          <div className={styles.inputGroup}>
            <input
              type="text"
              className={styles.input}
              value={targetAddr}
              onChange={(e) => setTargetAddr(e.target.value)}
              placeholder={counterAddress ? `Default: ${counterAddress.slice(0, 14)}...` : "0x... L2 contract address"}
            />
            {!existingProxy ? (
              <button
                className="btn btn-solid"
                onClick={handleCreateProxy}
                disabled={!effectiveTarget || busy}
              >
                {busy ? <><span className="btn-spinner" /> Creating...</> : "Create Proxy"}
              </button>
            ) : (
              <div className={styles.proxyBadge}>
                <span className={styles.proxyDot} />
                Proxy Ready
              </div>
            )}
          </div>

          {existingProxy && (
            <div className={styles.proxyAddr}>
              <span className={styles.proxyLabel}>L1 Proxy</span>
              <ExplorerLink value={existingProxy} chain="l1" className={styles.proxyValue} />
            </div>
          )}
        </div>

        {/* Section 2: Function Call */}
        {existingProxy && (
          <div className={styles.section}>
            <div className={styles.sectionTitle}>Function Call</div>

            <div className={styles.presets}>
              {PRESETS.map((p, i) => (
                <button
                  key={p.selector}
                  className={`${styles.presetBtn} ${!customCalldata && selectedPreset === i ? styles.presetActive : ""}`}
                  onClick={() => {
                    setSelectedPreset(i);
                    setCustomCalldata("");
                  }}
                >
                  <code>{p.label}</code>
                  <span>{p.description}</span>
                </button>
              ))}
            </div>

            <div className={styles.orDivider}>
              <span>or enter custom calldata</span>
            </div>

            <input
              type="text"
              className={styles.input}
              value={customCalldata}
              onChange={(e) => setCustomCalldata(e.target.value)}
              placeholder="0x... (raw calldata bytes)"
            />

            <button
              className="btn btn-solid btn-green btn-block"
              onClick={handleSendCall}
              disabled={busy}
            >
              {busy ? (
                <><span className="btn-spinner" /> Sending...</>
              ) : (
                "Send Cross-Chain Call"
              )}
            </button>
          </div>
        )}
      </div>

      {/* Saved proxies */}
      {proxyEntries.length > 0 && (
        <div className={styles.savedSection}>
          <div className={styles.sectionTitle}>Known Proxies</div>
          <div className={styles.savedList}>
            {proxyEntries.map(([target, proxy]) => (
              <div key={target} className={styles.savedItem}>
                <span className={styles.savedTarget}>
                  L2: <ExplorerLink value={target} chain="l2" className={styles.savedTarget} />
                </span>
                <span className={styles.savedArrow}>→</span>
                <span className={styles.savedProxy}>
                  L1: <ExplorerLink value={proxy} chain="l1" className={styles.savedProxy} />
                </span>
              </div>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}
