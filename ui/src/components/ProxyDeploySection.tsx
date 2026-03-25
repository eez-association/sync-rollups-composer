import { useState, useEffect, useCallback, useRef } from "react";
import type { CrossChainState } from "../hooks/useCrossChain";
import { config } from "../config";
import { rpcCall } from "../rpc";
import { TxLink } from "./TxLink";
import { ExplorerLink } from "./ExplorerLink";
import styles from "./ProxyDeploySection.module.css";

interface Props {
  state: CrossChainState;
  targetAddress: string;
  onTargetChange: (addr: string) => void;
  contractName: string | null;
  recentAddresses: string[];
  savedProxies: Record<string, string>;
  onCreateProxy: (target: string) => void;
  getProxy: (target: string) => string | null;
  onReset: () => void;
  /** Auto-detect: compute proxy address + check if exists on-chain */
  computeProxyAddress: (target: string) => Promise<string | null>;
  /** Called when an on-chain proxy is auto-detected (lifts to parent) */
  onProxyDetected: (proxyAddr: string | null) => void;
}

/** Check if an address has code deployed on L1 */
async function hasCode(address: string): Promise<boolean> {
  try {
    const code = (await rpcCall(config.l1Rpc, "eth_getCode", [address, "latest"])) as string;
    return !!code && code !== "0x" && code !== "0x0";
  } catch {
    return false;
  }
}

