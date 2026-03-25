import { ExplorerLink } from "./ExplorerLink";

interface Props {
  hash: string;
  chain?: "l1" | "l2";
  short?: boolean;
  className?: string;
}

/** Convenience wrapper for transaction hash links */
export function TxLink({ hash, chain = "l2", short = true, className }: Props) {
  return <ExplorerLink value={hash} type="tx" chain={chain} short={short} className={className} />;
}
