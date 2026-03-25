import { useState } from "react";
import { config } from "../config";
import { lookupAddress } from "../lib/addressBook";
import elStyles from "./ExplorerLink.module.css";

interface Props {
  value: string;
  type?: "tx" | "address" | "block";
  chain?: "l1" | "l2";
  short?: boolean;
  className?: string;
  label?: string;
}

/**
 * Renders a hash/address as a clickable explorer link (if explorer configured)
 * or a copy-to-clipboard span (fallback).
 * Auto-resolves known addresses to human-readable names via the address book.
 */
export function ExplorerLink({ value, type = "address", chain = "l2", short = true, className, label }: Props) {
  const [copied, setCopied] = useState(false);
  const explorer = chain === "l1" ? config.l1Explorer : config.l2Explorer;

  // Resolve display text: explicit label > address book > truncated/full hex
  const knownName = !label && type === "address" ? lookupAddress(value) : undefined;
  const display = label
    ? label
    : knownName
      ? knownName
      : short
        ? `${value.slice(0, 10)}...${value.slice(-6)}`
        : value;

  const handleCopy = () => {
    navigator.clipboard.writeText(value);
    setCopied(true);
    setTimeout(() => setCopied(false), 1200);
  };

  // Title shows full address when using a label/known name
  const titleAddr = (knownName || label) ? ` (${value})` : "";

  if (explorer) {
    const path = type === "tx" ? "tx" : type === "block" ? "block" : "address";
    const url = `${explorer.replace(/\/$/, "")}/${path}/${value}`;
    return (
      <a
        href={url}
        target="_blank"
        rel="noopener noreferrer"
        className={`${elStyles.link} ${className || ""}`}
        title={copied ? "Copied!" : `Open in explorer (${chain.toUpperCase()})${titleAddr} \u00b7 Click to copy`}
        onClick={(e) => {
          if (e.ctrlKey || e.metaKey) return;
          handleCopy();
        }}
      >
        {display}
        {" \u2197"}
      </a>
    );
  }

  return (
    <span
      className={`${elStyles.copyable} ${className || ""}`}
      onClick={handleCopy}
      title={copied ? "Copied!" : `Click to copy${titleAddr}`}
    >
      {display}
      {copied && <span className={elStyles.copiedCheck}>{"\u2713"}</span>}
    </span>
  );
}
