import { useState, useEffect, useCallback } from "react";
import { encodeFunctionData, decodeFunctionResult } from "viem";
import { rpcCall } from "../rpc";
import type { AbiFunction } from "../hooks/useBlockscoutAbi";
import styles from "./AbiMethodSelector.module.css";

interface Props {
  abi: AbiFunction[];
  targetAddress: string;
  onCalldataChange: (calldata: string | null) => void;
  onValueChange?: (value: string) => void;
  l2Rpc: string;
}

type Mode = "write" | "read";

function isWriteFunction(fn: AbiFunction): boolean {
  return fn.stateMutability !== "view" && fn.stateMutability !== "pure";
}

function formatResult(value: unknown): string {
  if (typeof value === "bigint") return value.toLocaleString();
  if (typeof value === "boolean") return value ? "true" : "false";
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.map(formatResult).join(", ");
  return String(value);
}

function toViemAbi(fn: AbiFunction) {
  return {
    type: "function" as const,
    name: fn.name,
    inputs: fn.inputs.map((inp) => ({ name: inp.name, type: inp.type })),
    outputs: (fn.outputs || []).map((o) => ({ name: o.name, type: o.type })),
    stateMutability: fn.stateMutability as "nonpayable" | "payable" | "view" | "pure",
  };
}

/** Parse a string parameter value into the expected ABI type */
function parseParam(type: string, raw: string): unknown {
  if (!raw && type.startsWith("uint")) return BigInt(0);
  if (!raw && type.startsWith("int")) return BigInt(0);
  if (!raw && type === "bool") return false;
  if (!raw && type === "address") return "0x0000000000000000000000000000000000000000";
  if (!raw && type === "bytes") return "0x";
  if (!raw && type.startsWith("bytes")) return "0x" + "00".repeat(parseInt(type.slice(5)) || 1);
  if (!raw) return raw;

  if (type.startsWith("uint") || type.startsWith("int")) return BigInt(raw);
  if (type === "bool") return raw === "true" || raw === "1";
  if (type === "address") return raw;
  if (type.endsWith("[]")) {
    try { return JSON.parse(raw); } catch { return raw.split(",").map((s) => s.trim()); }
  }
  return raw;
}

