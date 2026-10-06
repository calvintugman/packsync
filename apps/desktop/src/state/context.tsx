import { createContext, useCallback, useContext, useEffect, useRef, useState, useSyncExternalStore } from "react";
import type { ReactNode } from "react";
import { ApiError } from "../api/errors";
import type { AppState, AppStore } from "./store";

const Ctx = createContext<AppStore | null>(null);

export function StoreProvider({ store, children }: { store: AppStore; children: ReactNode }) {
  useEffect(() => {
    void store.start();
    return () => store.stop();
  }, [store]);
  return <Ctx.Provider value={store}>{children}</Ctx.Provider>;
}

export function useStore(): AppStore {
  const s = useContext(Ctx);
  if (!s) throw new Error("useStore outside StoreProvider");
  return s;
}

/** Read a slice of the state. The selector must return values that are stable between unrelated changes. */
export function useAppState<T>(select: (s: AppState) => T): T {
  const store = useStore();
  return useSyncExternalStore(store.subscribe, () => select(store.getState()));
}

export interface ActionState<A extends unknown[], R> {
  run: (...args: A) => Promise<R | undefined>;
  busy: boolean;
  error: ApiError | null;
  reset: () => void;
}

/**
 * Wrap an async handler with busy/error state. Errors are kept (not thrown) so the component can show
 * them next to the button that caused them; a newer run replaces an older error.
 */
export function useAction<A extends unknown[], R>(fn: (...args: A) => Promise<R>): ActionState<A, R> {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<ApiError | null>(null);
  const fnRef = useRef(fn);
  const alive = useRef(true);
  useEffect(() => {
    fnRef.current = fn;
  });
  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
    };
  }, []);
  const run = useCallback(async (...args: A): Promise<R | undefined> => {
    setBusy(true);
    setError(null);
    try {
      return await fnRef.current(...args);
    } catch (e) {
      const err = ApiError.from(e);
      if (alive.current && err.code !== "cancelled") setError(err);
      return undefined;
    } finally {
      if (alive.current) setBusy(false);
    }
  }, []);
  const reset = useCallback(() => setError(null), []);
  return { run, busy, error, reset };
}
