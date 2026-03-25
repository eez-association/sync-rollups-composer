import { useState, useRef, useEffect, useMemo } from "react";
import type { WalletState } from "../types";
import type { HealthData } from "../hooks/useHealth";
import { L1_CHAIN, L2_CHAIN, config } from "../config";
import { ExplorerLink } from "./ExplorerLink";
import styles from "./Header.module.css";
import nhStyles from "./NodeHealth.module.css";

interface ChainData {
  blockNumber: number | null;
  txCount?: number | null;
  gasUsed?: number | null;
  gasLimit?: number | null;
  timestamp?: number | null;
  synced?: boolean | null;
}

interface Props {
  wallet: WalletState;
  onConnect: () => void;
  onDisconnect: () => void;
  onNavigate?: (view: string) => void;
  currentView?: string;
  theme?: "dark" | "light";
  onToggleTheme?: () => void;
  currentChainId?: string | null;
  onSwitchL1?: () => void;
  onSwitchL2?: () => void;
  health?: HealthData | null;
  l1?: ChainData;
  l2?: ChainData;
}

const NAV_ITEMS = [
  { id: "dashboard", label: "Dashboard" },
  { id: "visualizer", label: "Visualizer" },
];

function formatAge(ts: number, now: number): string {
  const age = now - ts;
  if (age < 0) return "now";
  if (age < 60) return `${age}s`;
  if (age < 3600) return `${Math.floor(age / 60)}m`;
  return `${Math.floor(age / 3600)}h`;
}

function formatGas(used: number, limit: number): string {
  const m = (used / 1e6).toFixed(1);
  return limit > 0 ? `${m}M (${Math.round((used / limit) * 100)}%)` : `${m}M`;
}

function ChainMini({ label, chain }: { label: "L1" | "L2"; chain?: ChainData }) {
  const isL1 = label === "L1";
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  useEffect(() => {
    const id = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000);
    return () => clearInterval(id);
  }, []);
  const age = chain?.timestamp ? formatAge(chain.timestamp, now) : null;
  const gas = useMemo(
    () => chain?.gasUsed != null && chain?.gasLimit ? formatGas(chain.gasUsed, chain.gasLimit) : null,
    [chain?.gasUsed, chain?.gasLimit],
  );

  const explorerUrl = isL1 ? config.l1Explorer : config.l2Explorer;
  const blockUrl = chain?.blockNumber != null
    ? `${explorerUrl}/block/${chain.blockNumber}`
    : undefined;

  return (
    <span className={nhStyles.chainGroup} data-chain={isL1 ? "l1" : "l2"}>
      <a
        href={explorerUrl}
        target="_blank"
        rel="noopener noreferrer"
        className={`${nhStyles.chainPill} ${isL1 ? nhStyles.pillL1 : nhStyles.pillL2}`}
      >
        {label}
      </a>
      {blockUrl ? (
        <a href={blockUrl} target="_blank" rel="noopener noreferrer" className={nhStyles.blockLink}>
          {chain!.blockNumber!.toLocaleString()}
        </a>
      ) : (
        <span className={nhStyles.blockNum}>&mdash;</span>
      )}
      {chain?.txCount != null && (
        <span className={nhStyles.meta}>{chain.txCount}{chain.txCount === 1 ? "tx" : "txs"}</span>
      )}
      {gas && <span className={`${nhStyles.meta} ${nhStyles.gas}`}>{gas}</span>}
      {age && <span className={nhStyles.age}>{age}</span>}
    </span>
  );
}

