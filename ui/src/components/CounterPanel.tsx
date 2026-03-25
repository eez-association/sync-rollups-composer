import type { TxStatus } from "../hooks/useCounter";
import { TxLink } from "./TxLink";
import styles from "./CounterPanel.module.css";

interface Props {
  address: string;
  onAddressChange: (addr: string) => void;
  count: number | null;
  prevCount: number | null;
  deploying: boolean;
  incrementing: boolean;
  txStatus: TxStatus;
  totalIncrements: number;
  onDeploy: () => void;
  onIncrement: () => void;
  onRefresh: () => void;
  connected: boolean;
}

function StepIndicator({
  step,
  active,
  done,
  label,
}: {
  step: number;
  active: boolean;
  done: boolean;
  label: string;
}) {
  return (
    <div
      className={`${styles.step} ${active ? styles.stepActive : ""} ${done ? styles.stepDone : ""}`}
    >
      <div className={styles.stepCircle}>
        {done ? (
          <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="3" strokeLinecap="round" strokeLinejoin="round">
            <polyline points="20 6 9 17 4 12" />
          </svg>
        ) : (
          step
        )}
      </div>
      <span className={styles.stepLabel}>{label}</span>
    </div>
  );
}

function TxLifecycle({ status }: { status: TxStatus }) {
  if (status.phase === "idle") return null;

  const phaseLabels = {
    sending: "Sending transaction...",
    pending: "Waiting for confirmation...",
    confirming: "Processing...",
    confirmed: "Confirmed",
    failed: "Failed",
  };

  return (
    <div
      className={`${styles.txBar} ${status.phase === "confirmed" ? styles.txConfirmed : ""} ${status.phase === "failed" ? styles.txFailed : ""}`}
    >
      <div className={styles.txPhase}>
        {(status.phase === "sending" || status.phase === "pending") && (
          <span className={styles.spinner} />
        )}
        {status.phase === "confirmed" && (
          <svg className={styles.txIcon} width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
            <polyline points="20 6 9 17 4 12" />
          </svg>
        )}
        {status.phase === "failed" && (
          <svg className={styles.txIcon} width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round">
            <line x1="18" y1="6" x2="6" y2="18" /><line x1="6" y1="6" x2="18" y2="18" />
          </svg>
        )}
        <span>{phaseLabels[status.phase]}</span>
      </div>
      <div className={styles.txMeta}>
        {status.hash && (
          <TxLink hash={status.hash} chain="l2" className={styles.txHash} />
        )}
        {status.gasUsed && (
          <span className={styles.txGas}>{status.gasUsed} gas</span>
        )}
        {status.error && (
          <span className={styles.txError}>{status.error}</span>
        )}
      </div>
    </div>
  );
}

export function CounterPanel({
  address,
  onAddressChange,
  count,
  prevCount,
  deploying,
  incrementing,
  txStatus,
  totalIncrements,
  onDeploy,
  onIncrement,
  onRefresh,
  connected,
}: Props) {
  const hasContract = address.startsWith("0x") && address.length === 42;
  const hasInteracted = totalIncrements > 0 || (count !== null && count > 0);

  return (
    <div className={styles.card}>
      <div className={styles.cardHeader}>
        <span className={styles.cardTitle}>Counter Demo</span>
        <span className={styles.status}>
          <span
            className={`${styles.dot} ${connected ? styles.ok : styles.err}`}
          />
          {connected ? "Connected to L2" : "Disconnected"}
        </span>
      </div>

      {/* Step indicators */}
      <div className={styles.steps}>
        <StepIndicator step={1} active={!hasContract} done={hasContract} label="Deploy" />
        <div className={styles.stepLine} />
        <StepIndicator step={2} active={hasContract && !hasInteracted} done={hasInteracted} label="Increment" />
        <div className={styles.stepLine} />
        <StepIndicator step={3} active={hasInteracted} done={false} label="Interact" />
      </div>

      {/* Tx lifecycle bar */}
      <TxLifecycle status={txStatus} />

      <div className={styles.grid}>
        {/* Counter display */}
        <div className={styles.display}>
          <div className={`${styles.number} ${txStatus.phase === "confirmed" ? styles.numberPop : ""}`}>
            {count !== null ? count : "\u2014"}
          </div>
          <div className={styles.label}>Current Count</div>
          {prevCount !== null && count !== null && count !== prevCount && (
            <div className={styles.delta}>
              +{count - prevCount}
            </div>
          )}
          {totalIncrements > 0 && (
            <div className={styles.sessionCount}>
              {totalIncrements} tx{totalIncrements !== 1 ? "s" : ""} this session
            </div>
          )}
        </div>

        {/* Controls */}
        <div className={styles.controls}>
          {!hasContract ? (
            <>
              <p className={styles.hint}>
                Deploy a simple counter contract to L2. This creates a contract with
                <code>increment()</code> and <code>getCount()</code> functions.
              </p>
              <div className={styles.inputRow}>
                <input
                  type="text"
                  className={styles.input}
                  value={address}
                  onChange={(e) => onAddressChange(e.target.value)}
                  placeholder="Paste existing address or deploy new..."
                />
              </div>
              <div className={styles.btnRow}>
                <button
                  className="btn btn-solid"
                  onClick={onDeploy}
                  disabled={deploying}
                >
                  {deploying ? (
                    <><span className="btn-spinner" /> Deploying...</>
                  ) : (
                    "Deploy Counter"
                  )}
                </button>
              </div>
            </>
          ) : (
            <>
              <div className={styles.contractAddr}>
                <span className={styles.contractLabel}>Contract</span>
                <span
                  className={styles.contractValue}
                  onClick={() => navigator.clipboard.writeText(address)}
                  title="Click to copy"
                >
                  {address.slice(0, 14)}...{address.slice(-10)}
                </span>
                <button
                  className="btn btn-sm btn-ghost"
                  onClick={() => {
                    onAddressChange("");
                    localStorage.removeItem("counterAddress");
                  }}
                  title="Use a different contract"
                >
                  Change
                </button>
              </div>

              <div className={styles.btnRow}>
                <button
                  className="btn btn-solid btn-green"
                  onClick={onIncrement}
                  disabled={incrementing}
                >
                  {incrementing ? (
                    <><span className="btn-spinner" /> Sending...</>
                  ) : (
                    "Increment (+1)"
                  )}
                </button>
                <button
                  className="btn btn-solid"
                  onClick={onDeploy}
                  disabled={deploying}
                >
                  {deploying ? "Deploying..." : "Deploy New"}
                </button>
                <button className="btn btn-outline" onClick={onRefresh}>
                  Refresh
                </button>
              </div>

              {!hasInteracted && (
                <p className={styles.hint}>
                  Click <strong>Increment</strong> to send a transaction that increases the counter by 1.
                  The count updates after the tx is confirmed (~12s).
                </p>
              )}
            </>
          )}
        </div>
      </div>
    </div>
  );
}
