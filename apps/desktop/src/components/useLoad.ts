import { useCallback, useEffect, useRef, useState } from "react";
import { ApiError } from "../api/errors";

export interface Loaded<T> {
  /** The latest result; kept while a reload runs so the screen does not flicker. */
  data: T | undefined;
  error: ApiError | null;
  loading: boolean;
  reload: () => void;
}

/**
 * Load something when the screen opens and again whenever `key` changes (for example the instance's
 * revision, so a finished sync refreshes the history without any polling in the component).
 */
export function useLoad<T>(fn: () => Promise<T>, key: unknown): Loaded<T> {
  const [tick, setTick] = useState(0);
  const [res, setRes] = useState<{ token: string; data?: T; error?: ApiError } | null>(null);
  const latest = useRef(fn);
  useEffect(() => {
    latest.current = fn;
  });
  const token = `${String(key)}#${tick}`;
  useEffect(() => {
    let live = true;
    latest.current().then(
      (data) => live && setRes({ token, data }),
      (e: unknown) => live && setRes({ token, error: ApiError.from(e) }),
    );
    return () => {
      live = false;
    };
  }, [token]);
  const reload = useCallback(() => setTick((t) => t + 1), []);
  const current = res?.token === token;
  return {
    data: res?.data,
    error: current ? (res?.error ?? null) : null,
    loading: !current,
    reload,
  };
}
