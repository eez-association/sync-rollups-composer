import type { FaucetState } from "../hooks/useFaucet";
import styles from "./FaucetPanel.module.css";

interface FaucetPanelProps {
  state: FaucetState;
  ready: boolean;
  cooldownRemaining: number;
  faucetBalance: string | null;
  walletAddress: string | null;
  onSetChain: (chain: "l1" | "l2") => void;
  onRequestFunds: () => void;
  onDismiss: () => void;
}

function formatCooldown(seconds: number): string {
  const m = Math.floor(seconds / 60);
  const s = seconds % 60;
  return `${m}:${s.toString().padStart(2, "0")}`;
}

function truncAddr(addr: string): string {
  return `${addr.slice(0, 6)}...${addr.slice(-4)}`;
}

/** Inline water droplet icon, 16px */
function DropletIcon() {
  return (
    <svg
      width="16"
      height="16"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d="M12 2.69l5.66 5.66a8 8 0 1 1-11.31 0z" />
    </svg>
  );
}

/** Small check icon for confirmed state */
function CheckIcon() {
  return (
    <svg
      width="12"
      height="12"
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth="3"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <polyline points="20 6 9 17 4 12" />
    </svg>
  );
}

export function FaucetPanel({
  state,
  ready,
  cooldownRemaining,
  faucetBalance,
  walletAddress,
  onSetChain,
  onRequestFunds,
  onDismiss,
}: FaucetPanelProps) {
  // When faucet not configured, hide entirely
  if (!ready) return null;

  const busy = state.phase === "sending" || state.phase === "tx-pending";
  const confirmed = state.phase === "confirmed";
  const failed = state.phase === "failed";
  const canRequest =
    state.phase === "idle" &&
    cooldownRemaining === 0 &&
    !!walletAddress;

  // Build strip className
  const stripClass = [
    styles.strip,
    busy ? styles.busy : "",
    confirmed ? styles.success : "",
    failed ? styles.error : "",
  ]
    .filter(Boolean)
    .join(" ");

  // Description text
  const descText = walletAddress
    ? `0.5 ETH to ${truncAddr(walletAddress)}`
    : "0.5 ETH";

  // Button label
  let btnLabel: string;
  if (busy) {
    btnLabel = state.phase === "sending" ? "Sending" : "Pending";
  } else if (cooldownRemaining > 0) {
    btnLabel = formatCooldown(cooldownRemaining);
  } else if (!walletAddress) {
    btnLabel = "No wallet";
  } else {
    btnLabel = "Request";
  }

  return (
    <div className={stripClass}>
      {/* Droplet icon */}
      <span className={styles.icon}>
        <DropletIcon />
      </span>

      {/* Label */}
      <span className={styles.label}>Faucet</span>

      {/* Separator */}
      <span className={styles.dot}>&middot;</span>

      {/* Description */}
      <span className={styles.desc}>{descText}</span>

      {/* Chain toggle pills */}
      <div className={styles.chainToggle}>
        <button
          className={`${styles.chainBtn} ${state.chain === "l1" ? styles.chainActive : ""}`}
          onClick={() => onSetChain("l1")}
          disabled={busy}
        >
          L1
        </button>
        <button
          className={`${styles.chainBtn} ${state.chain === "l2" ? styles.chainActive : ""}`}
          onClick={() => onSetChain("l2")}
          disabled={busy}
        >
          L2
        </button>
      </div>

      {/* Status indicators (replace button area when not idle) */}
      {busy && (
        <span className={`${styles.statusText} ${styles.statusPending}`}>
          <span className={styles.spinner} />
          {btnLabel}
        </span>
      )}

      {confirmed && (
        <span className={`${styles.statusText} ${styles.statusOk}`}>
          <span className={styles.checkIcon}><CheckIcon /></span>
          {state.chain === "l1" ? "Sent" : "Bridging"}
        </span>
      )}

      {failed && state.error && (
        <>
          <span className={styles.errorText} title={state.error}>
            {state.error}
          </span>
          <button className={styles.dismissX} onClick={onDismiss} title="Dismiss">
            &times;
          </button>
        </>
      )}

      {/* Request button (shown when idle or cooldown) */}
      {!busy && !confirmed && !failed && (
        <button
          className={styles.requestBtn}
          onClick={onRequestFunds}
          disabled={!canRequest}
        >
          {cooldownRemaining > 0 && (
            <span className={styles.cooldown}>{formatCooldown(cooldownRemaining)}</span>
          )}
          {cooldownRemaining === 0 && (
            <>
              <span style={{ fontSize: "11px" }}>{"\u25B6"}</span>
              {btnLabel}
            </>
          )}
        </button>
      )}

      {/* TX hash (inline, dim) */}
      {state.txHash && (
        <span className={styles.txHash} title={state.txHash}>
          {state.txHash.slice(0, 10)}...
        </span>
      )}

      {/* Faucet balance (right edge) */}
      {faucetBalance !== null && (
        <span className={styles.balance}>
          Faucet: <span className={styles.balanceAmount}>{faucetBalance} ETH</span>
        </span>
      )}
    </div>
  );
}
