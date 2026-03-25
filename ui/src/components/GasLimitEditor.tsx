import { useState, useEffect, useCallback } from "react";
import styles from "./GasLimitEditor.module.css";

const MIN_GAS = 21_000;
const MAX_GAS = 60_000_000; // block gas limit
const LOW_GAS_THRESHOLD = 0.7; // warn if custom < 70% of estimate

interface Props {
  /** Estimated gas limit (raw estimate, before buffer) — null if not yet estimated */
  estimatedGas: number | null;
  /** Estimated gas limit with buffer applied (the value that would be sent) */
  estimatedGasWithBuffer: number | null;
  /** Whether estimation is in progress */
  estimating: boolean;
  /** Estimation method label (e.g. "L1 calldata analysis") — null to hide */
  estimationMethod: string | null;
  /** Called with the gas hex string to use, or null to use the estimate */
  onGasOverride: (gasHex: string | null) => void;
  /** Whether the parent form is busy / disabled */
  disabled?: boolean;
}

export function GasLimitEditor({
  estimatedGas,
  estimatedGasWithBuffer,
  estimating,
  estimationMethod,
  onGasOverride,
  disabled,
}: Props) {
  const [expanded, setExpanded] = useState(false);
  const [customValue, setCustomValue] = useState("");
  const [useCustom, setUseCustom] = useState(false);

  // When estimate changes, reset custom if user hasn't touched it
  useEffect(() => {
    if (!useCustom && estimatedGasWithBuffer !== null) {
      setCustomValue(estimatedGasWithBuffer.toString());
    }
  }, [estimatedGasWithBuffer, useCustom]);

  // Notify parent of override changes
  useEffect(() => {
    if (!useCustom || !customValue) {
      onGasOverride(null);
      return;
    }
    const parsed = parseInt(customValue, 10);
    if (!isNaN(parsed) && parsed >= MIN_GAS && parsed <= MAX_GAS) {
      onGasOverride("0x" + parsed.toString(16));
    } else {
      onGasOverride(null);
    }
  }, [useCustom, customValue, onGasOverride]);

  const handleInputChange = useCallback((e: React.ChangeEvent<HTMLInputElement>) => {
    const val = e.target.value.replace(/[^0-9]/g, "");
    setCustomValue(val);
    setUseCustom(true);
  }, []);

  const handleReset = useCallback(() => {
    setUseCustom(false);
    if (estimatedGasWithBuffer !== null) {
      setCustomValue(estimatedGasWithBuffer.toString());
    }
    onGasOverride(null);
  }, [estimatedGasWithBuffer, onGasOverride]);

  // Validation
  const parsed = parseInt(customValue, 10);
  const isValid = !customValue || (!isNaN(parsed) && parsed >= MIN_GAS && parsed <= MAX_GAS);
  const isBelowEstimate =
    useCustom &&
    !isNaN(parsed) &&
    estimatedGas !== null &&
    parsed < estimatedGas * LOW_GAS_THRESHOLD;
  const isAboveMax = !isNaN(parsed) && parsed > MAX_GAS;
  const isBelowMin = !isNaN(parsed) && parsed > 0 && parsed < MIN_GAS;

  return (
    <div className={styles.container}>
      <button
        className={styles.toggle}
        onClick={() => setExpanded(!expanded)}
        type="button"
        disabled={disabled}
      >
        <svg
          className={`${styles.chevron} ${expanded ? styles.chevronOpen : ""}`}
          width="10"
          height="10"
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth="2.5"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <polyline points="9 18 15 12 9 6" />
        </svg>
        <span>Advanced Gas Settings</span>
        {useCustom && (
          <span className={styles.customBadge}>Custom</span>
        )}
      </button>

      {expanded && (
        <div className={styles.panel}>
          {/* Estimation status */}
          <div className={styles.estimateRow}>
            <span className={styles.estimateLabel}>Estimated gas</span>
            <span className={styles.estimateValue}>
              {estimating ? (
                <span className={styles.estimatingText}>
                  <span className={styles.spinner} />
                  Estimating...
                </span>
              ) : estimatedGas !== null ? (
                <>
                  {estimatedGas.toLocaleString()}
                  {estimationMethod && (
                    <span className={styles.methodTag}>
                      {estimationMethod}
                    </span>
                  )}
                </>
              ) : (
                <span className={styles.dimText}>—</span>
              )}
            </span>
          </div>

          {estimatedGasWithBuffer !== null && !estimating && (
            <div className={styles.estimateRow}>
              <span className={styles.estimateLabel}>With 1.3x buffer</span>
              <span className={styles.estimateValue}>
                {estimatedGasWithBuffer.toLocaleString()}
              </span>
            </div>
          )}

          {/* Custom gas input */}
          <div className={styles.inputSection}>
            <div className={styles.inputLabel}>
              Gas limit
              {useCustom && (
                <button
                  className={styles.resetBtn}
                  onClick={handleReset}
                  type="button"
                >
                  Reset to estimate
                </button>
              )}
            </div>
            <input
              type="text"
              className={`${styles.input} ${!isValid ? styles.inputError : ""} ${useCustom ? styles.inputCustom : ""}`}
              value={customValue}
              onChange={handleInputChange}
              placeholder={estimatedGasWithBuffer?.toLocaleString() || "Enter gas limit"}
              disabled={disabled}
            />

            {/* Validation messages */}
            {isBelowMin && (
              <div className={styles.validationError}>
                Minimum gas limit is {MIN_GAS.toLocaleString()}
              </div>
            )}
            {isAboveMax && (
              <div className={styles.validationError}>
                Maximum gas limit is {MAX_GAS.toLocaleString()} (block gas limit)
              </div>
            )}
            {isBelowEstimate && !isBelowMin && !isAboveMax && (
              <div className={styles.validationWarning}>
                Below estimated gas ({estimatedGas!.toLocaleString()}) — transaction may fail
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
