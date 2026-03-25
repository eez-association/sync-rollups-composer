import type { BridgeState, BridgeDirection, BridgeAsset, TokenMeta } from "../hooks/useBridge";
import { config } from "../config";
import { GasLimitEditor } from "./GasLimitEditor";
import { TxLink } from "./TxLink";
import styles from "./BridgePanel.module.css";

interface Props {
  state: BridgeState;
  recentTokens: TokenMeta[];
  walletAddress: string | null;
  onSetDirection: (dir: BridgeDirection) => void;
  onSetAsset: (asset: BridgeAsset) => void;
  onSetAmount: (amt: string) => void;
  onSetDestination: (addr: string) => void;
  onSetTokenAddress: (addr: string) => void;
  onSetMax: () => void;
  onApprove: () => void;
  onBridge: () => void;
  onDismiss: () => void;
  onGasOverride: (gasHex: string | null) => void;
}

function DirectionSelector({
  direction,
  onSwap,
}: {
  direction: BridgeDirection;
  onSwap: () => void;
}) {
  const isL1Source = direction === "l1-to-l2";
  return (
    <div className={styles.directionBar}>
      <div className={styles.chainBadge}>
        <div className={styles.chainIcon}>{isL1Source ? "L1" : "L2"}</div>
        <div>
          <div className={styles.chainName}>
            {isL1Source ? "Ethereum L1" : "Rollup L2"}
          </div>
          <div className={styles.chainRole}>Source</div>
        </div>
      </div>
      <button className={styles.swapBtn} onClick={onSwap} title="Swap direction">
        <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round">
          <path d="M7 16l-4-4 4-4" /><path d="M17 8l4 4-4 4" />
          <path d="M3 12h18" />
        </svg>
      </button>
      <div className={styles.chainBadge} style={{ justifyContent: "flex-end", textAlign: "right" }}>
        <div>
          <div className={styles.chainName}>
            {isL1Source ? "Rollup L2" : "Ethereum L1"}
          </div>
          <div className={styles.chainRole}>Destination</div>
        </div>
        <div className={styles.chainIcon}>{isL1Source ? "L2" : "L1"}</div>
      </div>
    </div>
  );
}

function AssetToggle({
  asset,
  onChange,
}: {
  asset: BridgeAsset;
  onChange: (a: BridgeAsset) => void;
}) {
  return (
    <div className={styles.assetToggle}>
      <button
        className={`${styles.assetBtn} ${asset === "eth" ? styles.assetActive : ""}`}
        onClick={() => onChange("eth")}
      >
        ETH
      </button>
      <button
        className={`${styles.assetBtn} ${asset === "erc20" ? styles.assetActive : ""}`}
        onClick={() => onChange("erc20")}
      >
        ERC20
      </button>
    </div>
  );
}