export function AbiMethodSelector({ abi, targetAddress, onCalldataChange, onValueChange, l2Rpc }: Props) {
  const [mode, setMode] = useState<Mode>("write");
  const [selectedFn, setSelectedFn] = useState("");
  const [paramValues, setParamValues] = useState<Record<string, string>>({});
  const [encodeError, setEncodeError] = useState<string | null>(null);
  const [ethValue, setEthValue] = useState("");
  const [readResult, setReadResult] = useState<string | null>(null);
  const [readLoading, setReadLoading] = useState(false);

  const writeFns = abi.filter(isWriteFunction);
  const readFns = abi.filter((fn) => !isWriteFunction(fn));
  const currentList = mode === "write" ? writeFns : readFns;
  const currentFn = currentList.find((f) => f.name === selectedFn);

  // Auto-select first function when list changes
  useEffect(() => {
    const list = mode === "write" ? writeFns : readFns;
    if (list.length > 0 && !list.find((f) => f.name === selectedFn)) {
      setSelectedFn(list[0]!.name);
      setParamValues({});
      setEncodeError(null);
      setReadResult(null);
      setEthValue("");
    }
  }, [mode, abi]); // eslint-disable-line react-hooks/exhaustive-deps

  // Encode calldata when params change (write mode only)
  useEffect(() => {
    if (mode !== "write" || !currentFn) {
      if (mode === "write") onCalldataChange(null);
      return;
    }

    try {
      const args = currentFn.inputs.map((input, i) => {
        const raw = paramValues[`${currentFn.name}_${i}`] || "";
        return parseParam(input.type, raw);
      });

      const data = encodeFunctionData({
        abi: [toViemAbi(currentFn)],
        functionName: currentFn.name,
        args,
      });

      setEncodeError(null);
      onCalldataChange(data);
    } catch (e) {
      setEncodeError((e as Error).message);
      onCalldataChange(null);
    }
  }, [selectedFn, paramValues, mode, currentFn]); // eslint-disable-line react-hooks/exhaustive-deps

  // Notify parent of value changes for payable functions
  useEffect(() => {
    if (onValueChange) {
      onValueChange(
        currentFn?.stateMutability === "payable" && ethValue ? ethValue : ""
      );
    }
  }, [ethValue, currentFn, onValueChange]);

  const handleRead = useCallback(async () => {
    if (!currentFn || !targetAddress) return;
    setReadLoading(true);
    setReadResult(null);
    try {
      const args = currentFn.inputs.map((input, i) => {
        const raw = paramValues[`${currentFn.name}_${i}`] || "";
        return parseParam(input.type, raw);
      });

      const abiItem = toViemAbi(currentFn);

      const data = encodeFunctionData({
        abi: [abiItem],
        functionName: currentFn.name,
        args,
      });

      const result = await rpcCall(l2Rpc, "eth_call", [
        { to: targetAddress, data },
        "latest",
      ]) as string;

      if (currentFn.outputs && currentFn.outputs.length > 0) {
        try {
          const decoded = decodeFunctionResult({
            abi: [abiItem],
            functionName: currentFn.name,
            data: result as `0x${string}`,
          });
          setReadResult(formatResult(decoded));
        } catch {
          setReadResult(result);
        }
      } else {
        setReadResult(result || "(empty)");
      }
    } catch (e) {
      setReadResult(`Error: ${(e as Error).message}`);
    } finally {
      setReadLoading(false);
    }
  }, [currentFn, paramValues, l2Rpc, targetAddress]);

  return (
    <div className={styles.container}>
      <div className={styles.tabs}>
        <button
          className={`${styles.tab} ${mode === "write" ? styles.tabActive : ""}`}
          onClick={() => { setMode("write"); setReadResult(null); }}
        >
          Write ({writeFns.length})
        </button>
        <button
          className={`${styles.tab} ${mode === "read" ? styles.tabActive : ""}`}
          onClick={() => { setMode("read"); onCalldataChange(null); }}
        >
          Read ({readFns.length})
        </button>
      </div>

      {currentList.length === 0 ? (
        <div className={styles.noFunctions}>No {mode} functions found</div>
      ) : (
        <>
          <select
            className={styles.select}
            value={selectedFn}
            onChange={(e) => {
              setSelectedFn(e.target.value);
              setParamValues({});
              setEncodeError(null);
              setReadResult(null);
              setEthValue("");
            }}
          >
            {currentList.map((fn) => (
              <option key={fn.name} value={fn.name}>
                {fn.name}({fn.inputs.map((i) => i.type).join(", ")})
                {fn.stateMutability === "payable" ? " payable" : ""}
              </option>
            ))}
          </select>

          {currentFn && currentFn.inputs.length > 0 && (
            <div className={styles.params}>
              {currentFn.inputs.map((input, i) => (
                <div key={`${currentFn.name}_${i}`} className={styles.paramRow}>
                  <label className={styles.paramLabel}>
                    {input.name || `arg${i}`} ({input.type})
                  </label>
                  <input
                    className={styles.paramInput}
                    value={paramValues[`${currentFn.name}_${i}`] || ""}
                    onChange={(e) =>
                      setParamValues((prev) => ({
                        ...prev,
                        [`${currentFn.name}_${i}`]: e.target.value,
                      }))
                    }
                    placeholder={input.type}
                  />
                </div>
              ))}
            </div>
          )}

          {currentFn?.stateMutability === "payable" && (
            <div className={styles.valueRow}>
              <label className={styles.valueLabel}>ETH Value</label>
              <input
                className={styles.valueInput}
                value={ethValue}
                onChange={(e) => setEthValue(e.target.value)}
                placeholder="0.0"
              />
            </div>
          )}

          {encodeError && mode === "write" && (
            <div className={styles.encodeError}>{encodeError}</div>
          )}

          {mode === "read" && (
            <>
              <button
                className={styles.callBtn}
                onClick={handleRead}
                disabled={readLoading}
              >
                {readLoading ? "Calling..." : "Call"}
              </button>
              {readResult !== null && (
                <div className={styles.readResult}>
                  <div className={styles.readResultLabel}>Result</div>
                  <div className={styles.readResultValue}>{readResult}</div>
                </div>
              )}
            </>
          )}
        </>
      )}
    </div>
  );
}