export function ProxyDeploySection({
  state,
  targetAddress,
  onTargetChange,
  contractName,
  recentAddresses,
  savedProxies,
  onCreateProxy,
  getProxy,
  onReset,
  computeProxyAddress,
  onProxyDetected,
}: Props) {
  const [showRecent, setShowRecent] = useState(false);
  const [autoDetectedProxy, setAutoDetectedProxy] = useState<string | null>(null);
  const [checking, setChecking] = useState(false);
  const [deployOpen, setDeployOpen] = useState(false);
  const [proxyLiveness, setProxyLiveness] = useState<Record<string, boolean>>({});
  const dropdownRef = useRef<HTMLDivElement>(null);

  const existingProxy = targetAddress ? getProxy(targetAddress) : null;
  const proxyAddr = existingProxy || autoDetectedProxy;

  const proxyEntries = Object.entries(savedProxies);
  const proxyCount = proxyEntries.length;

  // Auto-expand deploy section if no proxies exist
  useEffect(() => {
    if (proxyCount === 0) setDeployOpen(true);
  }, [proxyCount]);

  // Check liveness of all saved proxies on mount / when savedProxies changes
  useEffect(() => {
    let cancelled = false;
    (async () => {
      const results: Record<string, boolean> = {};
      for (const [, proxy] of proxyEntries) {
        const live = await hasCode(proxy);
        if (cancelled) return;
        results[proxy] = live;
      }
      if (!cancelled) setProxyLiveness(results);
    })();
    return () => { cancelled = true; };
  }, [savedProxies]); // eslint-disable-line react-hooks/exhaustive-deps

  // Auto-detect proxy when address changes
  useEffect(() => {
    const addr = targetAddress.trim();
    if (!addr || !/^0x[0-9a-fA-F]{40}$/.test(addr)) {
      setAutoDetectedProxy(null);
      onProxyDetected(null);
      return;
    }

    if (getProxy(addr)) {
      setAutoDetectedProxy(null);
      onProxyDetected(null);
      return;
    }

    setChecking(true);
    let cancelled = false;

    (async () => {
      try {
        const computed = await computeProxyAddress(addr);
        if (cancelled) return;
        if (computed) {
          const live = await hasCode(computed);
          if (!cancelled && live) {
            setAutoDetectedProxy(computed);
            onProxyDetected(computed);
          } else if (!cancelled) {
            setAutoDetectedProxy(null);
            onProxyDetected(null);
          }
        } else if (!cancelled) {
          setAutoDetectedProxy(null);
          onProxyDetected(null);
        }
      } catch {
        if (!cancelled) {
          setAutoDetectedProxy(null);
          onProxyDetected(null);
        }
      } finally {
        if (!cancelled) setChecking(false);
      }
    })();

    return () => { cancelled = true; };
  }, [targetAddress, getProxy, computeProxyAddress]); // eslint-disable-line react-hooks/exhaustive-deps

  const handleCreateProxy = useCallback(() => {
    if (!targetAddress) return;
    onCreateProxy(targetAddress);
  }, [targetAddress, onCreateProxy]);

  const handleCallProxy = useCallback((target: string) => {
    onTargetChange(target);
    // Scroll to the CrossChainCallBuilder below
    setTimeout(() => {
      const el = document.querySelector("[data-call-builder]");
      if (el) el.scrollIntoView({ behavior: "smooth", block: "start" });
    }, 100);
  }, [onTargetChange]);

  // Close dropdown on outside click
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setShowRecent(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, []);

  const busy = !["idle", "confirmed", "failed"].includes(state.phase);
  const isProxyPhase = state.phase === "creating-proxy" || state.phase === "proxy-pending";

  return (
    <div className={styles.card}>
      {/* Header */}
      <div className={styles.cardHeader}>
        <div className={styles.headerLeft}>
          <span className={styles.cardTitle}>Cross-Chain Proxies</span>
          {proxyCount > 0 && (
            <span className={styles.countBadge}>{proxyCount}</span>
          )}
        </div>
        <button
          className={styles.deployToggle}
          onClick={() => setDeployOpen(!deployOpen)}
        >
          {deployOpen ? "- Hide" : "+ Deploy New"}
        </button>
      </div>

      {/* Proxy table */}
      {proxyCount > 0 ? (
        <div className={styles.tableWrap}>
          <table className={styles.table}>
            <thead>
              <tr>
                <th className={styles.th}>Target</th>
                <th className={styles.th}>Proxy</th>
                <th className={styles.thNarrow}>Status</th>
                <th className={styles.thNarrow}>Actions</th>
              </tr>
            </thead>
            <tbody>
              {proxyEntries.map(([target, proxy]) => {
                const live = proxyLiveness[proxy];
                return (
                  <tr key={target} className={styles.row}>
                    <td className={styles.td}>
                      <ExplorerLink value={target} chain="l2" className={styles.addrLink} />
                    </td>
                    <td className={styles.td}>
                      <ExplorerLink value={proxy} chain="l1" className={styles.addrLink} />
                    </td>
                    <td className={styles.tdNarrow}>
                      {live === undefined ? (
                        <span className={styles.statusChecking}>
                          <span className={styles.checkSpinner} />
                        </span>
                      ) : live ? (
                        <span className={styles.statusLive}>
                          <span className={styles.liveDot} />
                          Live
                        </span>
                      ) : (
                        <span className={styles.statusGone}>Gone</span>
                      )}
                    </td>
                    <td className={styles.tdNarrow}>
                      <button
                        className={styles.callBtn}
                        onClick={() => handleCallProxy(target)}
                      >
                        Call
                      </button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      ) : (
        <div className={styles.emptyState}>
          <span className={styles.emptyText}>No proxies deployed yet</span>
          {!deployOpen && (
            <span className={styles.emptyArrow}>
              {"\u2191"} Click "Deploy New" to create one
            </span>
          )}
        </div>
      )}

      {/* Collapsible deploy section */}
      <div className={`${styles.deploySection} ${deployOpen ? styles.deployOpen : styles.deployClosed}`}>
        <div className={styles.deployInner}>
          <div className={styles.deploySeparator} />

          {/* Phase indicator for proxy creation */}
          {isProxyPhase && (
            <div className={styles.phaseBar}>
              <span className={styles.spinner} />
              <span>
                {state.phase === "creating-proxy" ? "Creating proxy on L1..." : "Waiting for L1 confirmation..."}
              </span>
            </div>
          )}

          {state.phase === "confirmed" && isProxyPhase && (
            <div className={`${styles.phaseBar} ${styles.phaseOk}`}>
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
              <span>Proxy created</span>
            </div>
          )}

          {state.error && isProxyPhase && (
            <div className={styles.errorBar}>
              {state.error}
              <button className={styles.dismissBtn} onClick={onReset}>Dismiss</button>
            </div>
          )}

          {state.txHash && isProxyPhase && (
            <div className={styles.txHashRow}>
              <span className={styles.txLabel}>TX</span>
              <TxLink hash={state.txHash} chain="l1" className={styles.txValue} />
            </div>
          )}

          <div className={styles.sectionTitle}>Target Contract (L2)</div>
          <div ref={dropdownRef} className={styles.inputWrapper}>
            <div className={styles.inputGroup}>
              <input
                type="text"
                className={styles.input}
                value={targetAddress}
                onChange={(e) => onTargetChange(e.target.value)}
                onFocus={() => recentAddresses.length > 0 && setShowRecent(true)}
                placeholder="0x... L2 contract address"
              />
              {proxyAddr ? (
                <div className={styles.proxyBadge}>
                  <span className={styles.proxyDot} />
                  Proxy Exists
                </div>
              ) : checking ? (
                <div className={styles.checkingLabel}>
                  <span className={styles.checkSpinner} />
                  Checking...
                </div>
              ) : targetAddress && /^0x[0-9a-fA-F]{40}$/.test(targetAddress) ? (
                <button
                  className={styles.deployBtn}
                  onClick={handleCreateProxy}
                  disabled={busy}
                >
                  {busy ? (
                    <>
                      <span className={styles.spinner} style={{ width: 12, height: 12 }} />
                      Creating...
                    </>
                  ) : "Deploy"}
                </button>
              ) : null}
            </div>

            {showRecent && recentAddresses.length > 0 && (
              <div className={styles.recentDropdown}>
                <div className={styles.recentHeader}>Recent addresses</div>
                {recentAddresses.map((addr) => (
                  <button
                    key={addr}
                    className={styles.recentItem}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      onTargetChange(addr);
                      setShowRecent(false);
                    }}
                  >
                    {addr}
                  </button>
                ))}
              </div>
            )}
          </div>

          {contractName && (
            <div className={styles.contractBadge}>
              {contractName} (verified)
            </div>
          )}

          {proxyAddr && (
            <div className={styles.proxyAddrRow}>
              <span className={styles.proxyLabel}>Computed L1 Proxy</span>
              <ExplorerLink value={proxyAddr} chain="l1" className={styles.proxyValue} />
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
