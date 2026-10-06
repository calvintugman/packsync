import { describe, expect, it } from "vitest";
import { ApiError, describeError, isTransient } from "./errors";

describe("ApiError.from", () => {
  it("takes the engine's error view", () => {
    const e = ApiError.from({ code: "conflict", message: "two versions", advice: "pick one", exit_code: 12 });
    expect(e).toBeInstanceOf(ApiError);
    expect(e).toMatchObject({ code: "conflict", message: "two versions", advice: "pick one", exitCode: 12 });
  });
  it("copes with strings, Errors and junk", () => {
    expect(ApiError.from("boom")).toMatchObject({ code: "internal", message: "boom" });
    expect(ApiError.from(new Error("bang"))).toMatchObject({ code: "internal", message: "bang" });
    expect(ApiError.from(42).message).toBe("Something went wrong.");
    expect(ApiError.from(null).code).toBe("internal");
  });
  it("keeps an existing ApiError as it is", () => {
    const a = new ApiError({ code: "offline", message: "x" });
    expect(ApiError.from(a)).toBe(a);
  });
  it("round-trips through its view", () => {
    const a = new ApiError({ code: "no_space", message: "disk full", advice: "free space", exit_code: 17 });
    expect(ApiError.from(a.view())).toMatchObject({ code: "no_space", advice: "free space", exitCode: 17 });
  });
});

describe("describeError", () => {
  it("gives each important code a title a person understands", () => {
    expect(describeError({ code: "offline", message: "m" }).title).toBe("Can't reach the cloud");
    expect(describeError({ code: "quota_exceeded", message: "m" }).title).toBe("Google Drive is full");
    expect(describeError({ code: "no_space", message: "m" }).title).toBe("Not enough disk space");
    expect(describeError({ code: "weird_new_code", message: "m" }).title).toBe("Something went wrong");
  });
  it("offers the next step where there is one", () => {
    expect(describeError({ code: "auth_required", message: "m" }).action).toBe("sign_in");
    expect(describeError({ code: "auth_revoked", message: "m" }).action).toBe("sign_in");
    expect(describeError({ code: "conflict", message: "m" }).action).toBe("open_conflict");
    expect(describeError({ code: "crash_pending", message: "m" }).action).toBe("open_crash");
    expect(describeError({ code: "io", message: "m" }).action).toBeUndefined();
  });
  it("tells transient problems from real ones", () => {
    expect(isTransient({ code: "offline", message: "" })).toBe(true);
    expect(isTransient({ code: "rate_limited", message: "" })).toBe(true);
    expect(isTransient({ code: "corrupt_remote", message: "" })).toBe(false);
  });
});