export function Header({
  wallet,
  onConnect,
  onDisconnect,
  onNavigate,
  currentView,
  theme,
  onToggleTheme,
  currentChainId,
  onSwitchL1,
  onSwitchL2,
  health,
  l1,
  l2,
}: Props) {
  const [menuOpen, setMenuOpen] = useState(false);
  const [dropdownOpen, setDropdownOpen] = useState(false);
  const dropdownRef = useRef<HTMLDivElement>(null);

  const l1Active = wallet.isConnected && currentChainId === L1_CHAIN.chainId;
  const l2Active = wallet.isConnected && currentChainId === L2_CHAIN.chainId;
  const showChainSwitcher = onSwitchL1 && onSwitchL2;

  useEffect(() => {
    if (!dropdownOpen) return;
    function handleClick(e: MouseEvent) {
      if (dropdownRef.current && !dropdownRef.current.contains(e.target as Node)) {
        setDropdownOpen(false);
      }
    }
    document.addEventListener("mousedown", handleClick);
    return () => document.removeEventListener("mousedown", handleClick);
  }, [dropdownOpen]);

  const ThemeIcon = () =>
    theme === "dark" ? (
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <circle cx="12" cy="12" r="5" />
        <line x1="12" y1="1" x2="12" y2="3" />
        <line x1="12" y1="21" x2="12" y2="23" />
        <line x1="4.22" y1="4.22" x2="5.64" y2="5.64" />
        <line x1="18.36" y1="18.36" x2="19.78" y2="19.78" />
        <line x1="1" y1="12" x2="3" y2="12" />
        <line x1="21" y1="12" x2="23" y2="12" />
        <line x1="4.22" y1="19.78" x2="5.64" y2="18.36" />
        <line x1="18.36" y1="5.64" x2="19.78" y2="4.22" />
      </svg>
    ) : (
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
        <path d="M21 12.79A9 9 0 1111.21 3 7 7 0 0021 12.79z" />
      </svg>
    );

  const shortAddr = wallet.address
    ? `${wallet.address.slice(0, 6)}...${wallet.address.slice(-4)}`
    : "";

  const shortBal = (b: string) => parseFloat(b).toFixed(2);

  const synced = l2?.synced;

  return (
      <header className={styles.header}>
        {/* ── Left: logo + desktop nav ── */}
        <div className={styles.left}>
          <div className={styles.logo}>
            <img
              src={theme === "light" ? "/logo-icon-dark.png" : "/logo-icon.png"}
              alt=""
              className={styles.logoIcon}
            />
            <span>Based Rollup</span>
          </div>

          <div className={styles.sep} />

          {onNavigate && (
            <nav className={styles.nav}>
              {NAV_ITEMS.map((item) => (
                <button
                  key={item.id}
                  className={`${styles.navLink} ${currentView === item.id ? styles.navActive : ""}`}
                  onClick={() => onNavigate(item.id)}
                >
                  {item.label}
                </button>
              ))}
            </nav>
          )}

          <div className={styles.sep} />
        </div>

        {/* ── Center: health status (inline on desktop, second row on mobile) ── */}
        <div className={styles.center}>
          {!health ? (
            <span className={nhStyles.statusGroup}>
              <span className={`${nhStyles.dot} ${nhStyles.warn}`} />
              <span className={nhStyles.statusText}>Connecting...</span>
            </span>
          ) : (
            <>
              <ChainMini label="L1" chain={l1} />
              <span className={nhStyles.sep} />
              <ChainMini label="L2" chain={l2} />

              <span className={nhStyles.rightCluster}>
                {synced != null && (
                  <span className={`${nhStyles.syncBadge} ${synced ? nhStyles.synced : nhStyles.syncing}`}>
                    {synced ? "SYNCED" : "SYNCING"}
                  </span>
                )}
                {synced != null && (
                  <span
                    className={`${nhStyles.syncDot} ${synced ? nhStyles.syncDotOk : nhStyles.syncDotWarn}`}
                    title={synced ? "Synced" : "Syncing"}
                  />
                )}
                {health.pending_submissions > 0 && (
                  <span className={nhStyles.alertBadge}>
                    {health.pending_submissions} pending
                  </span>
                )}
                {health.consecutive_rewind_cycles > 0 && (
                  <span className={nhStyles.rewindBadge}>
                    {health.consecutive_rewind_cycles} rewinds
                  </span>
                )}
                <span className={`${nhStyles.dot} ${health.healthy ? nhStyles.ok : nhStyles.err}`} />
                {health.commit && (
                  <span className={nhStyles.commit}>{health.commit.slice(0, 7)}</span>
                )}
              </span>
            </>
          )}
        </div>

        {/* ── Right: chain selector + wallet dropdown ── */}
        <div className={styles.right}>
          {showChainSwitcher && (
            <div className={styles.chainSwitcher}>
              <button
                className={`${styles.chainBtn} ${l1Active ? styles.chainBtnL1Active : ""}`}
                onClick={onSwitchL1}
                title="Switch wallet to L1"
              >
                <span className={styles.chainBtnDot} />
                L1
                {wallet.l1Balance && (
                  <>
                    <span className={styles.chainBal}>{wallet.l1Balance} ETH</span>
                    <span className={styles.chainBalShort}>{shortBal(wallet.l1Balance)}</span>
                  </>
                )}
              </button>
              <button
                className={`${styles.chainBtn} ${l2Active ? styles.chainBtnL2Active : ""}`}
                onClick={onSwitchL2}
                title="Switch wallet to L2"
              >
                <span className={styles.chainBtnDot} />
                L2
                {wallet.l2Balance && (
                  <>
                    <span className={styles.chainBal}>{wallet.l2Balance} ETH</span>
                    <span className={styles.chainBalShort}>{shortBal(wallet.l2Balance)}</span>
                  </>
                )}
              </button>
            </div>
          )}

          {/* Wallet pill + dropdown */}
          <div className={styles.walletArea} ref={dropdownRef}>
            {wallet.isConnected && wallet.address ? (
              <>
                <button
                  className={styles.walletPill}
                  onClick={() => setDropdownOpen((v) => !v)}
                >
                  <span className={styles.walletPillDot} />
                  {shortAddr}
                  <svg className={`${styles.walletPillChevron} ${dropdownOpen ? styles.walletPillChevronOpen : ""}`} width="10" height="10" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round">
                    <polyline points="6 9 12 15 18 9" />
                  </svg>
                </button>

                {dropdownOpen && (
                  <div className={styles.dropdown}>
                    <div className={styles.ddHeader}>
                      <ExplorerLink
                        value={wallet.address}
                        chain="l2"
                        className={styles.ddAddr}
                        label={shortAddr}
                      />
                    </div>

                    {onToggleTheme && (
                      <div className={styles.ddSection}>
                        <button className={styles.ddRow} onClick={onToggleTheme}>
                          <ThemeIcon />
                          <span>{theme === "dark" ? "Light" : "Dark"} mode</span>
                        </button>
                      </div>
                    )}

                    <div className={styles.ddSection}>
                      <button
                        className={`${styles.ddRow} ${styles.ddRowDanger}`}
                        onClick={() => { onDisconnect(); setDropdownOpen(false); }}
                      >
                        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
                          <path d="M9 21H5a2 2 0 01-2-2V5a2 2 0 012-2h4" />
                          <polyline points="16 17 21 12 16 7" />
                          <line x1="21" y1="12" x2="9" y2="12" />
                        </svg>
                        <span>Disconnect</span>
                      </button>
                    </div>
                  </div>
                )}
              </>
            ) : (
              <button className="btn btn-solid btn-sm" onClick={onConnect}>
                <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                  <path d="M15 3h4a2 2 0 012 2v14a2 2 0 01-2 2h-4" />
                  <polyline points="10 17 15 12 10 7" />
                  <line x1="15" y1="12" x2="3" y2="12" />
                </svg>
                Connect Wallet
              </button>
            )}
          </div>

          {/* Hamburger — mobile only */}
          <button
            className={`${styles.hamburger} ${menuOpen ? styles.hamburgerOpen : ""}`}
            onClick={() => setMenuOpen((v) => !v)}
            aria-label={menuOpen ? "Close menu" : "Open menu"}
            aria-expanded={menuOpen}
          >
            <span className={styles.hamburgerBar} />
            <span className={styles.hamburgerBar} />
            <span className={styles.hamburgerBar} />
          </button>
        </div>

        {/* ── Mobile slide-down menu ── */}
        {menuOpen && (
          <div className={styles.mobileMenu} role="dialog" aria-modal="true">
            <div className={styles.mobileBackdrop} onClick={() => setMenuOpen(false)} />

            <div className={styles.mobileMenuInner}>
              {onNavigate && (
                <div className={styles.mobileSection}>
                  <span className={styles.mobileSectionLabel}>Navigation</span>
                  {NAV_ITEMS.map((item) => (
                    <button
                      key={item.id}
                      className={`${styles.mobileNavItem} ${currentView === item.id ? styles.mobileNavActive : ""}`}
                      onClick={() => { onNavigate(item.id); setMenuOpen(false); }}
                    >
                      {item.label}
                    </button>
                  ))}
                </div>
              )}

              {showChainSwitcher && (
                <div className={styles.mobileSection}>
                  <span className={styles.mobileSectionLabel}>Switch Chain</span>
                  <div className={styles.mobileChainRow}>
                    <button
                      className={`${styles.mobileChainBtn} ${styles.mobileChainBtnL1} ${l1Active ? styles.mobileChainActive : ""}`}
                      onClick={() => { onSwitchL1!(); setMenuOpen(false); }}
                    >
                      <span className={styles.chainBtnDot} />
                      L1 &middot; {L1_CHAIN.chainName}
                    </button>
                    <button
                      className={`${styles.mobileChainBtn} ${styles.mobileChainBtnL2} ${l2Active ? styles.mobileChainActive : ""}`}
                      onClick={() => { onSwitchL2!(); setMenuOpen(false); }}
                    >
                      <span className={styles.chainBtnDot} />
                      L2 &middot; {L2_CHAIN.chainName}
                    </button>
                  </div>
                </div>
              )}

              <div className={styles.mobileSection}>
                <span className={styles.mobileSectionLabel}>Wallet</span>
                {wallet.isConnected && wallet.address ? (
                  <div className={styles.mobileWallet}>
                    <div className={styles.mobileWalletRow}>
                      <ExplorerLink
                        value={wallet.address}
                        chain="l2"
                        className={styles.ddAddr}
                        label={`${wallet.address.slice(0, 6)}...${wallet.address.slice(-4)}`}
                      />
                      <button className="btn btn-sm btn-ghost btn-red" onClick={() => { onDisconnect(); setMenuOpen(false); }}>
                        Disconnect
                      </button>
                    </div>
                  </div>
                ) : (
                  <button className="btn btn-solid btn-sm" onClick={() => { onConnect(); setMenuOpen(false); }} style={{ width: "100%" }}>
                    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
                      <path d="M15 3h4a2 2 0 012 2v14a2 2 0 01-2 2h-4" />
                      <polyline points="10 17 15 12 10 7" />
                      <line x1="15" y1="12" x2="3" y2="12" />
                    </svg>
                    Connect Wallet
                  </button>
                )}
              </div>

              {onToggleTheme && (
                <div className={styles.mobileSection}>
                  <button
                    className={`btn btn-sm btn-ghost ${styles.mobileThemeBtn}`}
                    onClick={() => { onToggleTheme(); }}
                  >
                    <ThemeIcon />
                    Switch to {theme === "dark" ? "Light" : "Dark"} Mode
                  </button>
                </div>
              )}

              {health?.commit && (
                <div className={styles.mobileCommit}>
                  Git: {health.commit.slice(0, 7)}
                </div>
              )}
            </div>
          </div>
        )}
      </header>
  );
}