function TokenInput({
  tokenAddress,
  tokenMeta,
  recentTokens,
  onAddressChange,
}: {
  tokenAddress: string;
  tokenMeta: TokenMeta | null;
  recentTokens: TokenMeta[];
  onAddressChange: (addr: string) => void;
}) {
  const isValid = !tokenAddress || /^0x[0-9a-fA-F]{40}$/.test(tokenAddress);

  return (
    <div className={styles.section}>
      <div className={styles.sectionTitle}>Token Address</div>
      <input
        type="text"
        className={styles.input}
        value={tokenAddress}
        onChange={(e) => onAddressChange(e.target.value)}
        placeholder="0x... (ERC20 token address)"
      />
      {!isValid && (
        <div className={styles.validationHint}>Enter a valid contract address</div>
      )}
      {tokenMeta && (
        <div className={styles.tokenMeta}>
          <span className={styles.tokenMetaSymbol}>{tokenMeta.symbol}</span>
          <span>{tokenMeta.name} ({tokenMeta.decimals} decimals)</span>
        </div>
      )}
      {recentTokens.length > 0 && (
        <div className={styles.recentTokens}>
          {recentTokens.map((t) => (
            <button
              key={t.address}
              className={styles.recentChip}
              onClick={() => onAddressChange(t.address)}
              title={`${t.name} (${t.symbol})`}
            >
              {t.symbol} {t.address.slice(0, 6)}...{t.address.slice(-4)}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

function AmountSection({
  amount,
  sourceBalance,
  asset,
  tokenMeta,
  sourceBalanceRaw,
  onAmountChange,
  onMax,
}: {
  amount: string;
  sourceBalance: string | null;
  asset: BridgeAsset;
  tokenMeta: TokenMeta | null;
  sourceBalanceRaw: bigint | null;
  onAmountChange: (amt: string) => void;
  onMax: () => void;
}) {
  const symbol = asset === "eth" ? "ETH" : (tokenMeta?.symbol || "tokens");
  const decimals = asset === "eth" ? 18 : (tokenMeta?.decimals ?? 18);

  // Validate amount vs balance
  let insufficientBalance = false;
  if (amount && sourceBalanceRaw !== null) {
    try {
      const parts = amount.split(".");
      const whole = parts[0] || "0";
      const frac = (parts[1] || "").padEnd(decimals, "0").slice(0, decimals);
      const rawAmount = BigInt(whole) * 10n ** BigInt(decimals) + BigInt(frac);
      if (rawAmount > sourceBalanceRaw) insufficientBalance = true;
    } catch { /* ignore */ }
  }

  return (
    <div className={styles.section}>
      <div className={styles.sectionTitle}>Amount</div>
      <div className={styles.amountRow}>
        <input
          type="text"
          className={styles.input}
          value={amount}
          onChange={(e) => onAmountChange(e.target.value)}
          placeholder={`0.0 ${symbol}`}
        />
        <button className={styles.maxBtn} onClick={onMax} disabled={!sourceBalance}>
          MAX
        </button>
      </div>
      {sourceBalance !== null && (
        <div className={styles.balanceRow}>
          Balance: <span className={styles.balanceValue}>{sourceBalance} {symbol}</span>
        </div>
      )}
      {insufficientBalance && (
        <div className={styles.validationHint}>Insufficient balance</div>
      )}
    </div>
  );
}

function ReceivePreview({
  amount,
  asset,
  tokenMeta,
  direction,
}: {
  amount: string;
  asset: BridgeAsset;
  tokenMeta: TokenMeta | null;
  direction: BridgeDirection;
}) {
  if (!amount || amount === "0") return null;
  const symbol = asset === "eth" ? "ETH" : (tokenMeta?.symbol || "tokens");
  const destChain = direction === "l1-to-l2" ? "Rollup L2" : "Ethereum L1";

  return (
    <div className={styles.receivePreview}>
      <div className={styles.receiveAmount}>~{amount} {symbol}</div>
      <div className={styles.receiveChain}>on {destChain}</div>
    </div>
  );
}

function PhaseIndicator({ phase }: { phase: string }) {
  if (phase === "idle" || phase === "confirmed") return null;

  const messages: Record<string, string> = {
    approving: "Sending approval transaction...",
    "approve-pending": "Waiting for approval confirmation...",
    sending: "Sending bridge transaction...",
    "tx-pending": "Waiting for confirmation...",
  };

  if (phase === "failed") return null; // error bar handles this

  return (
    <div className={styles.phaseBar}>
      <span className={styles.spinner} />
      <span>{messages[phase] || "Processing..."}</span>
    </div>
  );
}

export function BridgePanel({
  state,
  recentTokens,
  walletAddress,
  onSetDirection,
  onSetAsset,
  onSetAmount,
  onSetDestination,
  onSetTokenAddress,
  onSetMax,
  onApprove,
  onBridge,
  onDismiss,
  onGasOverride,
}: Props) {
  const {
    phase, direction, asset, amount, tokenAddress, tokenMeta,
    txHash, error, sourceBalance, sourceBalanceRaw, allowance,
    l1BridgeReady, l2BridgeReady, gas, destinationAddress,
  } = state;

  const busy = !["idle", "confirmed", "failed"].includes(phase);
  const sourceBridgeReady = direction === "l1-to-l2" ? l1BridgeReady : l2BridgeReady;
  const bridgeConfigured = direction === "l1-to-l2" ? !!config.l1Bridge : !!config.l2Bridge;

  // Determine if approval is needed
  const decimals = asset === "eth" ? 18 : (tokenMeta?.decimals ?? 18);
  let rawAmount = 0n;
  if (amount) {
    try {
      const parts = amount.split(".");
      const whole = parts[0] || "0";
      const frac = (parts[1] || "").padEnd(decimals, "0").slice(0, decimals);
      rawAmount = BigInt(whole) * 10n ** BigInt(decimals) + BigInt(frac);
    } catch { /* ignore */ }
  }
  const needsApproval = asset === "erc20" && allowance !== null && rawAmount > 0n && allowance < rawAmount;

  // Insufficient balance check
  let insufficientBalance = false;
  if (amount && sourceBalanceRaw !== null && rawAmount > 0n) {
    insufficientBalance = rawAmount > sourceBalanceRaw;
  }

  const canBridge =
    !busy &&
    sourceBridgeReady &&
    amount &&
    rawAmount > 0n &&
    !insufficientBalance &&
    !needsApproval &&
    (asset === "eth" || /^0x[0-9a-fA-F]{40}$/.test(tokenAddress));

  const dirLabel = direction === "l1-to-l2" ? "L1 \u2192 L2" : "L2 \u2192 L1";
  const actionLabel = asset === "eth" ? "Bridge ETH" : `Bridge ${tokenMeta?.symbol || "Tokens"}`;

  return (
    <div className={styles.card}>
      <div className={styles.cardHeader}>
        <span className={styles.cardTitle}>Bridge</span>
        <span className={styles.subtitle}>{dirLabel}</span>
      </div>

      {/* Warning: bridge not deployed */}
      {!bridgeConfigured && (
        <div className={styles.warningBar}>
          Bridge contract address not configured. Set via URL param ?l1bridge= / ?l2bridge= or rollup.env.
        </div>
      )}
      {bridgeConfigured && !sourceBridgeReady && (
        <div className={styles.warningBar}>
          Bridge contract not deployed or not initialized on {direction === "l1-to-l2" ? "L1" : "L2"}.
        </div>
      )}

      {/* Direction selector */}
      <DirectionSelector
        direction={direction}
        onSwap={() =>
          onSetDirection(direction === "l1-to-l2" ? "l2-to-l1" : "l1-to-l2")
        }
      />

      {/* Asset toggle */}
      <AssetToggle asset={asset} onChange={onSetAsset} />

      {/* Token address input (ERC20 only) */}
      {asset === "erc20" && (
        <TokenInput
          tokenAddress={tokenAddress}
          tokenMeta={tokenMeta}
          recentTokens={recentTokens}
          onAddressChange={onSetTokenAddress}
        />
      )}

      {/* Amount */}
      <AmountSection
        amount={amount}
        sourceBalance={sourceBalance}
        asset={asset}
        tokenMeta={tokenMeta}
        sourceBalanceRaw={sourceBalanceRaw}
        onAmountChange={onSetAmount}
        onMax={onSetMax}
      />

      {/* Destination address */}
      <div className={styles.section}>
        <div className={styles.sectionTitle}>Destination Address</div>
        <input
          type="text"
          className={styles.input}
          value={destinationAddress}
          onChange={(e) => onSetDestination(e.target.value)}
          placeholder={walletAddress || "0x... (defaults to your wallet)"}
          disabled={busy}
        />
        {!destinationAddress && walletAddress && (
          <div className={styles.sectionHint}>Defaults to your connected wallet</div>
        )}
      </div>

      {/* Receive preview */}
      <ReceivePreview
        amount={amount}
        asset={asset}
        tokenMeta={tokenMeta}
        direction={direction}
      />

      {/* Phase indicator */}
      <PhaseIndicator phase={phase} />

      {/* Confirmed */}
      {phase === "confirmed" && txHash && (
        <div className={`${styles.phaseBar} ${styles.phaseOk}`}>
          <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2.5" strokeLinecap="round" strokeLinejoin="round"><polyline points="20 6 9 17 4 12" /></svg>
          <span>Bridge transaction confirmed</span>
        </div>
      )}

      {/* Error bar */}
      {phase === "failed" && error && (
        <div className={styles.errorBar}>
          {error}
          <button className="btn btn-sm btn-ghost" onClick={onDismiss}>Dismiss</button>
        </div>
      )}

      {/* TX hash */}
      {txHash && (
        <div className={styles.txHashRow}>
          <span className={styles.txLabel}>TX</span>
          <TxLink
            hash={txHash}
            chain={direction === "l1-to-l2" ? "l1" : "l2"}
            className={styles.txValue}
          />
        </div>
      )}

      {/* Gas settings */}
      {amount && rawAmount > 0n && sourceBridgeReady && (
        <GasLimitEditor
          estimatedGas={gas.estimate}
          estimatedGasWithBuffer={gas.estimateWithBuffer}
          estimating={gas.status === "estimating"}
          estimationMethod={gas.method}
          onGasOverride={onGasOverride}
          disabled={busy}
        />
      )}

      {/* Approval button (ERC20 step 1) */}
      {needsApproval && !busy && (
        <>
          <div className={styles.approveNote}>Step 1 of 2 — Approve token spending</div>
          <button
            className="btn btn-solid btn-accent btn-block"
            onClick={onApprove}
            style={{ marginBottom: 8 }}
          >
            Approve {tokenMeta?.symbol || "Token"}
          </button>
        </>
      )}

      {/* Bridge button */}
      <button
        className="btn btn-solid btn-green btn-block"
        onClick={onBridge}
        disabled={!canBridge}
      >
        {busy ? (
          <><span className="btn-spinner" /> Bridging...</>
        ) : (
          actionLabel
        )}
      </button>

      {!amount && !busy && phase === "idle" && (
        <div style={{ fontSize: 11, color: "var(--text-dim)", textAlign: "center", padding: "4px 0" }}>
          Enter an amount to bridge
        </div>
      )}
    </div>
  );
}
