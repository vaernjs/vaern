import type { ChildProcess, SpawnOptions } from "node:child_process";

/** Package version, e.g. "0.0.4". */
export declare const version: string;

/** Name of the platform package that carries the binary for this host, e.g. "@vaernjs/vaern-linux-x64". */
export declare const platformPackage: string;

/** Absolute path to the native vaern binary, or null when this release has none. */
export declare function binaryPath(): string | null;

/** True when a native binary is available for the current platform. */
export declare function hasNativeBinary(): boolean;

/**
 * Spawn the vaern binary. Throws if no native binary is present for this platform.
 */
export declare function vaern(args?: string[], options?: SpawnOptions): ChildProcess;
