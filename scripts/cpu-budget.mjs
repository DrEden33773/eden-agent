// Divide file-level work without changing Cargo's jobserver or runtime quotas.
import { availableParallelism } from "node:os";

export function cpuBudget(env = process.env) {
  const value = env.EDEN_CPU_BUDGET;
  if (value !== undefined && (!/^[1-9][0-9]*$/.test(value) || !Number.isSafeInteger(Number(value))))
    throw new Error("EDEN_CPU_BUDGET must be a positive integer");
  return value === undefined ? availableParallelism() : Number(value);
}
