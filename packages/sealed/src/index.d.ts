export const MAGIC: Uint8Array;
export const MAX_RECIPIENTS: 16;
export const HEADER_LEN: number;
export const CHUNK_PLAINTEXT: number;
export const TAG_LEN: number;
export const PART_BYTES: number;
export const FILE_CHUNK_BYTES: number;
export const MIN_PADDED: number;
export const IDENTITY_PREFIX: "c0ps1.";
export const DEFAULT_BASE_URL: string;
export const DEFAULT_GATEWAY: string;
export const PIECE_BYTES: number;

export type Source = Blob | Uint8Array | ArrayBuffer;

export interface ClientOptions {
  /** OAuth 2.1 access token with the storage:write scope, or a c0k_ API key. */
  token: string;
  /** c0mpute site, default https://c0mpute.com. */
  baseUrl?: string;
  signal?: AbortSignal;
  fetch?: typeof fetch;
}

export interface Identity {
  linked: true;
  /** `c0ps1.<base64url X25519 public key>` */
  identity: string;
  /** The raw 32-byte X25519 public key, base64url. */
  x25519: string;
  fingerprint: string | null;
  linked_at: string;
  format: "c0mpute-private-v1";
  ingest_url: string;
}

export interface Progress {
  phase: "hashing" | "uploading" | "publishing" | "done";
  done: number;
  total: number;
  object: number;
  objects: number;
}

export interface UploadOptions extends ClientOptions {
  /** Stored encrypted inside the object (never sent in the clear). Defaults to File.name. */
  fileName?: string;
  /** Stored encrypted inside the object. Defaults to File.type. */
  contentType?: string;
  onProgress?: (p: Progress) => void;
}

export interface UploadResult {
  /** `c0p1:<site id>`: the owner reads it with `c0mpute storage get <ref>`. */
  ref: string;
  siteId: string;
  /** The object's data key (base64url). Anyone holding it can read: never store or send it. */
  key: string;
  /** https://gateway.c0mpute.com/p/<id>/#k=<key>: opens in a browser. Same secrecy as `key`. */
  link: string;
  uploadId: string;
  status: "publishing" | "replicating" | "replicated";
  copies: number;
  /** Ciphertext bytes sent. */
  bytes: number;
}

export interface ObjectStatus {
  ref: string;
  upload_id: string | null;
  app: string | null;
  status: "publishing" | "replicating" | "replicated";
  copies: number;
  target: number;
  objects: { site_id: string; published: boolean; copies: number; target?: number }[];
}

export class C0mputeStorageError extends Error {
  status: number;
  code: string;
}

export function identity(opts: ClientOptions): Promise<Identity>;
export function linkStatus(opts: ClientOptions): Promise<Identity | { linked: false }>;
export function uploadSealed(file: Source, opts: UploadOptions): Promise<UploadResult>;
export function status(ref: string, opts: ClientOptions): Promise<ObjectStatus>;
export function link(ref: string, key: string, opts?: { gateway?: string }): string;

export interface SiteRecord {
  v: 1;
  site_id: string;
  name: string;
  owner: string;
  manifest: string;
  seq: number;
  size: number;
  domains: [];
  sig: string;
}

export interface PlannedObject {
  kind: "file" | "part" | "multipart";
  size: number;
  siteId: string;
  name: string;
  blobSize: number;
}

export interface Plan {
  ref: string;
  siteId: string;
  key: string;
  objects: PlannedObject[];
  totalBytes: number;
  contentBytes: number;
}

/** Low level: plan the sealed objects for a file, for a custom transport. */
export function planSealed(input: Source, opts: { identity: string; fileName?: string; contentType?: string; now?: number }): Promise<Plan>;
/** Low level: seal one planned object into `write`, returning its signed hosting record. */
export function streamObject(
  p: PlannedObject,
  write: (piece: Uint8Array, offset: number) => Promise<void> | void,
  opts?: { onRead?: (n: number) => void; onWrite?: (n: number) => void; pieceBytes?: number },
): Promise<SiteRecord>;
/** Open a sealed object with its data key (verifies everything). */
export function openSealed(blob: Uint8Array, key: string | Uint8Array, expectedSiteId?: string): Promise<{ meta: Record<string, unknown>; content: Uint8Array }>;
export function fingerprint(identity: string): string;
export function parseIdentity(identity: string): Uint8Array;
export function padme(len: number): number;
export function siteIdFor(ownerHex: string, name: string): string;
export function b64url(bytes: Uint8Array): string;
export function fromB64url(s: string): Uint8Array;
export function wrapCustody(seed: Uint8Array, recipientKey: Uint8Array): Promise<string>;
export function unwrapCustody(custody: string, ownerHex: string, x25519Secret: Uint8Array): Promise<Uint8Array>;
