// Fee arithmetic for node earnings, free of server-only imports so
// unit tests and the operator report can use it.
import { OPERATOR_SHARE_PCT } from "./market";

/** What the operator keeps from what buyers paid: the share after the house fee. */
export function operatorEarnings(paidUsd: number): number {
  return (paidUsd * OPERATOR_SHARE_PCT) / 100;
}
